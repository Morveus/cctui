use super::{Account, Family, FireworksSettings, current_access_token, reload_account};

use axum::http::StatusCode;
use chrono::Utc;
use uuid::Uuid;

use crate::state::AppState;

/// Anthropic's free OAuth usage endpoint. Returns subscription window
/// utilization (5h session + 7d weekly) WITHOUT consuming any tokens — it is not
/// an inference call. Undocumented + caveat-accepted (same class as the OAuth
/// token endpoints above); overridable via env to track upstream changes.
pub fn anthropic_usage_url() -> String {
    std::env::var("CCTUI_ANTHROPIC_OAUTH_USAGE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/api/oauth/usage".into())
}

/// `User-Agent` the usage endpoint requires (`claude-code/<version>`). Without a
/// claude-code UA the endpoint drops the caller into an aggressively rate-limited
/// bucket (persistent 429s). Overridable so we can bump the version it expects
/// without a code redeploy.
pub fn anthropic_usage_user_agent() -> String {
    std::env::var("CCTUI_ANTHROPIC_USAGE_USER_AGENT").unwrap_or_else(|_| "claude-code/2.1.0".into())
}

/// OpenAI/codex's real per-account usage endpoint. Returns the `ChatGPT`
/// backend's actual 5h/7d rate-limit windows (`rate_limit.primary_window` /
/// `secondary_window`) — the same numbers `codex /status` shows. Works cookieless
/// with just the account's OAuth Bearer + `chatgpt-account-id` header, both of
/// which we already hold per account. Overridable via env to track upstream moves.
pub fn openai_usage_url() -> String {
    std::env::var("CCTUI_OPENAI_USAGE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/wham/usage".into())
}

/// Codex's "Redeem usage limit reset" credits (`GET`), consumed via
/// `POST {url}/consume {redeem_request_id, credit_id?}`.
pub fn openai_reset_credits_url() -> String {
    std::env::var("CCTUI_OPENAI_RESET_CREDITS_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits".into())
}

/// Normalize a `rate_limit_reset_credits` block (either the one embedded in
/// `wham/usage` or the standalone endpoint's body, snake or camel case) to
/// `{available_count, credits[{id, status, reset_type, granted_at, expires_at,
/// title}]}`. `None` when the body carries no such block.
pub fn map_reset_credits(body: &serde_json::Value) -> Option<serde_json::Value> {
    let block = body
        .get("rate_limit_reset_credits")
        .or_else(|| body.get("rateLimitResetCredits"))
        .unwrap_or(body);
    let credits = block.get("credits")?.as_array()?;
    let pick = |c: &serde_json::Value, snake: &str, camel: &str| {
        c.get(snake).or_else(|| c.get(camel)).cloned().unwrap_or(serde_json::Value::Null)
    };
    let credits: Vec<_> = credits
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": pick(c, "id", "id"),
                "status": pick(c, "status", "status"),
                "reset_type": pick(c, "reset_type", "resetType"),
                "granted_at": pick(c, "granted_at", "grantedAt"),
                "expires_at": pick(c, "expires_at", "expiresAt"),
                "title": pick(c, "title", "title"),
            })
        })
        .collect();
    let available_count = block
        .get("available_count")
        .or_else(|| block.get("availableCount"))
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(|| {
            credits
                .iter()
                .filter(|c| {
                    c.get("status").and_then(serde_json::Value::as_str) == Some("available")
                })
                .count()
                .try_into()
                .unwrap_or(i64::MAX)
        });
    Some(serde_json::json!({ "available_count": available_count, "credits": credits }))
}

