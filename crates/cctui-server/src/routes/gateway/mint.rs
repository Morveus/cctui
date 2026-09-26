use super::{Family, clear_orphan_fingerprint, session_token_ttl};

use chrono::Utc;
use uuid::Uuid;

use cctui_proto::ids::{SessionId, SpawnKey};

use crate::state::AppState;

/// Why [`mint_session_env`] could not mint, kept apart so callers can 404 with
/// a message naming the actual gap: "no such account" and "account
/// exists but carries no provider for this harness family" need different
/// remedies (fix the name vs. connect a provider).
#[derive(Debug)]
pub enum MintSessionEnvError {
    /// The account name resolved to nothing for this user (neither owned nor
    /// shared).
    NoAccount,
    /// The account identity exists but has no provider row in the requested
    /// family.
    NoProviderForFamily(Family),
    /// The database failed.
    Db(sqlx::Error),
}

/// One provider row of a resolved account identity: the credential-level
/// `account_providers` row the gateway binds session tokens to. At most one
/// per family per account.
pub struct ProviderRow {
    pub id: Uuid,
    pub provider: String,
    /// Per-provider logical→concrete model alias map.
    pub model_aliases: Option<serde_json::Value>,
    /// Account-owned model catalog (`[{model, label, pricing…}]`). The only
    /// source of concrete model ids for a `fireworks` provider — nothing in a
    /// worker image, entrypoint, or dispatch payload hardcodes one.
    pub models: Option<serde_json::Value>,
}

impl ProviderRow {
    pub fn family(&self) -> Family {
        Family::from_provider(&self.provider)
    }
}

/// Resolve an account identity by name for a user — either one they OWN or one
/// SHARED to them (`account_shares`), preferring their own on a name
/// clash — and return its provider rows. `Ok(None)` means no such
/// account; `Ok(Some(rows))` may be empty for an identity with no connected
/// providers.
pub async fn account_provider_rows(
    state: &AppState,
    user_id: Uuid,
    account_name: &str,
) -> Result<Option<Vec<ProviderRow>>, sqlx::Error> {
    let account_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT a.id FROM accounts a \
         WHERE a.name = $2 \
           AND (a.user_id = $1 OR EXISTS ( \
               SELECT 1 FROM resource_shares s \
                WHERE s.resource_type = 'account' AND s.resource_id = a.id \
                  AND s.grantee_id = $1 AND s.revoked_at IS NULL)) \
         ORDER BY (a.user_id = $1) DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(account_name)
    .fetch_optional(&state.pool)
    .await?;
    let Some(account_id) = account_id else { return Ok(None) };
    let rows: Vec<(Uuid, String, Option<serde_json::Value>, Option<serde_json::Value>)> =
        sqlx::query_as(
            "SELECT id, provider, model_aliases, models FROM account_providers \
             WHERE account_id = $1 ORDER BY provider",
        )
        .bind(account_id)
        .fetch_all(&state.pool)
        .await?;
    let mut rows: Vec<ProviderRow> = rows
        .into_iter()
        .map(|(id, provider, model_aliases, models)| ProviderRow {
            id,
            provider,
            model_aliases,
            models,
        })
        .collect();
    apply_account_redirects(&state.pool, user_id, account_id, &mut rows).await;
    Ok(Some(rows))
}

/// Substitute each provider row whose family carries a live account redirect
/// with the target account's same-family row. Per-family: a rule on the
/// anthropic binding must not move an openai one. Any failure — no rule, an
/// inaccessible target, a target without the family, a DB error — leaves the
/// original row in place: a broken rule must never fail a launch.
async fn apply_account_redirects(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    account_id: Uuid,
    rows: &mut [ProviderRow],
) {
    let rules = match crate::store::account_redirects::live_for_launch(pool, user_id).await {
        Ok(rules) if !rules.is_empty() => rules,
        Ok(_) => return,
        Err(e) => {
            tracing::error!(%user_id, "loading account redirects (launching unredirected): {e}");
            return;
        }
    };
    for row in rows {
        let family = row.family().label();
        let Some(target) =
            crate::store::account_redirects::follow_account_chain(&rules, account_id, family)
        else {
            continue;
        };
        // The target must be usable by this user (owned or shared — the same
        // predicate the name lookup applies) and carry the family: a redirect
        // must never widen access.
        let found: Option<(Uuid, String, Option<serde_json::Value>, Option<serde_json::Value>)> =
            sqlx::query_as(
                "SELECT p.id, p.provider, p.model_aliases, p.models \
                 FROM account_providers p JOIN accounts a ON a.id = p.account_id \
                 WHERE p.account_id = $2 AND p.family = $3 \
                   AND (a.user_id = $1 OR EXISTS ( \
                       SELECT 1 FROM resource_shares s \
                        WHERE s.resource_type = 'account' AND s.resource_id = a.id \
                          AND s.grantee_id = $1 AND s.revoked_at IS NULL)) \
                 LIMIT 1",
            )
            .bind(user_id)
            .bind(target)
            .bind(family)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        if let Some((id, provider, model_aliases, models)) = found {
            tracing::info!(
                %user_id,
                from = %account_id,
                to = %target,
                family,
                "account redirect applied at bind time"
            );
            *row = ProviderRow { id, provider, model_aliases, models };
        } else {
            tracing::warn!(
                %user_id,
                from = %account_id,
                to = %target,
                family,
                "account redirect target unusable (inaccessible or no provider) — ignoring rule"
            );
        }
    }
}

