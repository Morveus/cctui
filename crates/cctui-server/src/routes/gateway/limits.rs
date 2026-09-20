use super::{Account, session_id_for_token};

use uuid::Uuid;

use crate::live_sessions::live_sessions_predicate;
use crate::state::AppState;

/// Merge a `CctuiAgent` child's per-session dollar budget into `cap` as a
/// `session_usd` limit. A budget on the child always wins over an account-level
/// `session_usd`: it is the tighter, purpose-set ceiling.
pub fn merge_session_budget(
    cap: &crate::soft_limit::SoftLimits,
    budget_usd: Option<f64>,
) -> crate::soft_limit::SoftLimits {
    let Some(budget) = budget_usd.filter(|b| b.is_finite() && *b > 0.0) else {
        return cap.clone();
    };
    let mut merged = cap.clone();
    let entry = merged.limits.entry(crate::soft_limit::KEY_SESSION_USD.to_owned()).or_default();
    entry.cap_usd = Some(budget);
    merged
}

/// Durable key for a block, marked [`SESSION_SCOPE_PREFIX`] when the blocking
/// cap came from the child's own budget rather than the account configuration:
/// raising an account cap must not lift it.
pub fn durable_block_key(
    account: &crate::soft_limit::SoftLimits,
    effective: &crate::soft_limit::SoftLimits,
    key: &str,
) -> String {
    let usd = crate::soft_limit::KEY_SESSION_USD;
    let cap_of = |c: &crate::soft_limit::SoftLimits| c.limits.get(usd).and_then(|l| l.cap_usd);
    if key == usd && cap_of(effective) != cap_of(account) {
        return format!("{}{key}", crate::soft_limit::SESSION_SCOPE_PREFIX);
    }
    key.to_owned()
}

/// The account's soft limits with any per-session `CctuiAgent` budget applied.
/// Skips the token→session lookup entirely while no child budget is live.
pub async fn session_budget_limits(
    state: &AppState,
    acct: &Account,
    session_token: &str,
) -> crate::soft_limit::SoftLimits {
    if state.session_usd_budgets.is_empty() {
        return acct.soft_limits.clone();
    }
    let Some(session_id) = session_id_for_token(state, session_token).await else {
        return acct.soft_limits.clone();
    };
    let budget = state.session_usd_budgets.get(&session_id).map(|b| *b);
    merge_session_budget(&acct.soft_limits, budget)
}