/// Map the `ChatGPT` `wham/usage` body to our provider-agnostic `{five_hour,
/// seven_day}` usage shape, plus `reset_credits` when the body embeds them. `primary_window` is the 5h window,
/// `secondary_window` the 7d one; `used_percent` → `utilization`, `reset_at`
/// (unix epoch seconds) → `resets_at` (rfc3339). Returns `None` if the body has
/// no `rate_limit` or is missing either window, so the caller can fall back to the
/// local tally.
pub fn map_wham_usage(body: &serde_json::Value) -> Option<serde_json::Value> {
    let rate_limit = body.get("rate_limit")?;
    let window = |w: &serde_json::Value| {
        let utilization = w.get("used_percent").and_then(serde_json::Value::as_f64);
        let resets_at = w
            .get("reset_at")
            .and_then(serde_json::Value::as_i64)
            .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
            .map(|dt| dt.to_rfc3339());
        // Some plans report a weekly-only primary window; the slot does not imply the duration.
        let window_seconds = w.get("limit_window_seconds").and_then(serde_json::Value::as_i64);
        serde_json::json!({
            "utilization": utilization,
            "resets_at": resets_at,
            "window_seconds": window_seconds
        })
    };
    let five_hour = window(rate_limit.get("primary_window")?);
    let seven_day = window(rate_limit.get("secondary_window")?);
    let mut usage = serde_json::json!({ "five_hour": five_hour, "seven_day": seven_day });
    if let Some(credits) = map_reset_credits(body) {
        usage["reset_credits"] = credits;
    }
    Some(usage)
}

/// Standalone read of the account's reset credits, for a `wham/usage` body
/// that did not embed them. Best-effort: `None` on any failure.
pub async fn fetch_openai_reset_credits(
    state: &AppState,
    acct: &Account,
    access_token: &str,
) -> Option<serde_json::Value> {
    let account_id = acct.provider_account_id.as_deref()?;
    let resp = state
        .http_client
        .get(openai_reset_credits_url())
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header(reqwest::header::ACCEPT, "*/*")
        .send()
        .await
        .map_err(
            |e| tracing::debug!(account = %acct.id, "openai reset credits transport error: {e}"),
        )
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(account = %acct.id, status = %resp.status(), "openai reset credits rejected");
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    map_reset_credits(&body)
}

