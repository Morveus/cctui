//! `GET /api/v1/daemon/sessions/{id}/limits` — the server side of the daemon's
//! `CctuiUsage` tool.
//!
//! Answers "what limits apply to ME" from the session's own identity: the
//! account its live gateway token is pinned to, that account's usage windows,
//! the caps actually in force (including the per-child dollar budget a
//! `CctuiAgent` parent set), and the allow/block decision the gateway would
//! reach for each model the session could switch to.
//!
//! Deliberately NOT the human `GET /accounts/usage`: that route is
//! `require_human` and lists every credential the owner holds, which a pooled or
//! shared session cannot tell itself apart in. The owner is never returned here.
//!
//! Fails soft, like the gateway's own evaluation: an empty usage cache yields no
//! windows, an allowing decision and `stale: true` rather than an error, because
//! refusing to answer would be read as "blocked".

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use cctui_proto::api::ApiError;
use chrono::Utc;
use uuid::Uuid;

use crate::routes::gateway;
use crate::soft_limit::{self, Decision, SoftLimits, UsageWindow};
use crate::state::AppState;

#[derive(Debug, Default, serde::Deserialize)]
pub struct LimitsQuery {
    /// Ask about one model instead of the session's current one.
    pub model: Option<String>,
}

/// The pinned account, named but never attributed: no owner, no credential.
#[derive(Debug, serde::Serialize)]
pub struct AccountView {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct WindowView {
    #[serde(flatten)]
    pub window: UsageWindow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace: Option<crate::pace::Pace>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct DecisionView {
    pub allow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl From<Decision> for DecisionView {
    fn from(d: Decision) -> Self {
        match d {
            Decision::Allow => {
                Self { allow: true, retry_after_secs: None, reason: None, key: None }
            }
            Decision::Block { retry_after_secs, reason, key } => Self {
                allow: false,
                retry_after_secs: Some(retry_after_secs),
                reason: Some(reason),
                key: Some(key),
            },
        }
    }
}

/// A block already written onto the session row by an earlier refusal.
#[derive(Debug, serde::Serialize)]
pub struct BlockView {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionLimits {
    pub session_id: String,
    pub account: AccountView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub windows: Vec<WindowView>,
    pub caps: SoftLimits,
    pub spend: BTreeMap<String, f64>,
    pub decision: DecisionView,
    pub per_model: BTreeMap<String, DecisionView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockView>,
    pub age_secs: u64,
    pub stale: bool,
}

fn deny(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

/// The calling session's own row: who owns it, what it runs on, and any
/// durable soft-limit block a previous refusal wrote onto it.
#[derive(Debug, sqlx::FromRow)]
struct SessionRow {
    user_id: Option<Uuid>,
    model: Option<String>,
    soft_limit_reason: Option<String>,
    soft_limit_key: Option<String>,
}

/// The account the session's live token is pinned to. Ordered so a live token
/// wins over a revoked one, matching how the spawn path resolves the same row.
#[derive(Debug, sqlx::FromRow)]
struct Binding {
    provider_id: Uuid,
    account_id: Uuid,
    name: String,
    emoji: Option<String>,
    provider: String,
    pool: Option<String>,
}

async fn binding_for(state: &AppState, session_id: &str) -> Result<Option<Binding>, sqlx::Error> {
    sqlx::query_as(
        "SELECT st.account_id AS provider_id, a.id AS account_id, a.name, a.emoji, \
                ap.provider, p.name AS pool \
         FROM session_tokens st \
         JOIN account_providers ap ON ap.id = st.account_id \
         JOIN accounts a ON a.id = ap.account_id \
         LEFT JOIN account_pools p ON p.id = st.pool_id \
         WHERE st.session_id = $1 \
         ORDER BY (st.revoked_at IS NULL) DESC, st.created_at DESC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(&state.pool)
    .await
}

/// Every model the caps can judge independently: the model-scoped windows name
/// them, so an orchestrator sees which model is blocked without a hardcoded
/// catalog that would drift from the account's own.
pub fn models_in_play(windows: &[UsageWindow], current: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = windows
        .iter()
        .filter(|w| soft_limit::is_model_scoped_key(&w.key))
        .filter_map(|w| w.model_id.clone())
        .collect();
    if let Some(model) = current.map(str::trim).filter(|m| !m.is_empty()) {
        out.push(model.to_owned());
    }
    out.sort();
    out.dedup();
    out
}

/// `GET /api/v1/daemon/sessions/{id}/limits`.
pub async fn session_limits(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(session_id): Path<String>,
    Query(q): Query<LimitsQuery>,
) -> Result<Json<SessionLimits>, (StatusCode, Json<ApiError>)> {
    let caller = crate::routes::spawn_child::machine_user(&state, &headers).await?;
    let row: Option<SessionRow> = sqlx::query_as(
        "SELECT user_id, model, soft_limit_reason, soft_limit_key FROM sessions WHERE id = $1",
    )
    .bind(&session_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!(%session_id, "db error (session limits): {e}");
        deny(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;
    let Some(session) = row else {
        return Err(deny(StatusCode::NOT_FOUND, "calling session not found"));
    };
    if session.user_id != Some(caller) {
        return Err(deny(StatusCode::FORBIDDEN, "session belongs to another user"));
    }
    let SessionRow { model: current_model, soft_limit_reason, soft_limit_key, .. } = session;

    let binding = binding_for(&state, &session_id).await.map_err(|e| {
        tracing::error!(%session_id, "db error (session limits binding): {e}");
        deny(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    })?;
    let Some(binding) = binding else {
        return Err(deny(
            StatusCode::NOT_FOUND,
            "this session has no cctui gateway token, so no account limits apply to it — it is \
             not proxied through cctui (an unbound or non-proxied adapter). Nothing is capping \
             it on cctui's side.",
        ));
    };

    let usage = gateway::usage_for_soft_limit(&state, binding.provider_id).await;
    let age_secs = state
        .account_usage_cache
        .get(&binding.provider_id)
        .map_or(0, |hit| hit.fetched_at.elapsed().as_secs());
    let cache_stale = usage.is_none()
        || gateway::usage_cache_stale(
            state.account_usage_cache.get(&binding.provider_id).map(|hit| hit.fetched_at.elapsed()),
            crate::routes::accounts::USAGE_CACHE_TTL.to_std().unwrap_or_default(),
        );

    let mut windows = usage.as_ref().map(soft_limit::normalize_usage_windows).unwrap_or_default();

    let account_caps = gateway::reload_account(&state, binding.provider_id)
        .await
        .map(|acct| acct.soft_limits)
        .unwrap_or_default();
    let budget = state.session_usd_budgets.get(&session_id).map(|b| *b);
    let caps = gateway::merge_session_budget(&account_caps, budget);

    let mut spend = BTreeMap::new();
    if let Some(spent) = gateway::session_spend_usd(&state, binding.provider_id, &session_id).await
    {
        spend.insert(soft_limit::KEY_SESSION_USD.to_owned(), spent);
        windows.push(soft_limit::usd_window(soft_limit::KEY_SESSION_USD, spent, None));
    }

    let now = Utc::now();
    let model = q
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .or(current_model);
    let decision = soft_limit::evaluate_soft_limit(&windows, &caps, model.as_deref(), now).into();
    let per_model = models_in_play(&windows, model.as_deref())
        .into_iter()
        .map(|m| {
            let d = soft_limit::evaluate_soft_limit(&windows, &caps, Some(&m), now);
            (m, DecisionView::from(d))
        })
        .collect();

    let views = windows
        .into_iter()
        .map(|w| {
            let pace = crate::pace::for_window(now, &w, None);
            WindowView { window: w, pace }
        })
        .collect();

    Ok(Json(SessionLimits {
        session_id,
        account: AccountView {
            id: binding.account_id,
            name: binding.name,
            emoji: binding.emoji,
            provider: binding.provider,
            pool: binding.pool,
        },
        model,
        windows: views,
        caps,
        spend,
        decision,
        per_model,
        block: soft_limit_reason.map(|reason| BlockView { reason, key: soft_limit_key }),
        age_secs,
        stale: cache_stale,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soft_limit::SoftLimit;

    fn window(key: &str, utilization: f64, model: Option<&str>) -> UsageWindow {
        UsageWindow {
            key: key.to_owned(),
            kind: "percent".to_owned(),
            label: key.to_owned(),
            utilization,
            amount_usd: None,
            resets_at: Some(Utc::now() + chrono::Duration::hours(3)),
            model_id: model.map(str::to_owned),
            model_display_name: None,
        }
    }

    fn caps(entries: &[(&str, SoftLimit)]) -> SoftLimits {
        SoftLimits { limits: entries.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect() }
    }

    fn pct(cap: i32) -> SoftLimit {
        SoftLimit { cap_pct: Some(cap), cap_usd: None, bypass_minutes: None, pace_cap: None }
    }

    #[test]
    fn an_allowing_decision_serializes_as_just_allow() {
        let v = serde_json::to_value(DecisionView::from(Decision::Allow)).unwrap();
        assert_eq!(v, serde_json::json!({ "allow": true }));
    }

    #[test]
    fn a_block_carries_the_retry_the_reason_and_the_key() {
        let v = DecisionView::from(Decision::Block {
            retry_after_secs: 5400,
            reason: "cctui soft limit: weekly at 99%".to_owned(),
            key: "weekly_model:fable".to_owned(),
        });
        assert!(!v.allow);
        assert_eq!(v.retry_after_secs, Some(5400));
        assert_eq!(v.key.as_deref(), Some("weekly_model:fable"));
    }

    #[test]
    fn the_models_in_play_come_from_the_model_scoped_windows_plus_the_current_one() {
        let windows = vec![
            window(soft_limit::KEY_SESSION, 40.0, None),
            window("weekly_model:fable", 99.0, Some("claude-fable-5-1")),
            window("weekly_model:opus", 10.0, Some("claude-opus-5")),
        ];
        assert_eq!(
            models_in_play(&windows, Some("claude-sonnet-5")),
            vec!["claude-fable-5-1", "claude-opus-5", "claude-sonnet-5"],
        );
        assert_eq!(
            models_in_play(&windows, None),
            vec!["claude-fable-5-1", "claude-opus-5"],
            "a session with no model still reports the models the caps can block"
        );
        assert!(models_in_play(&[], Some("  ")).is_empty());
    }

    #[test]
    fn a_per_model_entry_matches_what_evaluate_soft_limit_would_decide() {
        let windows = vec![
            window("weekly_model:fable", 99.0, Some("claude-fable-5-1")),
            window("weekly_model:opus", 10.0, Some("claude-opus-5")),
        ];
        let caps = caps(&[("weekly_model:fable", pct(90)), ("weekly_model:opus", pct(90))]);
        let now = Utc::now();
        for model in models_in_play(&windows, None) {
            let direct = soft_limit::evaluate_soft_limit(&windows, &caps, Some(&model), now);
            let view = DecisionView::from(direct.clone());
            assert_eq!(view.allow, direct == Decision::Allow, "{model}");
        }
        let fable = DecisionView::from(soft_limit::evaluate_soft_limit(
            &windows,
            &caps,
            Some("claude-fable-5-1"),
            now,
        ));
        let opus = DecisionView::from(soft_limit::evaluate_soft_limit(
            &windows,
            &caps,
            Some("claude-opus-5"),
            now,
        ));
        assert!(!fable.allow, "a model over its own weekly cap is blocked");
        assert!(opus.allow, "a model well under its cap is not");
    }

    #[test]
    fn a_child_budget_becomes_the_session_usd_cap() {
        let account = caps(&[(soft_limit::KEY_SESSION, pct(90))]);
        let merged = gateway::merge_session_budget(&account, Some(20.0));
        assert_eq!(
            merged.limits[soft_limit::KEY_SESSION_USD].cap_usd,
            Some(20.0),
            "the CctuiAgent budget must surface as the session_usd cap"
        );
        assert_eq!(
            merged.limits[soft_limit::KEY_SESSION].cap_pct,
            Some(90),
            "and must not disturb the account's own caps"
        );
        let unbudgeted = gateway::merge_session_budget(&account, None);
        assert!(!unbudgeted.limits.contains_key(soft_limit::KEY_SESSION_USD));
    }

    #[test]
    fn an_empty_usage_cache_allows_rather_than_refusing() {
        let caps = caps(&[(soft_limit::KEY_SESSION, pct(90))]);
        let decision =
            DecisionView::from(soft_limit::evaluate_soft_limit(&[], &caps, None, Utc::now()));
        assert!(
            decision.allow,
            "no cached usage must read as allowed: a refusal would be taken for a block"
        );
    }

    #[tokio::test]
    async fn a_session_without_a_gateway_token_has_no_binding() {
        let Some(url) = gateway::test_db_url("a_session_without_a_gateway_token_has_no_binding")
        else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");
        let uid = Uuid::new_v4();
        let session = format!("limits-untokened-{uid}");
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("limits-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, status) \
             VALUES ($1, 'm1', '/w', $2, 'active')",
        )
        .bind(&session)
        .bind(uid)
        .execute(&pool)
        .await
        .expect("seed session");

        let found: Option<(Uuid,)> =
            sqlx::query_as("SELECT st.account_id FROM session_tokens st WHERE st.session_id = $1")
                .bind(&session)
                .fetch_optional(&pool)
                .await
                .expect("query");
        assert!(found.is_none(), "an unbound session resolves to no account — the route 404s");

        sqlx::query("DELETE FROM sessions WHERE id = $1").bind(&session).execute(&pool).await.ok();
        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }

    #[tokio::test]
    async fn resolution_returns_the_pinned_account_not_the_owners_other_credentials() {
        let Some(url) = gateway::test_db_url(
            "resolution_returns_the_pinned_account_not_the_owners_other_credentials",
        ) else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("limits-owner-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");

        // Two accounts the same human owns. The session is pinned to the
        // second; the first must never surface.
        let mut providers = Vec::new();
        for label in ["other", "pinned"] {
            let acct = Uuid::new_v4();
            let prov = Uuid::new_v4();
            sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
                .bind(acct)
                .bind(uid)
                .bind(format!("{label}-{uid}"))
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
            providers.push((label, acct, prov));
        }
        let (_, pinned_acct, pinned_prov) = providers[1];

        let session = format!("limits-pinned-{uid}");
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, status) \
             VALUES ($1, 'm1', '/w', $2, 'active')",
        )
        .bind(&session)
        .bind(uid)
        .execute(&pool)
        .await
        .expect("seed session");
        sqlx::query(
            "INSERT INTO session_tokens (token_hash, session_id, account_id) VALUES ($1, $2, $3)",
        )
        .bind(format!("th-{session}"))
        .bind(&session)
        .bind(pinned_prov)
        .execute(&pool)
        .await
        .expect("seed token");

        let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT st.account_id, a.id, a.name FROM session_tokens st \
             JOIN account_providers ap ON ap.id = st.account_id \
             JOIN accounts a ON a.id = ap.account_id \
             WHERE st.session_id = $1 \
             ORDER BY (st.revoked_at IS NULL) DESC, st.created_at DESC LIMIT 1",
        )
        .bind(&session)
        .fetch_optional(&pool)
        .await
        .expect("query");
        let (provider_id, account_id, name) = row.expect("the pinned binding resolves");
        assert_eq!(provider_id, pinned_prov);
        assert_eq!(account_id, pinned_acct);
        assert!(name.starts_with("pinned-"), "the owner's other account must not surface: {name}");

        sqlx::query("DELETE FROM session_tokens WHERE session_id = $1")
            .bind(&session)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM sessions WHERE id = $1").bind(&session).execute(&pool).await.ok();
        for (_, acct, prov) in providers {
            sqlx::query("DELETE FROM account_providers WHERE id = $1")
                .bind(prov)
                .execute(&pool)
                .await
                .ok();
            sqlx::query("DELETE FROM accounts WHERE id = $1").bind(acct).execute(&pool).await.ok();
        }
        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }
}