/// Resolve a named account for a user and mint a session-scoped gateway token
/// bound to `(session_id, provider row)`, returning the env vars to inject into
/// the worker so its agent traffic flows through this gateway under that
/// account. The raw credentials never leave the server —
/// only the opaque session token does. Resolution is by `(account identity,
/// harness family)`: the account carries at most one provider per
/// family, and `family` — derived from the adapter via
/// [`Family::from_adapter`] — picks that row.
pub async fn mint_session_env(
    state: &AppState,
    user_id: Uuid,
    account_name: &str,
    family: Family,
    session_id: &str,
) -> Result<std::collections::BTreeMap<String, String>, MintSessionEnvError> {
    let rows = account_provider_rows(state, user_id, account_name)
        .await
        .map_err(MintSessionEnvError::Db)?;
    let Some(rows) = rows else {
        // Fail-diagnosable, not silent: the account name resolved to
        // nothing for this user — neither owned nor shared. This is the exact
        // shape of the "404 no account named X" dispatch failures, so name the
        // user + account here rather than leaving a caller to dig through the DB.
        tracing::warn!(
            %user_id,
            account = %account_name,
            "mint_session_env: no account resolved (not owned by, nor shared to, this user)"
        );
        return Err(MintSessionEnvError::NoAccount);
    };
    let Some(row) = rows.iter().find(|r| r.family() == family) else {
        tracing::warn!(
            %user_id,
            account = %account_name,
            family = family.label(),
            "mint_session_env: account has no provider for this family"
        );
        return Err(MintSessionEnvError::NoProviderForFamily(family));
    };
    mint_env_for_account(state, row.id, &row.provider, session_id)
        .await
        .map_err(MintSessionEnvError::Db)
}

/// Mint gateway env for EVERY provider family the account carries, not just the
/// adapter's own. `primary` is the family the launch actually needs: a missing
/// provider there is an error exactly as in [`mint_session_env`], while a
/// secondary family that fails to mint is logged and skipped.
///
/// The families emit disjoint routing keys, so the worker ends up holding its
/// harness credential *and* the others — a claude session can then drive `codex`
/// (or spawn a codex child through `CctuiAgent`) under the same account.
/// `primary` mints last so its per-account custom env wins any key collision.
pub async fn mint_session_env_all_families(
    state: &AppState,
    user_id: Uuid,
    account_name: &str,
    primary: Family,
    session_id: &str,
) -> Result<std::collections::BTreeMap<String, String>, MintSessionEnvError> {
    let rows = account_provider_rows(state, user_id, account_name)
        .await
        .map_err(MintSessionEnvError::Db)?;
    let Some(rows) = rows else {
        tracing::warn!(
            %user_id,
            account = %account_name,
            "mint_session_env_all_families: no account resolved (not owned by, nor shared to, this user)"
        );
        return Err(MintSessionEnvError::NoAccount);
    };
    let order = mint_order(&rows, primary).inspect_err(|_| {
        tracing::warn!(
            %user_id,
            account = %account_name,
            family = primary.label(),
            "mint_session_env_all_families: account has no provider for this family"
        );
    })?;
    let (primary_row, secondaries) = order.split_last().expect("mint_order yields the primary");
    let mut env = std::collections::BTreeMap::new();
    for row in secondaries {
        match mint_env_for_account(state, row.id, &row.provider, session_id).await {
            Ok(e) => env.extend(e),
            Err(err) => tracing::warn!(
                %session_id,
                account = %account_name,
                family = row.family().label(),
                "secondary-family mint failed, launching without it: {err}"
            ),
        }
    }
    env.extend(
        mint_env_for_account(state, primary_row.id, &primary_row.provider, session_id)
            .await
            .map_err(MintSessionEnvError::Db)?,
    );
    Ok(env)
}

/// The account's provider rows to mint, secondaries first and `primary` LAST so
/// its per-account custom env wins a key collision. Errors when the account
/// carries no row for `primary` — the family the launch actually needs.
fn mint_order(
    rows: &[ProviderRow],
    primary: Family,
) -> Result<Vec<&ProviderRow>, MintSessionEnvError> {
    if !rows.iter().any(|r| r.family() == primary) {
        return Err(MintSessionEnvError::NoProviderForFamily(primary));
    }
    let mut order: Vec<&ProviderRow> = rows.iter().filter(|r| r.family() != primary).collect();
    order.extend(rows.iter().filter(|r| r.family() == primary));
    Ok(order)
}