/// Fetch an OpenAI/codex account's real 5h/7d usage from `wham/usage`.
///
/// Uses only stored OAuth data: a fresh Bearer via [`current_access_token`] and the
/// `chatgpt-account-id` from `acct.provider_account_id`. Returns `None` (so the
/// caller falls back to the local tally) when the account has no account-id, the
/// token can't be refreshed, the call fails/errs, or the body has no rate-limit
/// windows — logging the reason in each case. No inference request, costs no tokens.
pub async fn fetch_openai_usage(state: &AppState, acct: &Account) -> Option<serde_json::Value> {
    let account_id = acct.provider_account_id.as_deref()?;
    let access_token = current_access_token(state, acct).await.ok()?;
    let resp = state
        .http_client
        .get(openai_usage_url())
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header(reqwest::header::ACCEPT, "*/*")
        .send()
        .await
        .map_err(|e| tracing::warn!(account = %acct.id, "openai usage transport error: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(account = %acct.id, status = %resp.status(), "openai usage rejected");
        return None;
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| tracing::warn!(account = %acct.id, "openai usage decode error: {e}"))
        .ok()?;
    let mut usage = map_wham_usage(&body)?;
    if usage.get("reset_credits").is_none()
        && let Some(credits) = fetch_openai_reset_credits(state, acct, &access_token).await
    {
        usage["reset_credits"] = credits;
    }
    Some(usage)
}

/// Whether the soft-limit check must refresh usage from upstream before deciding.
///
/// A cold (`None`) or stale cache must trigger a refresh — treating a cold
/// cache as "no data → allow" would let a capped account run to 100% whenever
/// no human was viewing it.
pub fn usage_cache_stale(entry_age: Option<std::time::Duration>, ttl: std::time::Duration) -> bool {
    entry_age.is_none_or(|age| age >= ttl)
}

/// Usage payload for the soft-limit check on the dispatch hot path.
///
/// Serves the per-account usage cache, refreshing it from upstream when it is
/// cold or older than [`accounts::USAGE_CACHE_TTL`] (≤ once per TTL per account,
/// so the rate-limited endpoint is never spammed). On a fetch error it falls back
/// to the last cached value, else `None` (fail open). Unlike the accounts-page
/// route, this exists so the cap holds even when no human is viewing the account.
pub async fn usage_for_soft_limit(state: &AppState, account_id: Uuid) -> Option<serde_json::Value> {
    let ttl = crate::routes::accounts::USAGE_CACHE_TTL.to_std().unwrap_or_default();
    let entry_age = state.account_usage_cache.get(&account_id).map(|h| h.fetched_at.elapsed());
    if !usage_cache_stale(entry_age, ttl) {
        return state.account_usage_cache.get(&account_id).and_then(|h| h.usage.clone());
    }
    match fetch_usage_with_provider(state, account_id).await {
        Ok((provider, usage)) => {
            crate::routes::accounts::store_and_broadcast_usage(
                state,
                account_id,
                provider,
                usage.clone(),
            )
            .await;
            record_usage_samples(state, account_id, usage.as_ref());
            usage
        }
        // Upstream hiccup (429/refresh fail): fall back to the last cached value.
        Err(_) => state.account_usage_cache.get(&account_id).and_then(|h| h.usage.clone()),
    }
}

/// Append a fresh upstream reading to the credential's usage history, off the
/// request path: the caller has a payload to serve and the history is a
/// refinement (see [`crate::store::usage_samples`]). Nothing to record for a
/// credential with no usage.
pub fn record_usage_samples(
    state: &AppState,
    provider_id: Uuid,
    usage: Option<&serde_json::Value>,
) {
    let Some(usage) = usage else {
        return;
    };
    let windows = crate::soft_limit::normalize_usage_windows(usage);
    if windows.is_empty() {
        return;
    }
    let pool = state.pool.clone();
    tokio::spawn(async move {
        crate::store::usage_samples::record(
            &pool,
            provider_id,
            &windows,
            chrono::Utc::now(),
            "poll",
        )
        .await;
    });
}

/// Fetch the Anthropic OAuth usage windows for an account.
///
/// Reloads + decrypts the account, ensures a fresh access token (refreshing under
/// the per-account mutex if needed), and calls Anthropic's free usage endpoint.
/// Returns:
///   * `Ok(Some(json))` — anthropic account (fetched upstream) or OpenAI/codex
///     account (metered locally from `session_token_usage`)
///   * `Ok(None)` — no such account
///   * `Err(status)` — token refresh failed or upstream rejected (e.g. 429)
///
/// This makes NO inference request and costs no tokens. Callers MUST throttle it
/// (the endpoint rate-limits per access token); see the usage cache in the route.
pub async fn fetch_account_usage(
    state: &AppState,
    account_id: Uuid,
) -> Result<Option<serde_json::Value>, StatusCode> {
    fetch_usage_with_provider(state, account_id).await.map(|(_, usage)| usage)
}

/// [`fetch_account_usage`] plus the provider it resolved, which the broadcast
/// needs to normalize the windows without decrypting the account a second time.
pub async fn fetch_usage_with_provider(
    state: &AppState,
    account_id: Uuid,
) -> Result<(String, Option<serde_json::Value>), StatusCode> {
    let Some(acct) = reload_account(state, account_id).await else {
        return Ok((String::new(), None));
    };
    let provider = acct.provider.clone();
    usage_for_account(state, acct).await.map(|usage| (provider, usage))
}

async fn usage_for_account(
    state: &AppState,
    acct: Account,
) -> Result<Option<serde_json::Value>, StatusCode> {
    let account_id = acct.id;
    // Pay-per-token: dollars, not percent of a subscription window. Metered
    // locally, then reconciled upward against the provider's billing API.
    if Family::from_provider(&acct.provider) == Family::Fireworks {
        return fireworks_usd_windows(state, &acct).await;
    }
    if acct.provider != "anthropic" {
        // OpenAI/codex accounts: read the ChatGPT backend's REAL 5h/7d rate-limit
        // windows — the same numbers `codex /status` shows, keyed on the
        // stored OAuth Bearer + chatgpt-account-id. Fall back to the local token
        // tally only when that call can't produce windows (no account-id,
        // token refresh fail, upstream error, or a body without rate limits), so
        // freshly-enrolled / API-key accounts still render something.
        if acct.provider == "openai"
            && let Some(usage) = fetch_openai_usage(state, &acct).await
        {
            return Ok(Some(usage));
        }
        tracing::info!(account = %account_id, "openai usage unavailable; falling back to local token tally");
        return local_usage_windows(state, account_id).await;
    }
    let access_token = current_access_token(state, &acct).await?;
    let resp = state
        .http_client
        .get(anthropic_usage_url())
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header(reqwest::header::USER_AGENT, anthropic_usage_user_agent())
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(account = %account_id, "usage fetch transport error: {e}");
            StatusCode::BAD_GATEWAY
        })?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        tracing::warn!(account = %account_id, %status, "usage fetch rejected");
        return Err(status);
    }
    let json: serde_json::Value = resp.json().await.map_err(|e| {
        tracing::warn!(account = %account_id, "usage decode error: {e}");
        StatusCode::BAD_GATEWAY
    })?;
    Ok(Some(json))
}

