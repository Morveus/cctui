//! "Reset my usage limit" on a provider credential: Codex's "Redeem usage limit
//! reset" credits and Claude Code's `/limit-reset`, claimed server-side with the
//! stored OAuth credential so no interactive CLI is needed. Both are undocumented
//! upstream APIs: best-effort, fail soft, never retried in a loop.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::routes::accounts::{err, require_human};
use crate::routes::gateway::{self, Account};
use crate::state::AppState;

type ApiError = (StatusCode, Json<serde_json::Value>);

/// The endpoint that spends a Codex reset credit, overridable on its own: the
/// `wham` default performs the reset but answers in the credits shape, while
/// codex's own `/api/codex/rate-limit-reset-credits/consume` answers `{outcome}`.
pub fn openai_consume_url() -> String {
    std::env::var("CCTUI_OPENAI_RESET_CREDITS_CONSUME_URL")
        .unwrap_or_else(|_| format!("{}/consume", gateway::openai_reset_credits_url()))
}

pub fn anthropic_profile_url() -> String {
    std::env::var("CCTUI_ANTHROPIC_OAUTH_PROFILE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com/api/oauth/profile".into())
}

pub fn anthropic_reset_url(organization_uuid: &str) -> String {
    let base = std::env::var("CCTUI_ANTHROPIC_API_BASE")
        .unwrap_or_else(|_| "https://api.anthropic.com".into());
    format!("{base}/api/organizations/{organization_uuid}/reset_rate_limits")
}

/// What the account's latest usage payload says about a limit reset, normalized
/// across providers for the button in the usage row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LimitResetStatus {
    /// `codex` (reset credits) or `claude` (`juniper_tide`).
    pub kind: &'static str,
    /// Whether a claim would do anything right now.
    pub available: bool,
    /// Codex: the redeemable credit's title (e.g. "Full reset (Weekly + 5 hr)").
    pub title: Option<String>,
    /// Codex: the credit a claim would name.
    pub credit_id: Option<String>,
    /// Claude: why the reset cannot be claimed (e.g. `not_at_wall`).
    pub ineligible_reason: Option<String>,
    pub next_available_at: Option<String>,
    pub weekly_resets_at: Option<String>,
}

/// Derive the reset status from the usage JSON `GET /accounts/{id}/usage`
/// serves. `None` when the provider has no reset mechanism or the payload does
/// not mention one (an Anthropic account outside the experiment, a Codex body
/// with no credits block).
pub fn limit_reset_status(provider: &str, usage: &serde_json::Value) -> Option<LimitResetStatus> {
    let str_at =
        |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    match provider {
        "openai" => {
            let credits = usage.get("reset_credits")?;
            let available_count =
                credits.get("available_count").and_then(serde_json::Value::as_i64).unwrap_or(0);
            let first = credits.get("credits").and_then(|c| c.as_array()).and_then(|list| {
                list.iter()
                    .find(|c| c.get("status").and_then(|s| s.as_str()) == Some("available"))
                    .or_else(|| list.first())
            });
            Some(LimitResetStatus {
                kind: "codex",
                available: available_count > 0,
                title: first.and_then(|c| str_at(c, "title")),
                credit_id: first.and_then(|c| str_at(c, "id")),
                ineligible_reason: None,
                next_available_at: first.and_then(|c| str_at(c, "expires_at")),
                weekly_resets_at: None,
            })
        }
        "anthropic" => {
            let jt = usage.get("juniper_tide")?;
            let flag = |k: &str| jt.get(k).and_then(serde_json::Value::as_bool).unwrap_or(false);
            Some(LimitResetStatus {
                kind: "claude",
                available: flag("available") && flag("eligible"),
                title: None,
                credit_id: None,
                ineligible_reason: str_at(jt, "ineligible_reason"),
                next_available_at: str_at(jt, "next_available_at"),
                weekly_resets_at: str_at(jt, "weekly_resets_at"),
            })
        }
        _ => None,
    }
}