/// Re-mint a gateway session token + env for an **already-resolved** provider
/// row. `provider_id` is the `account_providers.id` the
/// session token binds to — NOT the identity-level `accounts.id`. Used on the
/// resume path, where the session already has a bound provider row (persisted
/// via `session_tokens` / `sessions.account_id`) and we just need to re-issue a
/// fresh token + env for the revived worker rather than re-resolving by name,
/// and on the dispatch path after `(account, family)` resolution.
pub async fn mint_session_env_for_account(
    state: &AppState,
    provider_id: Uuid,
    session_id: &str,
) -> Result<Option<std::collections::BTreeMap<String, String>>, sqlx::Error> {
    let provider: Option<String> =
        crate::store::account_providers::provider_by_id(&state.pool, provider_id).await?;
    let Some(provider) = provider else { return Ok(None) };
    Ok(Some(mint_env_for_account(state, provider_id, &provider, session_id).await?))
}

/// Resolve a session's bound OAuth account and re-mint its gateway env, ready
/// to hand to the daemon on any wake path (explicit resume *or* reply-driven
/// cold-resume) so a revived worker routes through the gateway with a fresh
/// valid token instead of launching with empty env and 401ing.
///
/// The binding is durable on `sessions.account_id`; falls back to the
/// most-recent non-revoked `session_tokens` row for sessions bound before that
/// column was populated. Returns empty env (never errors) for sessions with no
/// account binding — callers attach it unconditionally.
pub async fn resume_env_for_session(
    state: &AppState,
    session_id: &str,
) -> std::collections::BTreeMap<String, String> {
    // Re-mint EVERY bound family and merge. The two families emit disjoint env
    // keys (`ANTHROPIC_*` vs `OPENAI_*`), so a worker carrying both claude +
    // codex creds gets both restored — not just the last-minted family.
    let mut env = std::collections::BTreeMap::new();
    for aid in resolve_session_accounts(state, session_id).await {
        match mint_session_env_for_account(state, aid, session_id).await {
            Ok(Some(e)) => env.extend(e),
            Ok(None) => {} // this family's account row is gone; others may still mint
            Err(e) => {
                tracing::error!(%session_id, "re-mint gateway env on wake failed: {e}");
            }
        }
    }
    env
}

/// Resolve a session's bound OAuth accounts — **one per provider family**.
/// A session can carry a claude (Anthropic) account *and* a codex
/// (`OpenAI`) account at once; both must be re-minted on wake or the worker
/// launches missing one family's creds and 401s. The durable binding lives on
/// the session's live `session_tokens`
/// rows (one stable token per family, see `mint_env_for_account`); we take the
/// newest live token per family. Falls back to the legacy single
/// `sessions.account_id` column for sessions bound before per-family tokens
/// existed. Empty when the session has no account binding at all.
pub async fn resolve_session_accounts(state: &AppState, session_id: &str) -> Vec<Uuid> {
    // DISTINCT ON the family keeps the newest token per family, preferring live
    // over revoked. Revoked rows COUNT as a binding:
    // session end revokes every token (`revoke_session_tokens`), so a resume
    // after a real — or spurious — end would otherwise find no binding and
    // relaunch the worker with EMPTY gateway env (silently off-gateway on a
    // desktop, a 401 loop on k8s). The revoked row still names the account;
    // `mint_env_for_account` then mints a FRESH live token — the dead token
    // itself is never resurrected.
    let mut ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT ON (oa.family) st.account_id \
         FROM session_tokens st JOIN account_providers oa ON oa.id = st.account_id \
         WHERE st.session_id = $1 \
         ORDER BY oa.family, (st.revoked_at IS NULL) DESC, st.created_at DESC",
    )
    .bind(session_id)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    if ids.is_empty()
        && let Some(aid) = sqlx::query_scalar::<_, Uuid>(
            "SELECT account_id::uuid FROM sessions WHERE id = $1 AND account_id IS NOT NULL",
        )
        .bind(session_id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
    {
        ids.push(aid);
    }
    ids
}