/// Per-window token budget an OpenAI/codex account's local utilization is measured
/// against. Codex exposes no free usage endpoint, so we can't read a real
/// quota — utilization is `tokens_used_in_window / budget`. The budgets are
/// arbitrary-but-tunable so the soft-limit % stays meaningful per plan; override
/// via env to match whatever ChatGPT/codex tier the account is on.
pub fn openai_5h_token_budget() -> i64 {
    std::env::var("CCTUI_OPENAI_5H_TOKEN_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(8_000_000)
}

pub fn openai_7d_token_budget() -> i64 {
    std::env::var("CCTUI_OPENAI_7D_TOKEN_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(80_000_000)
}

/// Compute provider-agnostic 5h/7d usage windows from cctui's own recorded token
/// usage. Sums every token kind across the account's sessions inside each
/// rolling window and divides by the configured budget for a utilization percent.
/// `resets_at` is when the oldest contributing usage ages out of the window (i.e.
/// when capacity frees up). Emits the same JSON shape as the Anthropic usage
/// endpoint so [`crate::soft_limit`] and the accounts UI consume it unchanged.
pub async fn local_usage_windows(
    state: &AppState,
    account_id: Uuid,
) -> Result<Option<serde_json::Value>, StatusCode> {
    let five_hour = local_window(state, account_id, "5 hours", openai_5h_token_budget()).await?;
    let seven_day = local_window(state, account_id, "7 days", openai_7d_token_budget()).await?;
    Ok(Some(serde_json::json!({
        "five_hour": five_hour,
        "seven_day": seven_day,
    })))
}

pub fn window_utilization(tokens: i64, budget: i64) -> f64 {
    (tokens as f64 / budget as f64) * 100.0
}

/// One rolling window's `{utilization, resets_at}` from `session_token_usage`.
pub async fn local_window(
    state: &AppState,
    account_id: Uuid,
    interval: &str,
    budget: i64,
) -> Result<serde_json::Value, StatusCode> {
    // SUM() over bigint returns NUMERIC; cast back to bigint or sqlx fails to
    // decode into i64. `oldest` is the earliest in-window usage row.
    let row: (i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT COALESCE(SUM(stu.input_tokens + stu.output_tokens \
                + stu.cache_read_tokens + stu.cache_creation_tokens), 0)::bigint AS tokens, \
                MIN(stu.created_at) AS oldest \
         FROM session_tokens st \
         JOIN session_token_usage stu ON stu.session_id = st.session_id \
         WHERE st.account_id = $1 AND stu.created_at >= now() - $2::interval",
    )
    .bind(account_id)
    .bind(interval)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        tracing::warn!(account = %account_id, "local usage query error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let (tokens, oldest) = row;
    let utilization = window_utilization(tokens, budget);
    // The window frees up when the oldest contributing row falls out of it.
    let resets_at = oldest.map(|t| {
        let secs = if interval == "5 hours" { 5 * 3600 } else { 7 * 86400 };
        (t + chrono::Duration::seconds(secs)).to_rfc3339()
    });
    Ok(serde_json::json!({
        "utilization": utilization,
        "resets_at": resets_at,
    }))
}

/// The provider row's model catalog — the only source of prices.
pub async fn account_catalog(state: &AppState, account_id: Uuid) -> Option<serde_json::Value> {
    sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT models FROM account_providers WHERE id = $1",
    )
    .bind(account_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten()
    .flatten()
}

/// One raw usage-tally row: model, non-cached input, cached input, output,
/// oldest contributing timestamp.
pub type TallyRow = (Option<String>, i64, i64, i64, Option<chrono::DateTime<Utc>>);

/// One model's tallied usage in a window, with the oldest contributing row.
pub type ModelTally = (Option<String>, crate::cost::TokenUsage, Option<chrono::DateTime<Utc>>);

/// Per-model token tallies for one account, restricted by an SQL predicate on
/// `stu`/`st` bound to `$2`.
pub async fn model_tallies(
    state: &AppState,
    account_id: Uuid,
    filter: &str,
    bind: &str,
) -> Vec<ModelTally> {
    let sql = format!(
        "SELECT stu.model, \
                COALESCE(SUM(stu.input_tokens + stu.cache_creation_tokens), 0)::bigint, \
                COALESCE(SUM(stu.cache_read_tokens), 0)::bigint, \
                COALESCE(SUM(stu.output_tokens), 0)::bigint, \
                MIN(stu.created_at) \
         FROM session_tokens st \
         JOIN session_token_usage stu ON stu.session_id = st.session_id \
         WHERE st.account_id = $1 AND {filter} \
         GROUP BY stu.model"
    );
    let rows: Vec<TallyRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(account_id)
        .bind(bind)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(account = %account_id, "usd usage query error: {e}");
            Vec::new()
        });
    rows.into_iter()
        .map(|(model, input, cached, output, oldest)| {
            (model, crate::cost::TokenUsage { input, cached_input: cached, output }, oldest)
        })
        .collect()
}

pub fn priced(catalog: Option<&serde_json::Value>, rows: &[ModelTally]) -> f64 {
    let tallies: Vec<_> = rows.iter().map(|(m, u, _)| (m.clone(), *u)).collect();
    crate::cost::tallies_cost_usd(catalog, &tallies)
}

/// The dearest single session under this account within `interval`, priced from
/// the catalog.
///
/// `session_usd` caps a session, not the account, so the account-level figure
/// that means anything is the session closest to that cap. Windowed so the scan
/// stays bounded; `None` when nothing was metered.
pub async fn max_session_spend_usd(
    pool: &sqlx::PgPool,
    account_id: Uuid,
    catalog: Option<&serde_json::Value>,
    interval: &str,
) -> Option<f64> {
    let rows: Vec<(String, Option<String>, i64, i64, i64)> = sqlx::query_as(
        "SELECT st.session_id, stu.model, \
                COALESCE(SUM(stu.input_tokens + stu.cache_creation_tokens), 0)::bigint, \
                COALESCE(SUM(stu.cache_read_tokens), 0)::bigint, \
                COALESCE(SUM(stu.output_tokens), 0)::bigint \
         FROM session_tokens st \
         JOIN session_token_usage stu ON stu.session_id = st.session_id \
         WHERE st.account_id = $1 AND stu.created_at >= now() - $2::interval \
         GROUP BY st.session_id, stu.model",
    )
    .bind(account_id)
    .bind(interval)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(account = %account_id, "session usd usage query error: {e}");
        Vec::new()
    });
    if rows.is_empty() {
        return None;
    }
    let mut per_session: std::collections::HashMap<String, Vec<(Option<String>, _)>> =
        std::collections::HashMap::new();
    for (session_id, model, input, cached, output) in rows {
        per_session
            .entry(session_id)
            .or_default()
            .push((model, crate::cost::TokenUsage { input, cached_input: cached, output }));
    }
    per_session
        .into_values()
        .map(|tallies| crate::cost::tallies_cost_usd(catalog, &tallies))
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