/// Upstream outcomes arrive `camelCase` from the app-server shape and `snake_case`
/// from the HTTP one; the audit row and the UI see one spelling.
pub fn normalize_outcome(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    for (i, ch) in raw.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The outcome of a 2xx `consume` response. The `wham` endpoint answers with the
/// credits block instead of the app-server's `{outcome}`, so the claimed credit
/// no longer being available is proof it was spent. `error` means the body said
/// nothing at all; `unconfirmed` means the call went through but the body could
/// not be read, and must not read as an upstream rejection.
pub fn consume_outcome(credit_id: Option<&str>, body: &serde_json::Value) -> String {
    if let Some(raw) = body.get("outcome").and_then(|o| o.as_str()) {
        return normalize_outcome(raw);
    }
    if !body.is_object() {
        return "error".to_owned();
    }
    let Some(credits) = gateway::map_reset_credits(body) else {
        return "unconfirmed".to_owned();
    };
    let list = credits["credits"].as_array().cloned().unwrap_or_default();
    let spent = credit_id.map_or_else(
        || credits["available_count"].as_i64() == Some(0),
        |id| {
            !list.iter().any(|c| {
                c.get("id").and_then(serde_json::Value::as_str) == Some(id)
                    && c.get("status").and_then(serde_json::Value::as_str) == Some("available")
            })
        },
    );
    if spent { "reset".to_owned() } else { "unconfirmed".to_owned() }
}

/// Whether the claim may have moved the account's windows, so the cached usage
/// must be dropped rather than served until the next poll.
pub fn invalidates_usage(outcome: &str) -> bool {
    !matches!(outcome, "error" | "unavailable" | "already_redeemed")
}

/// What a repeat claim on the same credit does with the prior attempt's row.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaimPlan {
    /// The credit was already redeemed by us: answer locally, send nothing.
    AlreadyRedeemed { idempotency_key: String },
    /// Send the consume request under this key (the prior attempt's if it did
    /// not settle, else a fresh one).
    Send { idempotency_key: String, reused: bool },
}