/// Mint a fresh opaque session token bound to `(session_id, account_id)`,
/// persist the account on the session row so the binding is durable across id
/// rotation / restart, and return the gateway env for the account's
/// provider family.
pub async fn mint_env_for_account(
    state: &AppState,
    account_id: Uuid,
    provider: &str,
    session_id: &str,
) -> Result<std::collections::BTreeMap<String, String>, sqlx::Error> {
    let family = Family::from_provider(provider);

    // A session gets ONE stable gateway token **per provider family** for its
    // whole life. Reuse the existing live token for THIS
    // family if we have one persisted, rather than minting a fresh row on every
    // resume — re-minting bloated `session_tokens` for no reason and left live
    // workers holding a token the gateway might no longer resolve. The token
    // string is immutable; only its account binding moves (repointed below) on
    // an account switch. Scoping reuse + repoint to the family is what
    // lets a worker carry claude + codex at once: minting the OpenAI account
    // must NOT repoint the Anthropic token to it.
    let key = crate::crypto::vault_key();
    let token = if let Some(existing) =
        existing_session_token(state, session_id, family, &key).await
    {
        // Repoint only THIS family's live token to the requested account (and
        // un-revoke, defensively) so an account switch reuses the same string
        // — the worker's `ANTHROPIC_AUTH_TOKEN`/`OPENAI_API_KEY` never
        // changes, the gateway just resolves it to the new account. The
        // `account_providers` join + family predicate confine the repoint to the
        // same-family token, leaving the other family's token untouched.
        let _ = sqlx::query(
            "UPDATE session_tokens AS st SET account_id = $2, revoked_at = NULL, expires_at = $4 \
                 FROM account_providers AS oa \
                 WHERE st.session_id = $1 AND st.revoked_at IS NULL \
                   AND st.account_id = oa.id \
                   AND oa.family = $3",
        )
        .bind(session_id)
        .bind(account_id)
        .bind(family.label())
        .bind(Utc::now() + session_token_ttl())
        .execute(&state.pool)
        .await;
        // The reused token's fingerprint may have been flagged as a
        // spamming orphan while its binding was broken — the
        // rebind keeps the SAME token string, so clear the block now
        // instead of leaving the just-fixed binding 401ing for the
        // remainder of the (up to 300s) block window.
        clear_orphan_fingerprint(&state.gateway_orphan_spam, &crate::auth::sha256_hex(&existing));
        existing
    } else {
        // First token for this session: mint a fresh opaque token (same
        // shape/entropy as other secrets), store its hash AND its
        // encrypted plaintext so resume can re-supply the same string.
        let token = format!("cctui_s_{}", crate::auth::mint_secret());
        let token_hash = crate::auth::sha256_hex(&token);
        let enc = crate::crypto::encrypt(&token, &key);
        sqlx::query(
            "INSERT INTO session_tokens \
                 (token_hash, session_id, account_id, encrypted_token, expires_at) \
                 VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&token_hash)
        .bind(session_id)
        .bind(account_id)
        .bind(&enc)
        .bind(Utc::now() + session_token_ttl())
        .execute(&state.pool)
        .await?;
        token
    };

    // Persist the account on the session row so it survives id rotation and
    // server restart — the resume path re-mints from here. Best
    // effort: a session row may not exist yet at spawn-time mint (registration
    // races), so a no-op update is fine; the token row is the live binding.
    let _ = sqlx::query("UPDATE sessions SET account_id = $2 WHERE id = $1")
        .bind(session_id)
        .bind(account_id.to_string())
        .execute(&state.pool)
        .await;

    let base = state.config.external_url.trim_end_matches('/');
    let mut env = std::collections::BTreeMap::new();
    // Per-account custom env: decrypt the account's `env_json` blob and
    // merge it in FIRST, so the gateway routing keys inserted by the `match`
    // below always win over any account-supplied key of the same name. Because
    // every worker (re)launch path funnels through here — initial spawn,
    // `resume_env_for_session`, and the daemon's `gateway-env` pull — the account
    // env is re-served on respawn/resume and survives a daemon / claude-daemon
    // restart, not just the initial spawn.
    if let Some(account_env) = account_env_json(state, account_id, &key).await {
        env.extend(account_env);
    }
    if family == Family::Openai
        && let Some(block) = codex_config_block(state, account_id).await
    {
        env.insert(cctui_proto::codex_config::CONFIG_TOML_ENV.to_owned(), block);
    }
    apply_gateway_env(&mut env, family, base, token);
    Ok(env)
}

/// Render this openai account's curated `config.toml` settings for the launch
/// env. Both codex injection paths read the rendered block from there — the
/// daemon turns it into `-c` flags, the k8s worker entrypoint splices it into
/// `~/.codex/config.toml` — so every (re)launch that mints env also re-derives
/// the settings, and neither path re-renders them itself.
async fn codex_config_block(state: &AppState, account_id: Uuid) -> Option<String> {
    let settings: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT settings_json FROM account_providers WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();
    cctui_proto::codex_config::render_block(&settings?)
}

/// Insert the family's gateway routing keys over whatever the account env
/// already carries. The three families emit DISJOINT key pairs, which is what
/// lets one worker hold a claude + a codex + a Fireworks credential at once —
/// `resume_env_for_session` merges every bound family's env blindly.
pub fn apply_gateway_env(
    env: &mut std::collections::BTreeMap<String, String>,
    family: Family,
    base: &str,
    token: String,
) {
    match family {
        Family::Anthropic => {
            env.insert("ANTHROPIC_BASE_URL".into(), format!("{base}/gateway/anthropic"));
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), token);
            apply_anthropic_cache_defaults(env);
        }
        Family::Openai => {
            env.insert("OPENAI_BASE_URL".into(), format!("{base}/gateway/openai"));
            env.insert("OPENAI_API_KEY".into(), token);
        }
        Family::Fireworks => {
            env.insert("FIREWORKS_BASE_URL".into(), format!("{base}/gateway/fireworks"));
            env.insert("FIREWORKS_API_KEY".into(), token);
        }
    }
}

/// Default-on Anthropic 1-hour prompt-cache flag: `or_insert_with`
/// (not a plain insert) so the account's already-merged resolved env can
/// override it — `ENABLE_PROMPT_CACHING_1H=0` opts back out — while the default
/// preserves the prior always-on behaviour. Curated in the settings catalog.
pub fn apply_anthropic_cache_defaults(env: &mut std::collections::BTreeMap<String, String>) {
    env.entry("ENABLE_PROMPT_CACHING_1H".to_string()).or_insert_with(|| "1".to_string());
}