/// USD spent by one session under this account, priced from the catalog.
pub async fn session_spend_usd(
    state: &AppState,
    account_id: Uuid,
    session_id: &str,
) -> Option<f64> {
    let catalog = account_catalog(state, account_id).await;
    let rows = model_tallies(state, account_id, "st.session_id = $2", session_id).await;
    Some(priced(catalog.as_ref(), &rows))
}

/// cctui's own 7d spend as Fireworks billed it, priced through the account
/// catalog because every upstream row reports `costNanoUsd: 0`.
///
/// `None` whenever the figure cannot be trusted — no billing key name configured,
/// no resolvable account slug, or any upstream failure — leaving the caller on its
/// locally metered number. A reconciliation that cannot be attributed must never
/// fall back to the whole account: the spend would be someone else's.
pub async fn fireworks_upstream_7d_usd(
    state: &AppState,
    acct: &Account,
    catalog: Option<&serde_json::Value>,
) -> Option<f64> {
    let key_name = FireworksSettings::resolve(acct.provider_settings.as_ref())
        .billing_api_key_name
        .or_else(|| {
            tracing::debug!(account = %acct.id, "fireworks billing reconciliation off: no api key name");
            None
        })?;
    let api_key = current_access_token(state, acct).await.ok()?;
    let slug = fireworks_account_slug(state, acct, &api_key).await?;
    let end = Utc::now();
    let start = end - chrono::Duration::days(7);
    let resp = state
        .http_client
        .get(format!("{}/accounts/{slug}/billingUsage", crate::fireworks_billing::billing_base()))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .query(&[
            ("startTime", start.to_rfc3339()),
            ("endTime", end.to_rfc3339()),
            ("usageType", "SERVERLESS".into()),
            ("groupBy", "api_key_name".into()),
            ("groupBy", "model_name".into()),
        ])
        .send()
        .await
        .map_err(|e| tracing::warn!(account = %acct.id, "fireworks billing transport error: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(account = %acct.id, status = %resp.status(), "fireworks billing rejected");
        return None;
    }
    let body = resp.bytes().await.ok()?;
    let rows = crate::fireworks_billing::parse_billing_usage(&body, &key_name);
    if rows.is_empty() {
        return None;
    }
    let tallies: Vec<_> = rows.into_iter().map(|(m, u)| (Some(m), u)).collect();
    Some(crate::cost::tallies_cost_usd(catalog, &tallies))
}

/// The account slug the billing API is addressed by, cached on the provider row.
/// A Fireworks API key does not name its account, so the first call resolves it
/// from `/accounts` and persists it; an ambiguous listing resolves to nothing
/// rather than guessing which account to bill against.
pub async fn fireworks_account_slug(
    state: &AppState,
    acct: &Account,
    api_key: &str,
) -> Option<String> {
    if let Some(slug) = acct.provider_account_id.as_deref().filter(|s| !s.trim().is_empty()) {
        return Some(slug.to_owned());
    }
    let resp = state
        .http_client
        .get(format!("{}/accounts", crate::fireworks_billing::billing_base()))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;
    let accounts = json.get("accounts")?.as_array()?;
    let [only] = accounts.as_slice() else {
        tracing::warn!(account = %acct.id, n = accounts.len(), "fireworks account slug ambiguous");
        return None;
    };
    let slug = only.get("name")?.as_str()?.strip_prefix("accounts/")?.to_owned();
    let _ = sqlx::query("UPDATE account_providers SET provider_account_id = $2 WHERE id = $1")
        .bind(acct.id)
        .bind(&slug)
        .execute(&state.pool)
        .await;
    Some(slug)
}

/// Rolling dollar-spend windows for a pay-per-token account: cctui's own recorded
/// usage priced against the account's catalog, then reconciled upward against the
/// provider's billing API so a window never reads lower than what was actually
/// billed. Emitted in the same fixed-field shape the rest of the pipeline consumes.
pub async fn fireworks_usd_windows(
    state: &AppState,
    acct: &Account,
) -> Result<Option<serde_json::Value>, StatusCode> {
    let account_id = acct.id;
    let catalog = account_catalog(state, account_id).await;
    let upstream_7d = fireworks_upstream_7d_usd(state, acct, catalog.as_ref()).await;
    let mut local = std::collections::BTreeMap::new();
    let mut out = serde_json::Map::new();
    for (key, interval, secs) in [
        (crate::soft_limit::KEY_USD_5H, "5 hours", 5 * 3600_i64),
        (crate::soft_limit::KEY_USD_7D, "7 days", 7 * 86400),
    ] {
        let rows =
            model_tallies(state, account_id, "stu.created_at >= now() - $2::interval", interval)
                .await;
        let resets_at = rows
            .iter()
            .filter_map(|(_, _, oldest)| *oldest)
            .min()
            .map(|t| (t + chrono::Duration::seconds(secs)).to_rfc3339());
        local.insert(key, priced(catalog.as_ref(), &rows));
        out.insert(key.to_owned(), serde_json::json!({ "resets_at": resets_at }));
    }
    let recent = local.get(crate::soft_limit::KEY_USD_5H).copied().unwrap_or_default();
    let week = local.get(crate::soft_limit::KEY_USD_7D).copied().unwrap_or_default();
    for (key, amount) in [
        (
            crate::soft_limit::KEY_USD_5H,
            crate::fireworks_billing::reconcile_5h(recent, week, upstream_7d),
        ),
        (crate::soft_limit::KEY_USD_7D, crate::fireworks_billing::reconcile_7d(week, upstream_7d)),
    ] {
        if let Some(w) = out.get_mut(key).and_then(serde_json::Value::as_object_mut) {
            w.insert("amount_usd".into(), serde_json::json!(amount));
        }
    }
    // Emitted unconditionally, like the 5h/7d windows: an account that metered
    // nothing has spent $0, which is a report. Omitting it renders a configured
    // cap as "not currently reported" forever.
    let top = max_session_spend_usd(&state.pool, account_id, catalog.as_ref(), "5 hours")
        .await
        .unwrap_or(0.0);
    out.insert(
        crate::soft_limit::KEY_SESSION_USD.to_owned(),
        serde_json::json!({ "amount_usd": top, "resets_at": serde_json::Value::Null }),
    );
    Ok(Some(serde_json::Value::Object(out)))
}

/// Persist one Fireworks response's usage. Idempotent on
/// `(session_id, message_id)`; a response without an upstream id gets a
/// synthetic one, so a retry of the same call is counted once per response, not
/// per attempt.
pub async fn record_fireworks_usage(
    pool: sqlx::PgPool,
    session_id: String,
    model: Option<String>,
    captured: crate::cost::CapturedUsage,
    gateway_rewrote_body: bool,
) {
    let message_id =
        captured.message_id.unwrap_or_else(|| format!("fw-{}", uuid::Uuid::new_v4().simple()));
    let u = captured.usage;
    if let Err(e) = sqlx::query(
        "INSERT INTO session_token_usage \
             (session_id, message_id, input_tokens, output_tokens, cache_read_tokens, model, \
              gateway_rewrote_body) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (session_id, message_id) DO NOTHING",
    )
    .bind(&session_id)
    .bind(&message_id)
    .bind(u.input)
    .bind(u.output)
    .bind(u.cached_input)
    .bind(model)
    .bind(gateway_rewrote_body)
    .execute(&pool)
    .await
    {
        // A foreign-key violation here is not transient: the session id the
        // gateway meters under has no `sessions` row, so every request for this
        // session records nothing and its spend silently reads $0.
        let unmetered = e
            .as_database_error()
            .is_some_and(|db| db.is_foreign_key_violation() || db.is_check_violation());
        if unmetered {
            tracing::error!(
                session = %session_id,
                "fireworks usage record rejected — session is unknown to the sessions table, \
                 so its spend will never be metered: {e}"
            );
        } else {
            tracing::warn!(session = %session_id, "fireworks usage record failed: {e}");
        }
    }
}