pub fn plan_claim(prior: Option<(String, String)>, fresh_key: String) -> ClaimPlan {
    match prior {
        Some((key, outcome)) if outcome == "reset" || outcome == "already_redeemed" => {
            ClaimPlan::AlreadyRedeemed { idempotency_key: key }
        }
        Some((key, _)) => ClaimPlan::Send { idempotency_key: key, reused: true },
        None => ClaimPlan::Send { idempotency_key: fresh_key, reused: false },
    }
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct LimitResetRequest {
    /// Codex: which credit to consume. Defaults to the first available one from
    /// the cached usage.
    #[serde(default)]
    pub credit_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct LimitResetResponse {
    pub account_id: Uuid,
    pub provider: String,
    /// Upstream outcome verbatim (`snake_case`), `error` when the call failed, or
    /// `unconfirmed` when it succeeded with a body we could not read.
    pub outcome: String,
    pub credit_id: Option<String>,
    pub next_available_at: Option<String>,
    pub weekly_resets_at: Option<String>,
    pub idempotency_key: String,
    /// The click matched a prior attempt and no new consume request was sent.
    pub reused: bool,
}

/// `POST /api/v1/accounts/{id}/limit-reset` — claim a usage-limit reset on a
/// provider credential. `{id}` is the provider-row id. Ownership as for usage:
/// a user may only act on their own providers; admin on any.
pub async fn limit_reset(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    body: Option<Json<LimitResetRequest>>,
) -> Result<Json<LimitResetResponse>, ApiError> {
    require_human(&ctx)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let provider: Option<String> = sqlx::query_scalar(
        "SELECT provider FROM account_providers \
         WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
    )
    .bind(id)
    .bind(ctx.owner_filter())
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error: {e}");
        err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;
    let Some(provider) = provider else {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    };
    let Some(acct) = gateway::reload_account(&state, id).await else {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    };
    let access_token = gateway::current_access_token(&state, &acct)
        .await
        .map_err(|s| err(s, "could not obtain an access token for this account"))?;

    let out = match provider.as_str() {
        "openai" => claim_codex(&state, &acct, &access_token, req.credit_id).await,
        "anthropic" => claim_claude(&state, &acct, &access_token).await,
        _ => return Err(err(StatusCode::BAD_REQUEST, "this provider has no limit reset")),
    };
    record(&state, id, &out, ctx.user_id).await;
    if invalidates_usage(&out.outcome) {
        state.account_usage_cache.remove(&id);
        // Net-zero upstream: the eviction above already forced the next reader
        // to fetch; doing it here just leaves the cache warm and pushes once.
        if let Ok((p, usage)) = crate::routes::gateway::fetch_usage_with_provider(&state, id).await
        {
            crate::routes::gateway::record_usage_samples(&state, id, usage.as_ref());
            crate::routes::accounts::store_and_broadcast_usage(&state, id, p, usage).await;
        }
    }
    Ok(Json(LimitResetResponse { account_id: id, provider, ..out }))
}

async fn record(state: &AppState, id: Uuid, out: &LimitResetResponse, requested_by: Uuid) {
    if let Err(e) = sqlx::query(
        "INSERT INTO account_limit_resets \
             (provider_id, idempotency_key, credit_id, outcome, requested_by) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(&out.idempotency_key)
    .bind(&out.credit_id)
    .bind(&out.outcome)
    .bind(requested_by)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(account = %id, "limit reset audit row failed: {e}");
    }
}

fn blank(outcome: &str, idempotency_key: String) -> LimitResetResponse {
    LimitResetResponse {
        account_id: Uuid::nil(),
        provider: String::new(),
        outcome: outcome.to_owned(),
        credit_id: None,
        next_available_at: None,
        weekly_resets_at: None,
        idempotency_key,
        reused: false,
    }
}

async fn claim_codex(
    state: &AppState,
    acct: &Account,
    access_token: &str,
    credit_id: Option<String>,
) -> LimitResetResponse {
    let credit_id = credit_id.or_else(|| {
        state
            .account_usage_cache
            .get(&acct.id)
            .and_then(|h| h.usage.clone())
            .and_then(|u| limit_reset_status("openai", &u))
            .and_then(|s| s.credit_id)
    });
    let prior: Option<(String, String)> = sqlx::query_as(
        "SELECT idempotency_key, outcome FROM account_limit_resets \
         WHERE provider_id = $1 AND credit_id IS NOT DISTINCT FROM $2 \
         ORDER BY at DESC LIMIT 1",
    )
    .bind(acct.id)
    .bind(&credit_id)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or_default();
    let (key, reused) = match plan_claim(prior, Uuid::new_v4().to_string()) {
        ClaimPlan::AlreadyRedeemed { idempotency_key } => {
            let mut r = blank("already_redeemed", idempotency_key);
            r.credit_id = credit_id;
            r.reused = true;
            return r;
        }
        ClaimPlan::Send { idempotency_key, reused } => (idempotency_key, reused),
    };
    let mut out = blank("error", key.clone());
    out.credit_id = credit_id.clone();
    out.reused = reused;
    let Some(account_id) = acct.provider_account_id.as_deref() else {
        return out;
    };
    let mut body = serde_json::json!({ "redeem_request_id": key });
    if let Some(c) = &credit_id {
        body["credit_id"] = serde_json::Value::String(c.clone());
    }
    let resp = state
        .http_client
        .post(openai_consume_url())
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header(reqwest::header::ACCEPT, "*/*")
        .json(&body)
        .send()
        .await;
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!(account = %acct.id, status = %r.status(), "codex limit reset rejected");
            return out;
        }
        Err(e) => {
            tracing::warn!(account = %acct.id, "codex limit reset transport error: {e}");
            return out;
        }
    };
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    if json.get("outcome").and_then(|o| o.as_str()).is_none() {
        tracing::warn!(account = %acct.id, body = %json, "codex limit reset: 2xx body carries no outcome");
    }
    out.outcome = consume_outcome(credit_id.as_deref(), &json);
    out
}