/// The session's existing stable gateway token for a given provider family
/// (decrypted), if one was minted and persisted with its plaintext.
/// `family` selects the row so the families' tokens stay independent. `None` for
/// a family with no live token, or pre-migration rows that only stored the
/// one-way hash (those fall through to a one-time fresh mint). Picks the newest
/// live token on the off chance a session accrued several from the old
/// re-mint-on-resume behaviour.
pub async fn existing_session_token(
    state: &AppState,
    session_id: &str,
    family: Family,
    key: &[u8],
) -> Option<String> {
    let enc: String = sqlx::query_scalar(
        "SELECT st.encrypted_token FROM session_tokens st \
         JOIN account_providers oa ON oa.id = st.account_id \
         WHERE st.session_id = $1 AND st.revoked_at IS NULL \
           AND st.encrypted_token IS NOT NULL \
           AND oa.family = $2 \
         ORDER BY st.created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .bind(family.label())
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()?;
    crate::crypto::decrypt(&enc, key)
}

/// Resolve a logical model name through a named account's alias map.
///
/// Mirrors [`mint_session_env`]'s `(account identity, family)` resolution
/// so spawn maps the *same* provider row the gateway binds the
/// session to, then looks `model` up in that row's `model_aliases` JSON object.
/// Returns the mapped concrete model (e.g. `opus` → `claude-opus-4-8[1m]`) or
/// the input unchanged when there's no account, no family match, no alias map,
/// or no matching key. A DB error degrades gracefully to the unmapped model
/// rather than failing spawn.
///
/// The `fireworks` family additionally resolves through the account's model
/// catalog ([`resolve_catalog_model`]), which is the sole source of its model
/// ids — its harness has no built-in model list to fall back on.
pub async fn resolve_account_model(
    state: &AppState,
    user_id: Uuid,
    account_name: &str,
    family: Family,
    model: &str,
) -> String {
    let Ok(Some(rows)) = account_provider_rows(state, user_id, account_name).await else {
        return model.to_owned();
    };
    let Some(row) = rows.iter().find(|r| r.family() == family) else {
        return model.to_owned();
    };
    // A model redirect flips the *requested* model before alias resolution, so
    // `fable → opus` still maps through the account's `opus` alias. Keyed on
    // the row's owning account: after an account redirect the target's rules
    // apply, not the source's.
    let model = &*flip_model_redirect(state, user_id, row.id, family, model).await;
    let aliased = row
        .model_aliases
        .as_ref()
        .and_then(|v| v.get(model))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| model.to_owned(), str::to_owned);
    if family != Family::Fireworks {
        return aliased;
    }
    resolve_catalog_model(row.models.as_ref(), &aliased).unwrap_or(aliased)
}