/// Resolve a session token to its `(session_id, account_name)` — used by the
/// soft-limit signalling path to tag the per-session WS event with the
/// human account name (the `Account` struct carries no name). `None` for
/// unknown/revoked tokens.
pub async fn session_and_account_name_for_token(
    state: &AppState,
    session_token: &str,
) -> Option<(String, String)> {
    let hash = crate::auth::sha256_hex(session_token);
    sqlx::query_as::<_, (String, String)>(
        "SELECT t.session_id, a.name \
         FROM session_tokens t \
         JOIN account_providers ap ON ap.id = t.account_id \
         JOIN accounts a ON a.id = ap.account_id \
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
}

/// Set the durable block on the session row, reporting whether it changed.
/// `false` means the row already carried this reason — another pod, or this
/// one, has already announced the episode.
async fn mark_block_row(
    pool: &sqlx::PgPool,
    session_id: &str,
    reason: &str,
    key: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(concat!(
        "UPDATE sessions SET soft_limit_reason = $2, soft_limit_key = $3 \
             WHERE id = $1 AND ",
        live_sessions_predicate!(),
        " AND (soft_limit_reason IS DISTINCT FROM $2 \
                   OR soft_limit_key IS DISTINCT FROM $3)"
    ))
    .bind(session_id)
    .bind(reason)
    .bind(key)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Drop the durable block on a session row, reporting whether it was set.
async fn clear_block_row(pool: &sqlx::PgPool, session_id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE sessions SET soft_limit_reason = NULL, soft_limit_key = NULL \
         WHERE id = $1 AND soft_limit_reason IS NOT NULL",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Drop the durable block on whatever session `token_hash` is bound to,
/// returning the session id when one was actually cleared.
async fn clear_block_row_for_token(
    pool: &sqlx::PgPool,
    token_hash: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "UPDATE sessions SET soft_limit_reason = NULL, soft_limit_key = NULL \
         WHERE soft_limit_reason IS NOT NULL AND id = ( \
             SELECT session_id FROM session_tokens \
             WHERE token_hash = $1 AND revoked_at IS NULL) \
         RETURNING id",
    )
    .bind(token_hash)
    .fetch_optional(pool)
    .await
}

/// Record a soft-limit block against a session and broadcast it.
///
/// Idempotent per block episode, across replicas: the UPDATE only touches a row
/// whose reason actually changes, so the first refused passthrough emits
/// [`ServerEvent::SoftLimitReached`] and the worker's repeated Retry-After
/// retries (still blocked) are no-ops on any pod. The webui shows the banner;
/// the matching clear arrives from [`clear_soft_limit_block`] on the next
/// success or an account switch.
pub async fn mark_soft_limit_block(
    state: &AppState,
    session_id: &str,
    account_id: Uuid,
    account_name: &str,
    reason: &str,
    block_key: &str,
    retry_after_secs: i64,
) {
    if session_id.is_empty() {
        return;
    }
    // Persist a durable block on the session row so the classifier drives the
    // session to `Bucket::Blocked` (✋ needs input) and the block survives a
    // resubscribe. The stored reason is an actionable "continue on
    // another account" hint; `list_sessions` reads it. Idempotent (overwrite),
    // and never clobbers the churning daemon `tempo`/`agent_state` signals.
    let needs = format!("switch account: {account_name} rate-limited");
    let changed = match mark_block_row(&state.pool, session_id, &needs, block_key).await {
        Ok(changed) => changed,
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "failed to persist soft-limit block");
            false
        }
    };
    if changed {
        state.bus.publish_server(cctui_proto::ws::ServerEvent::SoftLimitReached {
            session_id: session_id.to_owned(),
            account_id,
            account_name: account_name.to_owned(),
            reason: reason.to_owned(),
            retry_after_secs,
        });
    }
}

