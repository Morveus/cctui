//! Gateway account failover — when the bound account runs out of allocation,
//! move the session to another account it is allowed to run on instead of
//! letting it die against a window that resets hours later.
//!
//! Two mechanisms, deliberately distinct, and neither one implicit:
//!
//!   * **the session's pool** — its user declared a set of interchangeable
//!     accounts and armed `failover` on it. The election happens *inside that
//!     set and nowhere else*, by the pool's own strategy. This is the durable
//!     policy: "these accounts are the same to me".
//!   * **an explicit redirect rule** — `CCTUI_GATEWAY_FAILOVER=1` plus an
//!     unexpired `account_redirects` row for the exhausted account, the same
//!     rule that moves launches ([`super::mint`]). This is the incident knob:
//!     dated, one-off, "A is spent, send it to B today".
//!
//! What is gone, and stays gone, is the third thing that used to sit between
//! them: an implicit election over *every* account the user could reach. That
//! is how personal sessions silently ended up on work credentials — nothing had
//! ever said those accounts were interchangeable. A session with no pool and no
//! rule stays put and sees the honest refusal.
//!
//! Every move is recorded in `session_account_rebinds`, so a session that
//! changed accounts can say so afterwards. Discovering a rebind weeks later in
//! a bill was the real complaint; the movement itself was only the symptom.
//!
//! Deliberately NOT an in-gateway replay: request bodies stream through
//! unbuffered on the hot path, so the refused request cannot be re-sent by the
//! server. Instead the gateway repoints the session's token row (the same
//! statement the explicit switch-account endpoint uses — the token string the
//! worker holds never changes) and answers 429 `Retry-After: 1`. Every
//! supported harness retries a 429, and the retry resolves to the new account.
//!
//! Two callers in `passthrough`:
//!
//!   * the soft-limit gate, instead of refusing with the account's own reset
//!     horizon;
//!   * an upstream 429, which is otherwise mirrored verbatim and strands the
//!     session until its window resets.
//!
//! A per-session cooldown keeps a burst-429 (RPM, not quota) from ping-ponging
//! a session between accounts.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::state::AppState;
use crate::store::account_pools::AccountPool;
use crate::store::account_redirects::AccountRedirect;

/// Minimum spacing between two failovers of the same session. A quota 429
/// happens once per exhausted window; anything more frequent is a burst limit
/// the account will recover from on its own, where hopping accounts only
/// churns bindings (and prompt caches) for nothing.
const FAILOVER_COOLDOWN: Duration = Duration::from_mins(1);

/// Sessions that failed over recently, keyed by session id. Process-wide like
/// the orphan-spam config: the map guards wall-clock spacing, which no test
/// state isolation needs (tests inject their own map).
static RECENT_FAILOVERS: LazyLock<dashmap::DashMap<String, Instant>> =
    LazyLock::new(dashmap::DashMap::new);

/// Opt-in: `CCTUI_GATEWAY_FAILOVER=1|true|on|yes`. Unset or anything else
/// leaves failover off.
fn failover_enabled() -> bool {
    static ENABLED: LazyLock<bool> =
        LazyLock::new(|| std::env::var("CCTUI_GATEWAY_FAILOVER").is_ok_and(|v| flag_enables(&v)));
    *ENABLED
}