/// The model to actually spawn with, after any live model-redirect rule on the
/// provider row's owning account. Fail-soft on every path: no rule, no flip.
async fn flip_model_redirect(
    state: &AppState,
    user_id: Uuid,
    provider_row_id: Uuid,
    family: Family,
    requested: &str,
) -> String {
    let account_id: Option<Uuid> =
        sqlx::query_scalar("SELECT account_id FROM account_providers WHERE id = $1")
            .bind(provider_row_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();
    let Some(account_id) = account_id else { return requested.to_owned() };
    let Ok(rules) = crate::store::account_redirects::live_for_launch(&state.pool, user_id).await
    else {
        return requested.to_owned();
    };
    crate::store::account_redirects::model_flip(&rules, account_id, family.label(), requested)
        .map_or_else(
            || requested.to_owned(),
            |flipped| {
                tracing::info!(
                    %user_id,
                    account = %account_id,
                    family = family.label(),
                    requested,
                    flipped,
                    "model redirect applied at spawn time"
                );
                flipped.to_owned()
            },
        )
}

/// Resolve a model against an account's catalog: an exact `model` id passes
/// through, a catalog `label` maps to its id, and anything else (including an
/// empty request) falls back to the first catalog entry. `None` when the catalog
/// is empty — the caller then keeps the requested model verbatim.
pub fn resolve_catalog_model(models: Option<&serde_json::Value>, model: &str) -> Option<String> {
    let entries = models?.as_array().filter(|a| !a.is_empty())?;
    let id_of = |e: &serde_json::Value| {
        e.get("model").and_then(serde_json::Value::as_str).map(str::to_owned)
    };
    let wanted = model.trim();
    if !wanted.is_empty()
        && let Some(hit) = entries.iter().find(|e| {
            id_of(e).as_deref() == Some(wanted)
                || e.get("label").and_then(serde_json::Value::as_str) == Some(wanted)
        })
    {
        return id_of(hit);
    }
    entries.iter().find_map(id_of)
}

/// Decrypt a named account's per-account custom env (`env_json`).
///
/// `env_json` is stored as an encrypted JSON object of `{VAR: value}` (the
/// values may be secrets — daemon-supplied env like a per-account API key — so
/// the column is encrypted at rest and never serialized back out of the accounts
/// model). Returns the decoded map, or `None` when the account has no custom env
/// / the blob can't be decrypted or parsed (degrade gracefully: a bad blob must
/// never fail the mint and strand the worker with no gateway routing).
pub async fn account_env_json(
    state: &AppState,
    account_id: Uuid,
    key: &[u8],
) -> Option<std::collections::BTreeMap<String, String>> {
    // `env_json` lives on the identity parent (`accounts`); `account_id`
    // here is the provider-row id (session_tokens FK), so join up
    // to the parent to read it.
    let enc: String = sqlx::query_scalar::<_, Option<String>>(
        "SELECT a.env_json FROM accounts a \
         JOIN account_providers ap ON ap.account_id = a.id \
         WHERE ap.id = $1",
    )
    .bind(account_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .flatten()?;
    let json = crate::crypto::decrypt(&enc, key)?;
    serde_json::from_str::<std::collections::BTreeMap<String, String>>(&json).ok()
}

/// The merged per-account `settings_json` for a session's bound account(s)
/// served to the daemon via the gateway-env pull so it is re-derived
/// on every worker (re)launch (spawn/resume/cold-resume/fork) — surviving a
/// daemon / claude-daemon restart the same way gateway env does.
///
/// A session can bind one account per provider family (claude + codex); their
/// `settings_json` blobs are deep-merged (later family wins on a key clash,
/// which in practice never happens — the two harnesses don't share settings
/// keys). Returns `None` when no bound account carries settings.
///
/// The daemon deep-merges this UNDER its own managed hook settings when it
/// writes the worker's `--settings` file, so the managed hooks always win.
/// This function only makes the settings available on the server pull path.
///
/// OPEN QUESTION — MANAGED-only keys (`strictKnownMarketplaces`,
/// `strictPluginOnlyCustomization`, `disableSideloadFlags`,
/// `blockedMarketplaces`): it is NOT verified at runtime whether claude honors
/// these via a plain `--settings` file, or whether they must land in the
/// managed-settings drop-in path (e.g. `/etc/claude-code/managed-settings.json`)
/// to take effect. This is a follow-up to confirm when a real user needs one of
/// these keys. It does NOT block this path: those keys are tagged MANAGED in the
/// settings catalog and rejected by `Catalog::validate_settings`, so no
/// per-account `settings_json` can carry them yet — whatever arrives here is
/// safe/care keys that `--settings` honors.
/// Whether a session has an account of `family` bound, by the same token-then-
/// `sessions.account_id` resolution [`resolve_session_accounts`] uses.
pub async fn session_has_family(state: &AppState, session_id: &str, family: super::Family) -> bool {
    for account_id in resolve_session_accounts(state, session_id).await {
        let label: Option<String> =
            sqlx::query_scalar("SELECT family FROM account_providers WHERE id = $1")
                .bind(account_id)
                .fetch_optional(&state.pool)
                .await
                .ok()
                .flatten();
        if label.as_deref().and_then(super::Family::from_label) == Some(family) {
            return true;
        }
    }
    false
}

pub async fn resolve_session_settings(
    state: &AppState,
    session_id: &str,
) -> Option<serde_json::Value> {
    let mut merged: Option<serde_json::Value> = None;
    for account_id in resolve_session_accounts(state, session_id).await {
        let settings: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT settings_json FROM account_providers WHERE id = $1")
                .bind(account_id)
                .fetch_optional(&state.pool)
                .await
                .ok()
                .flatten();
        if let Some(s) = settings.filter(|v| !v.is_null()) {
            match merged.as_mut() {
                Some(base) => cctui_proto::util::deep_merge(base, s),
                None => merged = Some(s),
            }
        }
    }
    merged
}

/// Revoke every session token bound to a session — called when a
/// session ends so the gateway can no longer be used under that token.
pub async fn revoke_session_tokens(state: &AppState, session_id: &str) {
    if let Err(e) = crate::store::tokens::revoke_by_session(&state.pool, session_id).await {
        tracing::error!(
            %session_id,
            error = %e,
            "failed to revoke session tokens — a live gateway credential may remain usable"
        );
    }
    state.spawn_capabilities.remove(session_id);
    state.session_usd_budgets.remove(session_id);
    if let Err(e) = crate::store::spawn_capabilities::delete(&state.pool, session_id).await {
        tracing::warn!(%session_id, error = %e, "spawn-capability cleanup failed");
    }
}

pub fn needs_rebind(spawn_key: &str, session_id: &str) -> bool {
    !spawn_key.is_empty() && !session_id.is_empty() && spawn_key != session_id
}

/// Re-key a session's gateway token, and anything already metered under it, from
/// the spawn key onto the id the harness registered under. An adapter whose
/// harness names its own session (opencode returns `ses_…`) pulls gateway env
/// under the spawn key, binding the token to an id no later lookup uses. Adapters
/// whose local id IS the spawn key pass equal ids and no-op.
pub async fn rebind_spawn_key(state: &AppState, spawn_key: SpawnKey, session_id: SessionId) {
    let (spawn_key, session_id) = (spawn_key.as_str(), session_id.as_str());
    if !needs_rebind(spawn_key, session_id) {
        return;
    }
    if let Err(e) =
        crate::store::tokens::rebind_session_id(&state.pool, spawn_key, session_id).await
    {
        tracing::error!(
            %spawn_key,
            %session_id,
            error = %e,
            "spawn-key rebind failed — this session's usage will not be metered"
        );
        return;
    }
    // The in-memory maps are keyed the same way the token was, so they have to
    // move too: a budget or capability left under the spawn key is invisible to
    // every later lookup, which goes through the rebound id.
    if let Some((_, budget)) = state.session_usd_budgets.remove(spawn_key) {
        state.session_usd_budgets.insert(session_id.to_owned(), budget);
    }
    if let Some((_, cap)) = state.spawn_capabilities.remove(spawn_key) {
        state.spawn_capabilities.insert(session_id.to_owned(), cap);
    }
    if let Err(e) =
        crate::store::spawn_capabilities::rebind(&state.pool, spawn_key, session_id).await
    {
        tracing::error!(%spawn_key, %session_id, error = %e, "spawn-capability rebind failed");
    }
}

/// Look up the `session_id` bound to a (live) gateway session token — used only
/// to tag Langfuse traces. `None` for unknown/revoked tokens.
pub async fn session_id_for_token(state: &AppState, session_token: &str) -> Option<String> {
    let hash = crate::auth::sha256_hex(session_token);
    crate::store::tokens::session_id_by_token_hash(&state.pool, &hash).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::{Family, MintSessionEnvError, ProviderRow, mint_order};

    fn row(provider: &str) -> ProviderRow {
        ProviderRow {
            id: uuid::Uuid::new_v4(),
            provider: provider.to_owned(),
            model_aliases: None,
            models: None,
        }
    }

    #[test]
    fn every_family_is_minted_with_the_primary_last() {
        let rows = vec![row("anthropic"), row("openai"), row("fireworks")];
        let order = mint_order(&rows, Family::Openai).expect("openai provider present");
        let families: Vec<&str> = order.iter().map(|r| r.family().label()).collect();
        assert_eq!(families, vec!["anthropic", "fireworks", "openai"]);
    }

    #[test]
    fn a_lone_provider_still_mints() {
        let rows = vec![row("anthropic")];
        let order = mint_order(&rows, Family::Anthropic).expect("anthropic provider present");
        assert_eq!(order.len(), 1);
    }

    #[test]
    fn a_missing_primary_family_is_an_error_even_with_other_families() {
        let rows = vec![row("anthropic")];
        assert!(matches!(
            mint_order(&rows, Family::Openai),
            Err(MintSessionEnvError::NoProviderForFamily(Family::Openai))
        ));
        assert!(matches!(
            mint_order(&[], Family::Anthropic),
            Err(MintSessionEnvError::NoProviderForFamily(Family::Anthropic))
        ));
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;

    async fn mk_account(pool: &sqlx::PgPool, user: Uuid, name: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO accounts (user_id, name) VALUES ($1, $2) RETURNING id")
            .bind(user)
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn mk_provider(pool: &sqlx::PgPool, user: Uuid, account: Uuid, provider: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO account_providers (user_id, account_id, provider) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(user)
        .bind(account)
        .bind(provider)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn provider_rows(pool: &sqlx::PgPool, account: Uuid) -> Vec<ProviderRow> {
        let rows: Vec<(Uuid, String, Option<serde_json::Value>, Option<serde_json::Value>)> =
            sqlx::query_as(
                "SELECT id, provider, model_aliases, models FROM account_providers \
                 WHERE account_id = $1 ORDER BY provider",
            )
            .bind(account)
            .fetch_all(pool)
            .await
            .unwrap();
        rows.into_iter()
            .map(|(id, provider, model_aliases, models)| ProviderRow {
                id,
                provider,
                model_aliases,
                models,
            })
            .collect()
    }

    async fn mk_share(pool: &sqlx::PgPool, account: Uuid, grantee: Uuid) {
        sqlx::query(
            "INSERT INTO resource_shares (resource_type, resource_id, grantee_id) \
             VALUES ('account', $1, $2)",
        )
        .bind(account)
        .bind(grantee)
        .execute(pool)
        .await
        .unwrap();
    }

    /// An owner's rule on a shared account must move a grantee's launch; a
    /// rule authored by someone who owns neither the account nor the launch
    /// must not.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn owner_rule_redirects_grantee_launches() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let suffix = Uuid::new_v4();
        let mut users = Vec::new();
        for name in ["owner", "grantee", "stranger"] {
            users.push(sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO users (id, name, key_hash) VALUES (gen_random_uuid(), $1, gen_random_uuid()::text) RETURNING id",
            )
            .bind(format!("{name}-{suffix}"))
            .fetch_one(&pool)
            .await
            .unwrap());
        }
        let (owner, grantee, stranger) = (users[0], users[1], users[2]);

        let alpha = mk_account(&pool, owner, &format!("alpha-{suffix}")).await;
        let beta = mk_account(&pool, owner, &format!("beta-{suffix}")).await;
        let alpha_anthropic = mk_provider(&pool, owner, alpha, "anthropic").await;
        let beta_anthropic = mk_provider(&pool, owner, beta, "anthropic").await;
        mk_share(&pool, alpha, grantee).await;
        mk_share(&pool, beta, grantee).await;

        crate::store::account_redirects::upsert(
            &pool,
            crate::store::account_redirects::NewRedirect {
                user_id: owner,
                from_account: alpha,
                to_account: Some(beta),
                family: "anthropic",
                match_model: None,
                to_model: None,
                expires_at: None,
                reason: None,
            },
        )
        .await
        .unwrap();

        let mut rows = provider_rows(&pool, alpha).await;
        apply_account_redirects(&pool, grantee, alpha, &mut rows).await;
        assert_eq!(
            rows.iter().find(|r| r.family() == Family::Anthropic).unwrap().id,
            beta_anthropic,
            "owner's rule must redirect a grantee's launch"
        );

        sqlx::query("DELETE FROM account_redirects WHERE from_account = $1")
            .bind(alpha)
            .execute(&pool)
            .await
            .unwrap();
        crate::store::account_redirects::upsert(
            &pool,
            crate::store::account_redirects::NewRedirect {
                user_id: stranger,
                from_account: alpha,
                to_account: Some(beta),
                family: "anthropic",
                match_model: None,
                to_model: None,
                expires_at: None,
                reason: None,
            },
        )
        .await
        .unwrap();
        let mut rows = provider_rows(&pool, alpha).await;
        apply_account_redirects(&pool, grantee, alpha, &mut rows).await;
        assert_eq!(
            rows.iter().find(|r| r.family() == Family::Anthropic).unwrap().id,
            alpha_anthropic,
            "a stranger's rule must not move anyone else's launch"
        );

        for u in [owner, grantee, stranger] {
            sqlx::query("DELETE FROM users WHERE id = $1").bind(u).execute(&pool).await.unwrap();
        }
    }

    /// Real-DB proof of the bind-time seam: a live rule substitutes the
    /// target's same-family provider row; other families, expired rules, and
    /// inaccessible targets leave rows untouched.
    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn bind_time_redirect_substitutes_target_provider_row() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let suffix = Uuid::new_v4();
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (id, name, key_hash) VALUES (gen_random_uuid(), $1, gen_random_uuid()::text) RETURNING id",
        )
        .bind(format!("redir-{suffix}"))
        .fetch_one(&pool)
        .await
        .unwrap();
        let stranger: Uuid = sqlx::query_scalar(
            "INSERT INTO users (id, name, key_hash) VALUES (gen_random_uuid(), $1, gen_random_uuid()::text) RETURNING id",
        )
        .bind(format!("stranger-{suffix}"))
        .fetch_one(&pool)
        .await
        .unwrap();

        let alpha = mk_account(&pool, user, &format!("alpha-{suffix}")).await;
        let beta = mk_account(&pool, user, &format!("beta-{suffix}")).await;
        let foreign = mk_account(&pool, stranger, &format!("foreign-{suffix}")).await;
        let alpha_anthropic = mk_provider(&pool, user, alpha, "anthropic").await;
        let alpha_openai = mk_provider(&pool, user, alpha, "openai").await;
        let beta_anthropic = mk_provider(&pool, user, beta, "anthropic").await;
        mk_provider(&pool, stranger, foreign, "openai").await;

        crate::store::account_redirects::upsert(
            &pool,
            crate::store::account_redirects::NewRedirect {
                user_id: user,
                from_account: alpha,
                to_account: Some(beta),
                family: "anthropic",
                match_model: None,
                to_model: None,
                expires_at: None,
                reason: None,
            },
        )
        .await
        .unwrap();

        let mut rows = provider_rows(&pool, alpha).await;
        apply_account_redirects(&pool, user, alpha, &mut rows).await;
        let anthropic = rows.iter().find(|r| r.family() == Family::Anthropic).unwrap();
        let openai = rows.iter().find(|r| r.family() == Family::Openai).unwrap();
        assert_eq!(anthropic.id, beta_anthropic, "anthropic row must become beta's");
        assert_eq!(openai.id, alpha_openai, "openai row must stay on alpha");

        // An inaccessible target (no ownership, no share) never applies.
        crate::store::account_redirects::upsert(
            &pool,
            crate::store::account_redirects::NewRedirect {
                user_id: user,
                from_account: alpha,
                to_account: Some(foreign),
                family: "openai",
                match_model: None,
                to_model: None,
                expires_at: None,
                reason: None,
            },
        )
        .await
        .unwrap();
        let mut rows = provider_rows(&pool, alpha).await;
        apply_account_redirects(&pool, user, alpha, &mut rows).await;
        assert_eq!(
            rows.iter().find(|r| r.family() == Family::Openai).unwrap().id,
            alpha_openai,
            "a redirect must never widen access"
        );

        // An expired rule is dead.
        sqlx::query(
            "UPDATE account_redirects SET expires_at = now() - interval '1 minute' \
                     WHERE from_account = $1",
        )
        .bind(alpha)
        .execute(&pool)
        .await
        .unwrap();
        let mut rows = provider_rows(&pool, alpha).await;
        apply_account_redirects(&pool, user, alpha, &mut rows).await;
        assert_eq!(
            rows.iter().find(|r| r.family() == Family::Anthropic).unwrap().id,
            alpha_anthropic,
            "expired rules must not redirect"
        );

        for u in [user, stranger] {
            sqlx::query("DELETE FROM users WHERE id = $1").bind(u).execute(&pool).await.unwrap();
        }
    }
}