async fn organization_uuid(state: &AppState, acct: &Account, access_token: &str) -> Option<String> {
    let stored: Option<Option<String>> =
        sqlx::query_scalar("SELECT organization_uuid FROM account_providers WHERE id = $1")
            .bind(acct.id)
            .fetch_optional(&state.pool)
            .await
            .ok()?;
    if let Some(org) = stored.flatten().filter(|s| !s.trim().is_empty()) {
        return Some(org);
    }
    let resp = state
        .http_client
        .get(anthropic_profile_url())
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header(reqwest::header::USER_AGENT, gateway::anthropic_usage_user_agent())
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .map_err(|e| tracing::warn!(account = %acct.id, "oauth profile transport error: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(account = %acct.id, status = %resp.status(), "oauth profile rejected");
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let org = json.pointer("/organization/uuid")?.as_str()?.to_owned();
    gateway::remember_organization_uuid(state, acct.id, &org).await;
    Some(org)
}

async fn claim_claude(state: &AppState, acct: &Account, access_token: &str) -> LimitResetResponse {
    let key = Uuid::new_v4().to_string();
    let mut out = blank("unavailable", key);
    let Some(org) = organization_uuid(state, acct, access_token).await else {
        tracing::warn!(account = %acct.id, "claude limit reset: organization uuid unknown");
        return out;
    };
    let lock = state
        .account_locks
        .entry(acct.id)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;
    let resp = state
        .http_client
        .post(anthropic_reset_url(&org))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header(reqwest::header::USER_AGENT, gateway::anthropic_usage_user_agent())
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&serde_json::json!({ "program": "juniper_tide" }))
        .send()
        .await;
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            tracing::warn!(account = %acct.id, status = %r.status(), "claude limit reset rejected");
            out.outcome = "error".into();
            return out;
        }
        Err(e) => {
            tracing::warn!(account = %acct.id, "claude limit reset transport error: {e}");
            out.outcome = "error".into();
            return out;
        }
    };
    let json: serde_json::Value = resp.json().await.unwrap_or_default();
    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    out.outcome = s("result").map_or_else(|| "error".to_owned(), |r| normalize_outcome(&r));
    out.next_available_at = s("next_available_at");
    out.weekly_resets_at = s("weekly_resets_at");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn juniper_tide_block_is_surfaced() {
        let usage = serde_json::json!({
            "five_hour": { "utilization": 100.0, "resets_at": "2026-09-05T10:00:00Z" },
            "juniper_tide": {
                "eligible": true, "ineligible_reason": null, "in_experiment": true,
                "arm": "reset", "available": true,
                "next_available_at": "2026-09-12T00:00:00Z",
                "weekly_resets_at": "2026-09-08T00:00:00Z", "resets_per_week": 1
            }
        });
        let s = limit_reset_status("anthropic", &usage).unwrap();
        assert_eq!(s.kind, "claude");
        assert!(s.available);
        assert_eq!(s.ineligible_reason, None);
        assert_eq!(s.next_available_at.as_deref(), Some("2026-09-12T00:00:00Z"));
        assert_eq!(s.weekly_resets_at.as_deref(), Some("2026-09-08T00:00:00Z"));

        let not_at_wall = serde_json::json!({
            "juniper_tide": { "eligible": false, "ineligible_reason": "not_at_wall", "available": true }
        });
        let s = limit_reset_status("anthropic", &not_at_wall).unwrap();
        assert!(!s.available);
        assert_eq!(s.ineligible_reason.as_deref(), Some("not_at_wall"));

        assert!(limit_reset_status("anthropic", &serde_json::json!({ "five_hour": {} })).is_none());
    }

    #[test]
    fn codex_reset_credits_are_surfaced() {
        let usage = serde_json::json!({
            "reset_credits": {
                "available_count": 1,
                "credits": [{
                    "id": "cr_1", "status": "available", "reset_type": "full",
                    "granted_at": "2026-09-01T00:00:00Z", "expires_at": "2026-09-08T00:00:00Z",
                    "title": "Full reset (Weekly + 5 hr)"
                }]
            }
        });
        let s = limit_reset_status("openai", &usage).unwrap();
        assert_eq!(s.kind, "codex");
        assert!(s.available);
        assert_eq!(s.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert_eq!(s.credit_id.as_deref(), Some("cr_1"));

        let none = serde_json::json!({ "reset_credits": { "available_count": 0, "credits": [] } });
        let s = limit_reset_status("openai", &none).unwrap();
        assert!(!s.available);
        assert_eq!(s.credit_id, None);

        assert!(limit_reset_status("openai", &serde_json::json!({ "five_hour": {} })).is_none());
        assert!(limit_reset_status("fireworks", &usage).is_none());
    }

    #[test]
    fn wham_usage_embeds_reset_credits() {
        let body = serde_json::json!({
            "rate_limit": {
                "primary_window": { "used_percent": 42.0, "reset_at": 1_800_000_000 },
                "secondary_window": { "used_percent": 7.5, "reset_at": 1_800_500_000 }
            },
            "rate_limit_reset_credits": {
                "availableCount": 2,
                "credits": [
                    { "id": "a", "status": "available", "resetType": "full", "title": "Full reset" },
                    { "id": "b", "status": "redeemed", "resetType": "full", "title": "Full reset" }
                ]
            }
        });
        let usage = gateway::map_wham_usage(&body).unwrap();
        assert_eq!(usage["reset_credits"]["available_count"], 2);
        assert_eq!(usage["reset_credits"]["credits"][0]["reset_type"], "full");
        assert_eq!(usage["reset_credits"]["credits"][1]["status"], "redeemed");

        let plain = serde_json::json!({ "rate_limit": body["rate_limit"].clone() });
        assert!(gateway::map_wham_usage(&plain).unwrap().get("reset_credits").is_none());

        let counted = gateway::map_reset_credits(&serde_json::json!({
            "credits": [{ "id": "a", "status": "available" }, { "id": "b", "status": "expired" }]
        }))
        .unwrap();
        assert_eq!(counted["available_count"], 1);
    }

    #[test]
    fn repeat_claim_reuses_idempotency_key_and_skips_consume() {
        let fresh = || "fresh".to_owned();
        assert_eq!(
            plan_claim(None, fresh()),
            ClaimPlan::Send { idempotency_key: "fresh".into(), reused: false }
        );
        assert_eq!(
            plan_claim(Some(("k1".into(), "reset".into())), fresh()),
            ClaimPlan::AlreadyRedeemed { idempotency_key: "k1".into() }
        );
        assert_eq!(
            plan_claim(Some(("k1".into(), "already_redeemed".into())), fresh()),
            ClaimPlan::AlreadyRedeemed { idempotency_key: "k1".into() }
        );
        assert_eq!(
            plan_claim(Some(("k1".into(), "error".into())), fresh()),
            ClaimPlan::Send { idempotency_key: "k1".into(), reused: true }
        );
    }

    #[test]
    fn consume_body_without_outcome_is_read_from_the_credits_block() {
        let claimed = "RateLimitResetCredit_6feb0bc2664c8191ae128bfe969348f5";

        assert_eq!(
            consume_outcome(Some(claimed), &serde_json::json!({ "outcome": "reset" })),
            "reset"
        );
        assert_eq!(
            consume_outcome(Some(claimed), &serde_json::json!({ "outcome": "alreadyRedeemed" })),
            "already_redeemed"
        );
        assert_eq!(
            consume_outcome(Some(claimed), &serde_json::json!({ "outcome": "nothingToReset" })),
            "nothing_to_reset"
        );

        let wham = serde_json::json!({
            "rateLimitResetCredits": {
                "availableCount": 1,
                "credits": [{
                    "id": "RateLimitResetCredit_72cf3aea", "status": "available",
                    "resetType": "full", "grantedAt": "2026-09-04T00:00:00Z",
                    "expiresAt": "2026-09-11T00:00:00Z", "title": "Full reset"
                }]
            },
            "rate_limit": {
                "primary_window": { "used_percent": 0.0, "reset_at": 1_800_000_000 },
                "secondary_window": { "used_percent": 0.0, "reset_at": 1_800_500_000 }
            }
        });
        assert_eq!(consume_outcome(Some(claimed), &wham), "reset");

        let redeemed = serde_json::json!({
            "credits": [{ "id": claimed, "status": "redeemed" }]
        });
        assert_eq!(consume_outcome(Some(claimed), &redeemed), "reset");

        let untouched = serde_json::json!({
            "credits": [{ "id": claimed, "status": "available" }]
        });
        assert_eq!(consume_outcome(Some(claimed), &untouched), "unconfirmed");
        assert_eq!(consume_outcome(None, &untouched), "unconfirmed");
        assert_eq!(
            consume_outcome(None, &serde_json::json!({ "availableCount": 0, "credits": [] })),
            "reset"
        );

        assert_eq!(consume_outcome(Some(claimed), &serde_json::json!({})), "unconfirmed");
        assert_eq!(consume_outcome(Some(claimed), &serde_json::Value::Null), "error");
        assert_eq!(consume_outcome(Some(claimed), &serde_json::json!("not json")), "error");
    }

    #[test]
    fn usage_cache_is_dropped_for_every_outcome_that_may_have_reset() {
        assert!(invalidates_usage("reset"));
        assert!(invalidates_usage("unconfirmed"));
        assert!(invalidates_usage("nothing_to_reset"));
        assert!(!invalidates_usage("error"));
        assert!(!invalidates_usage("unavailable"));
        assert!(!invalidates_usage("already_redeemed"));
    }

    #[test]
    fn outcomes_normalize_to_snake_case() {
        assert_eq!(normalize_outcome("alreadyRedeemed"), "already_redeemed");
        assert_eq!(normalize_outcome("nothingToReset"), "nothing_to_reset");
        assert_eq!(normalize_outcome("noCredit"), "no_credit");
        assert_eq!(normalize_outcome("reset"), "reset");
        assert_eq!(normalize_outcome("already_used"), "already_used");
    }
}