/// Clear a session's soft-limit block and broadcast the dismissal.
///
/// The session row is the only source of truth: the UPDATE reports whether it
/// really flipped, so exactly one pod emits [`ServerEvent::SoftLimitCleared`]
/// however many replicas race, and a block set before a rollout still clears.
pub async fn clear_soft_limit_block(state: &AppState, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    match clear_block_row(&state.pool, session_id).await {
        Ok(true) => {
            state.bus.publish_server(cctui_proto::ws::ServerEvent::SoftLimitCleared {
                session_id: session_id.into(),
            });
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(%session_id, error = %e, "failed to clear soft-limit block"),
    }
}

/// Clear whatever session a gateway token is bound to, in one statement.
///
/// The success path runs on every proxied 2xx, so it resolves the token inside
/// the UPDATE rather than paying a separate lookup per request.
pub async fn clear_soft_limit_block_for_token(state: &AppState, session_token: &str) {
    let hash = crate::auth::sha256_hex(session_token);
    match clear_block_row_for_token(&state.pool, &hash).await {
        Ok(Some(session_id)) => {
            state.bus.publish_server(cctui_proto::ws::ServerEvent::SoftLimitCleared { session_id });
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "failed to clear soft-limit block"),
    }
}

/// Record that a session token was just presented at the gateway, so the UI
/// can distinguish an account-bound session whose worker actually routes here
/// from one silently riding ambient creds. Fire-and-forget + self-throttling
/// (skips a write when stamped within the last minute) to stay off the
/// passthrough hot path. `token_fp` is the sha256 hex == `session_tokens.token_hash`.
pub fn note_token_used(state: &AppState, token_fp: &str) {
    let pool = state.pool.clone();
    let hash = token_fp.to_owned();
    tokio::spawn(async move {
        let _ = crate::store::tokens::stamp_last_used(&pool, &hash).await;
    });
}

/// Flag an account as needing reauthentication: the upstream provider
/// rejected its OAuth credentials. Persists `needs_reauth` + the error so the
/// accounts UI can show a "credential rejected — reauthenticate" badge. Gated on
/// the in-memory set so a flapping worker doesn't re-write the row on every 401 —
/// the DB write fires only on the false→true transition.
pub fn flag_account_reauth(state: &AppState, account_id: Uuid, reason: &str) {
    if state.account_reauth.insert(account_id, ()).is_some() {
        return; // already flagged in memory — no redundant write
    }
    let pool = state.pool.clone();
    let reason = reason.to_string();
    tokio::spawn(async move {
        if let Err(e) = sqlx::query(
            "UPDATE account_providers \
                SET needs_reauth = true, last_auth_error = $2, last_auth_error_at = now() \
             WHERE id = $1",
        )
        .bind(account_id)
        .bind(reason)
        .execute(&pool)
        .await
        {
            tracing::warn!(account = %account_id, error = %e, "failed to flag account reauth");
        }
    });
}

/// Clear an account's reauth flag after a successful upstream call.
/// Gated on the in-memory set so the common case (account healthy) costs nothing;
/// the DB write fires only on the true→false transition.
pub fn clear_account_reauth(state: &AppState, account_id: Uuid) {
    if state.account_reauth.remove(&account_id).is_none() {
        return; // not flagged — nothing to clear
    }
    let pool = state.pool.clone();
    tokio::spawn(async move {
        if let Err(e) = sqlx::query(
            "UPDATE account_providers \
                SET needs_reauth = false, last_auth_error = NULL, last_auth_error_at = NULL \
             WHERE id = $1 AND needs_reauth",
        )
        .bind(account_id)
        .execute(&pool)
        .await
        {
            tracing::warn!(account = %account_id, error = %e, "failed to clear account reauth");
        }
    });
}

/// Resolve the session token (the upstream bearer the worker sent) to its
/// account. Returns `None` for unknown/revoked tokens.
/// Env-tunable thresholds for the orphan-token spam guard. Parsed once.
pub struct OrphanSpamCfg {
    /// Unresolved 401s within `window` before a fingerprint is blocked.
    threshold: u32,
    /// Counting window.
    window: std::time::Duration,
    /// How long a flagged fingerprint stays blocked (DB lookups skipped).
    block: std::time::Duration,
}

pub static ORPHAN_SPAM_CFG: std::sync::LazyLock<OrphanSpamCfg> = std::sync::LazyLock::new(|| {
    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }
    OrphanSpamCfg {
        threshold: u32::try_from(env_u64("CCTUI_GATEWAY_SPAM_THRESHOLD", 10)).unwrap_or(10),
        window: std::time::Duration::from_secs(env_u64("CCTUI_GATEWAY_SPAM_WINDOW_SECS", 60)),
        block: std::time::Duration::from_secs(env_u64("CCTUI_GATEWAY_SPAM_BLOCK_SECS", 300)),
    }
});

pub type OrphanSpamMap = dashmap::DashMap<String, crate::state::OrphanSpam>;

/// True if this token fingerprint is currently blocked as a spamming orphan.
/// Pure in-memory check — no DB — so blocked orphans cost ~nothing.
pub fn orphan_is_blocked(state: &AppState, token_fp: &str) -> bool {
    orphan_is_blocked_at(&state.gateway_orphan_spam, token_fp, std::time::Instant::now())
}

pub fn orphan_is_blocked_at(map: &OrphanSpamMap, token_fp: &str, now: std::time::Instant) -> bool {
    let Some(entry) = map.get(token_fp) else { return false };
    matches!(entry.blocked_until, Some(until) if until > now)
}