/// Whether an env-flag value spells "on". Unset/anything else means off.
pub fn flag_enables(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

/// True when `session_id` failed over within `cooldown` of `now`.
pub fn cooldown_active(
    map: &dashmap::DashMap<String, Instant>,
    session_id: &str,
    now: Instant,
    cooldown: Duration,
) -> bool {
    map.get(session_id).is_some_and(|at| now.duration_since(*at) < cooldown)
}

/// Stamp `session_id` as having just failed over.
pub fn note_failover(map: &dashmap::DashMap<String, Instant>, session_id: &str, now: Instant) {
    map.insert(session_id.to_owned(), now);
}

/// Why a session was moved. Recorded verbatim on the audit row and used to
/// word the retry the worker sees, so "my pool balanced this" is never
/// confused with "a rule I wrote last Tuesday moved this".
pub const REASON_POOL: &str = "pool";
pub const REASON_REDIRECT: &str = "redirect";

/// The credential a failing session should rebind to.
pub struct FailoverTarget {
    pub session_id: String,
    pub provider_id: Uuid,
    pub account_name: String,
    /// The account being left, for the audit row.
    pub from_account_name: String,
    /// The pool that authorised the move, when one did.
    pub pool_id: Option<Uuid>,
    /// [`REASON_POOL`] or [`REASON_REDIRECT`].
    pub reason: &'static str,
}

/// The account an explicit rule sends `from_account` to for `model`. A rule
/// matching the exact model beats a catch-all (`match_model` NULL); with no
/// model known (the soft-limit gate) only a catch-all applies. Rules that flip
/// the model rather than the account never move a session.
pub fn explicit_target(
    rules: &[AccountRedirect],
    from_account: Uuid,
    family: &str,
    model: Option<&str>,
) -> Option<Uuid> {
    let candidates = || {
        rules.iter().filter(|r| {
            r.from_account == from_account && r.family == family && r.to_account.is_some()
        })
    };
    model
        .and_then(|m| candidates().find(|r| r.match_model.as_deref() == Some(m)))
        .or_else(|| candidates().find(|r| r.match_model.is_none()))
        .and_then(|r| r.to_account)
}

/// The credential the session behind `session_token` may move to, or `None`
/// when nothing authorises a move — the caller then mirrors / refuses as
/// before.
///
/// Order of authority: the session's own pool first (the standing policy),
/// then an explicit redirect rule (the dated override). Never an implicit
/// election over everything the user can reach.
pub async fn pick_failover_target(
    state: &AppState,
    session_token: &str,
    exclude_provider: Uuid,
    model: Option<&str>,
) -> Option<FailoverTarget> {
    let hash = crate::auth::sha256_hex(session_token);
    let bound: Option<(String, Uuid, Uuid, String, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT t.session_id, ap.user_id, ap.account_id, ap.family, a.name, t.pool_id \
         FROM session_tokens t \
         JOIN account_providers ap ON ap.id = t.account_id \
         JOIN accounts a ON a.id = ap.account_id \
         WHERE t.token_hash = $1 AND t.revoked_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let (session_id, user_id, from_account, family, from_account_name, pool_id) = bound?;
    if cooldown_active(&RECENT_FAILOVERS, &session_id, Instant::now(), FAILOVER_COOLDOWN) {
        return None;
    }

    // 1. The pool the session was launched into, when its owner armed failover.
    if let Some(pool_id) = pool_id
        && let Ok(Some(pool)) = crate::store::account_pools::get(&state.pool, pool_id, None).await
        && in_pool_failover_armed(Some(&pool))
        && let Some(target) = pick_within_pool(
            state,
            &pool,
            user_id,
            &family,
            exclude_provider,
            model,
            &session_id,
            &from_account_name,
        )
        .await
    {
        return Some(target);
    }

    // 2. The explicit, opt-in redirect path — unchanged, and still gated on the
    // env flag so an operator who wants no mid-session movement at all keeps
    // getting none from this direction.
    if !failover_enabled() {
        return None;
    }
    let rules = crate::store::account_redirects::live_for_account(
        &state.pool,
        user_id,
        from_account,
        &family,
    )
    .await
    .ok()?;
    let to_account = explicit_target(&rules, from_account, &family, model)?;

    let (provider_id, account_name): (Uuid, String) = sqlx::query_as(
        "SELECT ap.id, a.name \
         FROM account_providers ap JOIN accounts a ON a.id = ap.account_id \
         WHERE ap.account_id = $1 AND ap.family = $2 AND ap.id != $3",
    )
    .bind(to_account)
    .bind(&family)
    .bind(exclude_provider)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()?;
    Some(FailoverTarget {
        session_id,
        provider_id,
        account_name,
        from_account_name,
        pool_id: None,
        reason: REASON_REDIRECT,
    })
}

/// Elect a member of `pool` for a session whose bound credential just refused.
///
/// The pool's own strategy decides (`headroom` ranks, `ordered` walks the
/// ladder), over the members that are still usable *and* still measurable as
/// having room. Unlike a launch, a member with no readable usage is not
/// elected here: at launch an unreadable account is a degraded guess with
/// nothing at stake, whereas here the current account is already refusing and
/// moving to another unknown would burn the cooldown for nothing.
///
/// The excluded credential is the one that just failed — never a candidate for
/// its own replacement.
#[allow(clippy::too_many_arguments)]
async fn pick_within_pool(
    state: &AppState,
    pool: &AccountPool,
    user_id: Uuid,
    family: &str,
    exclude_provider: Uuid,
    model: Option<&str>,
    session_id: &str,
    from_account_name: &str,
) -> Option<FailoverTarget> {
    let members =
        crate::store::account_pools::usable_members(&state.pool, pool.id, user_id, family)
            .await
            .ok()?;
    let members: Vec<_> =
        members.into_iter().filter(|m| m.provider_id != exclude_provider).collect();
    if members.is_empty() {
        return None;
    }

    let usages = futures_util::future::join_all(
        members.iter().map(|m| super::usage_for_soft_limit(state, m.provider_id)),
    )
    .await;
    // A 429 burst moves many sessions off one account at once; counting the
    // ones already moved keeps them from all landing on the same sibling.
    let providers: Vec<Uuid> = members.iter().map(|m| m.provider_id).collect();
    let in_flight = crate::account_resolve::in_flight_by_provider(state, &providers).await;
    let candidates: Vec<crate::account_pick::Candidate> = members
        .iter()
        .zip(usages.iter())
        .map(|(m, usage)| crate::account_pick::Candidate {
            name: m.name.clone(),
            windows: usage
                .as_ref()
                .map(crate::soft_limit::normalize_usage_windows)
                .unwrap_or_default(),
            limits: crate::soft_limit::SoftLimits::from_json(m.soft_limits_json.as_ref()),
            usage_known: usage.is_some(),
            in_flight: in_flight.get(&m.provider_id).copied().unwrap_or(0),
        })
        .collect();

    elect_replacement(
        pool,
        &candidates,
        &providers,
        model,
        chrono::Utc::now(),
        session_id,
        from_account_name,
    )
}

/// Whether a session's stamped pool actually authorises an in-pool move. A
/// session whose token carries no `pool_id` — every dispatched session before
/// the dispatch path started stamping one — can never reach this path.
fn in_pool_failover_armed(pool: Option<&AccountPool>) -> bool {
    pool.is_some_and(|p| p.failover)
}

/// The election itself, over `candidates` paired positionally with
/// `providers`. Split from the DB/usage fetch so the rule that decides where a
/// refused session lands is testable without a gateway.
#[allow(clippy::too_many_arguments)]
fn elect_replacement(
    pool: &AccountPool,
    candidates: &[crate::account_pick::Candidate],
    providers: &[Uuid],
    model: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
    session_id: &str,
    from_account_name: &str,
) -> Option<FailoverTarget> {
    let pick = if pool.strategy == crate::store::account_pools::STRATEGY_ORDERED {
        crate::account_pick::pick_in_order(candidates, model, now)
    } else {
        crate::account_pick::pick_account(candidates, model, now)
    };
    let crate::account_pick::Pick::Chosen { name, .. } = pick else { return None };
    // Measured room only: see the note above on why an unreadable member is
    // not a failover target even though it is a launch candidate.
    let idx = candidates.iter().position(|c| c.name == name && c.usage_known)?;
    Some(FailoverTarget {
        session_id: session_id.to_owned(),
        provider_id: *providers.get(idx)?,
        account_name: name,
        from_account_name: from_account_name.to_owned(),
        pool_id: Some(pool.id),
        reason: REASON_POOL,
    })
}

/// Repoint the session's live token row from `from_provider` to the elected
/// target — the switch-account statement, minus the HTTP layer. Returns whether
/// the retry can be expected to land on a different account: a concurrent
/// request may have rebound first (0 rows), which is just as good.
pub async fn rebind_session(
    state: &AppState,
    target: &FailoverTarget,
    from_provider: Uuid,
) -> bool {
    let updated = sqlx::query(
        "UPDATE session_tokens SET account_id = $3 \
         WHERE session_id = $1 AND revoked_at IS NULL AND account_id = $2",
    )
    .bind(&target.session_id)
    .bind(from_provider)
    .bind(target.provider_id)
    .execute(&state.pool)
    .await;
    let rebound = match updated {
        Ok(res) => res.rows_affected() > 0,
        Err(e) => {
            tracing::warn!(session_id = %target.session_id, error = %e, "failover rebind failed");
            return false;
        }
    };
    if rebound {
        note_failover(&RECENT_FAILOVERS, &target.session_id, Instant::now());
        // The token string is unchanged — clear any orphan-spam block on its
        // fingerprint, and dismiss the per-chat soft-limit banner.
        super::clear_orphan_block_for_session(state, &target.session_id).await;
        super::clear_soft_limit_block(state, &target.session_id).await;
        // The audit row is the whole reason a user can trust this feature:
        // the session says, afterwards, that it moved and why. Best-effort —
        // the move already happened, and losing the record must not turn a
        // successful failover into a failed request.
        if let Err(e) = crate::store::account_pools::record_rebind(
            &state.pool,
            &target.session_id,
            target.pool_id,
            &target.from_account_name,
            &target.account_name,
            target.reason,
        )
        .await
        {
            tracing::warn!(session_id = %target.session_id, error = %e,
                "could not record the session rebind");
        }
        tracing::warn!(
            session_id = %target.session_id,
            from = %from_provider,
            to = %target.provider_id,
            account = %target.account_name,
            reason = %target.reason,
            "gateway failover: rebound session"
        );
    }
    // 0 rows = a concurrent failover won the race; the retry still lands on
    // the fresh binding, so the caller should answer retry-shortly either way.
    true
}

/// The response that sends the worker back around: 429 with an immediate
/// `Retry-After`, in the provider's native error envelope so the CLI renders
/// the message. The harness's own 429 backoff performs the "replay".
pub fn failover_retry_response(
    account_name: &str,
    reason: &str,
    is_anthropic: bool,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Name the mechanism: the difference between "my pool did its job" and
    // "a rule I forgot about moved this" is the difference between a feature
    // and a surprise.
    let because = if reason == REASON_POOL {
        "the next account in its pool"
    } else {
        "its configured redirect target"
    };
    let message = format!(
        "cctui gateway: the bound account is out of allocation — session moved to \
         account '{account_name}' ({because}). Retry now; the request will be served \
         by that account."
    );
    let body = if is_anthropic {
        serde_json::json!({
            "type": "error",
            "error": { "type": "rate_limit_error", "message": message },
        })
    } else {
        serde_json::json!({
            "error": { "message": message, "type": "rate_limit_error" },
        })
    };
    axum::response::Response::builder()
        .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
        .header(http::header::RETRY_AFTER, "1")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("x-cctui-failover", account_name)
        .header("x-cctui-failover-reason", reason)
        .body(axum::body::Body::from(body.to_string()))
        .unwrap_or_else(|_| axum::http::StatusCode::TOO_MANY_REQUESTS.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rule(
        from: Uuid,
        to: Option<Uuid>,
        family: &str,
        matches: Option<&str>,
        to_model: Option<&str>,
    ) -> AccountRedirect {
        AccountRedirect {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            from_account: from,
            to_account: to,
            family: family.to_owned(),
            match_model: matches.map(str::to_owned),
            to_model: to_model.map(str::to_owned),
            expires_at: None,
            reason: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn flag_only_enables_on_explicit_on_values() {
        for on in ["1", "true", "on", "yes", " True ", "ON"] {
            assert!(flag_enables(on), "{on:?} must enable");
        }
        for off in ["0", "false", "off", "no", "", "anything"] {
            assert!(!flag_enables(off), "{off:?} must not enable");
        }
    }

    #[test]
    fn disabled_by_default_even_with_a_sibling() {
        // The env is unset in tests: the process-wide gate must read as off.
        assert!(!failover_enabled(), "failover must be opt-in");
    }

    #[test]
    fn no_rule_means_no_target() {
        let a = Uuid::new_v4();
        assert_eq!(explicit_target(&[], a, "anthropic", Some("opus")), None);
        assert_eq!(explicit_target(&[], a, "anthropic", None), None);
    }

    #[test]
    fn explicit_rule_wins_over_any_richer_sibling() {
        let (a, x, y) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        // Y is a sibling with plenty of headroom but no rule points at it: it
        // is never a candidate. Only the configured target X is.
        let rules = [rule(a, Some(x), "anthropic", None, None)];
        assert_eq!(explicit_target(&rules, a, "anthropic", Some("opus")), Some(x));
        assert_eq!(explicit_target(&rules, a, "anthropic", None), Some(x));
        assert_ne!(explicit_target(&rules, a, "anthropic", None), Some(y));
    }

    #[test]
    fn exact_model_rule_beats_catch_all() {
        let (a, x, z) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let rules = [
            rule(a, Some(x), "anthropic", None, None),
            rule(a, Some(z), "anthropic", Some("fable"), None),
        ];
        assert_eq!(explicit_target(&rules, a, "anthropic", Some("fable")), Some(z));
        assert_eq!(explicit_target(&rules, a, "anthropic", Some("opus")), Some(x));
    }

    #[test]
    fn unknown_model_only_matches_a_catch_all() {
        let (a, z) = (Uuid::new_v4(), Uuid::new_v4());
        let rules = [rule(a, Some(z), "anthropic", Some("fable"), None)];
        assert_eq!(explicit_target(&rules, a, "anthropic", None), None);
        assert_eq!(explicit_target(&rules, a, "anthropic", Some("opus")), None);
    }

    #[test]
    fn family_and_source_are_scoped() {
        let (a, x) = (Uuid::new_v4(), Uuid::new_v4());
        let rules = [rule(a, Some(x), "anthropic", None, None)];
        assert_eq!(explicit_target(&rules, a, "openai", None), None);
        assert_eq!(explicit_target(&rules, Uuid::new_v4(), "anthropic", None), None);
    }

    #[test]
    fn model_flip_rules_never_move_a_session() {
        let a = Uuid::new_v4();
        let rules = [rule(a, None, "anthropic", None, Some("sonnet"))];
        assert_eq!(explicit_target(&rules, a, "anthropic", Some("opus")), None);
    }

    #[test]
    fn cooldown_spaces_rebinds_and_expires() {
        let map = dashmap::DashMap::new();
        let t0 = Instant::now();
        assert!(!cooldown_active(&map, "s1", t0, Duration::from_mins(1)));
        note_failover(&map, "s1", t0);
        assert!(cooldown_active(&map, "s1", t0 + Duration::from_secs(59), Duration::from_mins(1)));
        assert!(!cooldown_active(&map, "s1", t0 + Duration::from_secs(61), Duration::from_mins(1)));
        assert!(!cooldown_active(&map, "s2", t0, Duration::from_mins(1)));
    }

    fn pool(strategy: &str, failover: bool) -> AccountPool {
        AccountPool {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            name: "work".into(),
            strategy: strategy.to_owned(),
            failover,
            created_at: Utc::now(),
        }
    }

    fn member(name: &str, five_hour_pct: f64, usage_known: bool) -> crate::account_pick::Candidate {
        crate::account_pick::Candidate {
            name: name.to_owned(),
            windows: if usage_known {
                vec![crate::soft_limit::UsageWindow {
                    key: crate::soft_limit::KEY_SESSION.to_owned(),
                    kind: "session".to_owned(),
                    label: "5h".to_owned(),
                    utilization: five_hour_pct,
                    amount_usd: None,
                    resets_at: Some(Utc::now() + chrono::Duration::hours(3)),
                    model_id: None,
                    model_display_name: None,
                }]
            } else {
                Vec::new()
            },
            limits: crate::soft_limit::SoftLimits::default(),
            usage_known,
            in_flight: 0,
        }
    }

    #[test]
    fn an_unstamped_session_is_not_eligible_for_an_in_pool_move() {
        // The dispatch path used to leave `session_tokens.pool_id` null, which
        // is what made `pool.failover` inert for every dispatched session.
        assert!(!in_pool_failover_armed(None));
        assert!(!in_pool_failover_armed(Some(&pool(
            crate::store::account_pools::STRATEGY_HEADROOM,
            false
        ))));
        assert!(in_pool_failover_armed(Some(&pool(
            crate::store::account_pools::STRATEGY_HEADROOM,
            true
        ))));
    }

    #[test]
    fn a_refused_session_moves_to_the_member_with_room_and_records_the_pool() {
        let p = pool(crate::store::account_pools::STRATEGY_HEADROOM, true);
        let spare = Uuid::new_v4();
        let candidates = [member("alpha", 100.0, true), member("beta", 9.0, true)];
        let providers = [Uuid::new_v4(), spare];
        let target =
            elect_replacement(&p, &candidates, &providers, None, Utc::now(), "sess-1", "alpha")
                .expect("a member with room");
        assert_eq!(target.account_name, "beta");
        assert_eq!(target.provider_id, spare);
        assert_eq!(target.from_account_name, "alpha");
        // `rebind_session` writes exactly these two into `session_account_rebinds`,
        // so the move is attributable to the pool afterwards.
        assert_eq!(target.pool_id, Some(p.id));
        assert_eq!(target.reason, REASON_POOL);
    }

    #[test]
    fn an_unmeasurable_member_is_not_a_failover_target() {
        let p = pool(crate::store::account_pools::STRATEGY_HEADROOM, true);
        let candidates = [member("beta", 0.0, false)];
        let providers = [Uuid::new_v4()];
        assert!(
            elect_replacement(&p, &candidates, &providers, None, Utc::now(), "sess-1", "alpha")
                .is_none()
        );
    }

    #[test]
    fn every_remaining_member_out_leaves_the_session_put() {
        let p = pool(crate::store::account_pools::STRATEGY_HEADROOM, true);
        let candidates = [member("alpha", 100.0, true), member("beta", 100.0, true)];
        let providers = [Uuid::new_v4(), Uuid::new_v4()];
        assert!(
            elect_replacement(&p, &candidates, &providers, None, Utc::now(), "sess-1", "alpha")
                .is_none()
        );
    }
}