/// Drop a token fingerprint from the in-memory orphan-spam state.
///
/// Called after a successful rebind/mint that reuses an existing token string:
/// the fingerprint may have been blocked while the binding was broken (an
/// unresolvable token 401s its way past the threshold), and since a rebind
/// repoints the SAME token string, the block would otherwise keep dropping a
/// NOW-VALID token's requests for the remainder of the block window (up to
/// 300s). Clearing re-enables the DB lookup immediately. Idempotent.
pub fn clear_orphan_fingerprint(map: &OrphanSpamMap, token_fp: &str) {
    map.remove(token_fp);
}

/// Clear the orphan-spam block for every live token of `session_id`.
///
/// The explicit account-switch path (`sessions::switch_account`) rebinds token
/// rows by session id without the token plaintext in hand;
/// `session_tokens.token_hash` IS the fingerprint the spam guard keys on (both
/// are the sha256 hex of the token string), so clearing by stored hash needs no
/// token material. Best-effort: a failed lookup just leaves the block to
/// expire on its own.
pub async fn clear_orphan_block_for_session(state: &AppState, session_id: &str) {
    let hashes: Vec<String> =
        crate::store::tokens::token_hashes_by_session(&state.pool, session_id)
            .await
            .unwrap_or_default();
    for hash in &hashes {
        clear_orphan_fingerprint(&state.gateway_orphan_spam, hash);
    }
}

/// Record an unresolvable-token 401 and, once a fingerprint crosses the spam
/// threshold within the window, flag it as a blocked orphan and log LOUDLY.
pub fn note_orphan_401(state: &AppState, token_fp: &str) {
    let cfg = &*ORPHAN_SPAM_CFG;
    let fp_short: String = token_fp.chars().take(12).collect();
    let (count, newly_blocked) = bump_orphan_401(
        &state.gateway_orphan_spam,
        token_fp,
        std::time::Instant::now(),
        cfg.threshold,
        cfg.window,
        cfg.block,
    );

    if newly_blocked {
        tracing::error!(
            stage = "session-token",
            token_fp = %fp_short,
            count,
            block_secs = cfg.block.as_secs(),
            "🔴 GATEWAY ORPHAN SPAM: unresolvable session token exceeded {} 401s in {}s — \
             blocking fingerprint for {}s; subsequent requests dropped before any DB lookup. \
             A zombie worker lost its session→account binding; resume or kill it.",
            cfg.threshold,
            cfg.window.as_secs(),
            cfg.block.as_secs(),
        );
    } else {
        tracing::warn!(
            stage = "session-token",
            token_fp = %fp_short,
            count,
            "gateway 401: session token not resolvable (orphan worker retrying)"
        );
    }
}

/// Pure sliding-window counter. Returns `(count_in_window, newly_blocked)` where
/// `newly_blocked` is true only on the transition that flags the fingerprint.
pub fn bump_orphan_401(
    map: &OrphanSpamMap,
    token_fp: &str,
    now: std::time::Instant,
    threshold: u32,
    window: std::time::Duration,
    block: std::time::Duration,
) -> (u32, bool) {
    let mut entry = map.entry(token_fp.to_string()).or_insert_with(|| crate::state::OrphanSpam {
        count: 0,
        window_start: now,
        blocked_until: None,
    });

    // Roll the window over once it elapses (also clears an expired block).
    if now.duration_since(entry.window_start) > window {
        entry.count = 0;
        entry.window_start = now;
        entry.blocked_until = None;
    }
    entry.count += 1;
    let count = entry.count;

    let newly_blocked = count >= threshold && entry.blocked_until.is_none();
    if newly_blocked {
        entry.blocked_until = Some(now + block);
    }
    drop(entry);
    (count, newly_blocked)
}

#[cfg(test)]
mod tests {
    use super::{clear_block_row, clear_block_row_for_token, durable_block_key, mark_block_row};
    use crate::soft_limit::{KEY_SESSION_USD, SESSION_SCOPE_PREFIX, SoftLimits};

    #[test]
    fn a_child_budget_block_is_marked_session_scoped() {
        let account = SoftLimits::from_json(Some(&serde_json::json!({
            "session_usd": {"cap_usd": 10.0}
        })));
        let from_budget = crate::routes::gateway::merge_session_budget(&account, Some(2.0));
        assert_eq!(
            durable_block_key(&account, &from_budget, KEY_SESSION_USD),
            format!("{SESSION_SCOPE_PREFIX}{KEY_SESSION_USD}"),
            "a tighter child budget owns the block, not the account"
        );
        assert_eq!(
            durable_block_key(&account, &account, KEY_SESSION_USD),
            KEY_SESSION_USD,
            "the account's own session_usd cap stays account-scoped"
        );
        assert_eq!(durable_block_key(&account, &from_budget, "session"), "session");
    }

    /// DB-gated: a block this process never observed — set by the other replica,
    /// or before a rollout — must still clear through both paths, and each write
    /// must report the transition exactly once so only one pod broadcasts.
    #[tokio::test]
    async fn a_block_no_process_remembers_still_clears() {
        let Some(url) =
            crate::routes::gateway::test_db_url("a_block_no_process_remembers_still_clears")
        else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = uuid::Uuid::new_v4();
        let acct = uuid::Uuid::new_v4();
        let prov = uuid::Uuid::new_v4();
        let session_id = format!("soft-limit-{uid}");
        let token_hash = format!("th-{uid}");
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("soft-limit-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
            .bind(acct)
            .bind(uid)
            .bind(format!("soft-limit-acct-{uid}"))
            .execute(&pool)
            .await
            .expect("seed account");
        sqlx::query(
            "INSERT INTO account_providers \
                 (id, user_id, provider, encrypted_refresh_token, account_id) \
             VALUES ($1, $2, 'anthropic', 'x', $3)",
        )
        .bind(prov)
        .bind(uid)
        .bind(acct)
        .execute(&pool)
        .await
        .expect("seed provider");
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, status) \
             VALUES ($1, 'm1', '/w', $2, 'active')",
        )
        .bind(&session_id)
        .bind(uid)
        .execute(&pool)
        .await
        .expect("seed session");
        sqlx::query(
            "INSERT INTO session_tokens (token_hash, session_id, account_id) VALUES ($1, $2, $3)",
        )
        .bind(&token_hash)
        .bind(&session_id)
        .bind(prov)
        .execute(&pool)
        .await
        .expect("seed token");

        let reason = "switch account: personal rate-limited";
        assert!(
            mark_block_row(&pool, &session_id, reason, "session").await.unwrap(),
            "the first mark is the transition"
        );
        assert!(
            !mark_block_row(&pool, &session_id, reason, "session").await.unwrap(),
            "a retry against the same block must not re-announce it"
        );

        assert_eq!(
            clear_block_row_for_token(&pool, &token_hash).await.unwrap().as_deref(),
            Some(session_id.as_str()),
            "a 2xx must clear a block this pod never set"
        );
        assert!(
            clear_block_row_for_token(&pool, &token_hash).await.unwrap().is_none(),
            "the clear must announce once, not on every later success"
        );

        mark_block_row(&pool, &session_id, reason, "session").await.unwrap();
        assert!(
            clear_block_row(&pool, &session_id).await.unwrap(),
            "raising the cap must clear a block this pod never set"
        );
        assert!(!clear_block_row(&pool, &session_id).await.unwrap(), "already clear");

        sqlx::query("DELETE FROM session_tokens WHERE session_id = $1")
            .bind(&session_id)
            .execute(&pool)
            .await
            .expect("cleanup tokens");
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .expect("cleanup sessions");
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .expect("cleanup");
    }
}
