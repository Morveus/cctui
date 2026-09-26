//! Read side of the quota history: raw samples per credential, and the closed
//! window instances with how much of each went unused.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::routes::accounts::{err, require_human};
use crate::state::AppState;
use crate::store::usage_samples::{self, CloseRow, HistoryRow, RETENTION_DAYS};

type ApiErr = (StatusCode, Json<serde_json::Value>);

const DEFAULT_LOOKBACK: Duration = Duration::days(7);

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub window: Option<String>,
    pub from: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct HistorySample {
    pub window_key: String,
    pub utilization: f64,
    pub amount_usd: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub sampled_at: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct UsageHistory {
    pub account_id: Uuid,
    pub samples: Vec<HistorySample>,
}

#[derive(Debug, Serialize)]
pub struct WindowClose {
    pub account_id: Uuid,
    pub window_key: String,
    pub resets_at: DateTime<Utc>,
    pub final_utilization: f64,
    pub wasted_pct: f64,
    pub closed_at: DateTime<Utc>,
    pub source: String,
}

/// Mean unused share of the closed instances of one window key.
#[derive(Debug, Serialize, PartialEq)]
pub struct WastedSummary {
    pub window_key: String,
    pub windows: usize,
    pub mean_wasted_pct: f64,
}

#[derive(Debug, Serialize)]
pub struct WindowCloses {
    pub closes: Vec<WindowClose>,
    pub summary: Vec<WastedSummary>,
}

fn clamp_from(from: Option<DateTime<Utc>>, now: DateTime<Utc>) -> DateTime<Utc> {
    from.unwrap_or(now - DEFAULT_LOOKBACK).max(now - Duration::days(RETENTION_DAYS))
}

fn db_err(e: &sqlx::Error) -> ApiErr {
    tracing::error!("db error (usage history): {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
}

async fn owned_provider_ids(
    state: &AppState,
    ctx: &AuthContext,
    only: Option<Uuid>,
) -> Result<Vec<Uuid>, ApiErr> {
    sqlx::query_scalar(
        "SELECT id FROM account_providers \
         WHERE ($1::uuid IS NULL OR user_id = $1) AND ($2::uuid IS NULL OR id = $2)",
    )
    .bind(ctx.owner_filter())
    .bind(only)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_err(&e))
}

pub fn history_sample(r: HistoryRow) -> HistorySample {
    HistorySample {
        window_key: r.window_key,
        utilization: r.utilization,
        amount_usd: r.amount_usd,
        resets_at: r.resets_at,
        sampled_at: r.sampled_at,
        source: r.source,
    }
}

pub fn window_close(r: CloseRow) -> WindowClose {
    WindowClose {
        account_id: r.provider_id,
        window_key: r.window_key,
        resets_at: r.resets_at,
        final_utilization: r.final_utilization,
        wasted_pct: r.wasted_pct,
        closed_at: r.closed_at,
        source: r.source,
    }
}

/// Per window key, how many instances closed and their mean unused share.
pub fn summarize(closes: &[WindowClose]) -> Vec<WastedSummary> {
    let mut by_key: BTreeMap<&str, (usize, f64)> = BTreeMap::new();
    for c in closes {
        let e = by_key.entry(c.window_key.as_str()).or_default();
        e.0 += 1;
        e.1 += c.wasted_pct;
    }
    by_key
        .into_iter()
        .map(|(key, (n, sum))| WastedSummary {
            window_key: key.to_owned(),
            windows: n,
            mean_wasted_pct: sum / n as f64,
        })
        .collect()
}

/// `GET /accounts/{id}/usage/history?window=&from=`
pub async fn account_usage_history(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<UsageHistory>, ApiErr> {
    require_human(&ctx)?;
    if owned_provider_ids(&state, &ctx, Some(id)).await?.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    let from = clamp_from(q.from, Utc::now());
    let rows = usage_samples::history(&state.pool, id, q.window.as_deref(), from)
        .await
        .map_err(|e| db_err(&e))?;
    Ok(Json(UsageHistory {
        account_id: id,
        samples: rows.into_iter().map(history_sample).collect(),
    }))
}

async fn closes_for(
    state: &AppState,
    ids: &[Uuid],
    q: &HistoryQuery,
) -> Result<Json<WindowCloses>, ApiErr> {
    let from = clamp_from(q.from, Utc::now());
    let rows = usage_samples::closes(&state.pool, ids, q.window.as_deref(), from)
        .await
        .map_err(|e| db_err(&e))?;
    let closes: Vec<WindowClose> = rows.into_iter().map(window_close).collect();
    let summary = summarize(&closes);
    Ok(Json(WindowCloses { closes, summary }))
}

/// `GET /accounts/{id}/usage/closes?window=&from=`
pub async fn account_usage_closes(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<WindowCloses>, ApiErr> {
    require_human(&ctx)?;
    let ids = owned_provider_ids(&state, &ctx, Some(id)).await?;
    if ids.is_empty() {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    closes_for(&state, &ids, &q).await
}

/// `GET /accounts/usage/closes?window=&from=`: every credential the caller owns.
pub async fn all_usage_closes(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<WindowCloses>, ApiErr> {
    require_human(&ctx)?;
    let ids = owned_provider_ids(&state, &ctx, None).await?;
    closes_for(&state, &ids, &q).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn close(key: &str, wasted: f64) -> WindowClose {
        WindowClose {
            account_id: Uuid::nil(),
            window_key: key.into(),
            resets_at: t("2026-09-11T12:00:00Z"),
            final_utilization: 100.0 - wasted,
            wasted_pct: wasted,
            closed_at: t("2026-09-11T12:04:00Z"),
            source: "poll".into(),
        }
    }

    #[test]
    fn summary_averages_waste_per_window_key() {
        let s =
            summarize(&[close("session", 40.0), close("session", 20.0), close("weekly_all", 12.0)]);
        assert_eq!(
            s,
            vec![
                WastedSummary { window_key: "session".into(), windows: 2, mean_wasted_pct: 30.0 },
                WastedSummary {
                    window_key: "weekly_all".into(),
                    windows: 1,
                    mean_wasted_pct: 12.0
                },
            ]
        );
        assert!(summarize(&[]).is_empty());
    }

    #[test]
    fn from_defaults_to_a_week_and_never_reaches_past_retention() {
        let now = t("2026-09-24T00:00:00Z");
        assert_eq!(clamp_from(None, now), now - Duration::days(7));
        assert_eq!(clamp_from(Some(t("2020-01-01T00:00:00Z")), now), now - Duration::days(90));
    }

    #[test]
    fn closes_serialize_with_the_documented_shape() {
        let body = serde_json::to_value(WindowCloses {
            closes: vec![close("session", 25.0)],
            summary: vec![WastedSummary {
                window_key: "session".into(),
                windows: 1,
                mean_wasted_pct: 25.0,
            }],
        })
        .unwrap();
        let c = &body["closes"][0];
        for k in [
            "account_id",
            "window_key",
            "resets_at",
            "final_utilization",
            "wasted_pct",
            "closed_at",
            "source",
        ] {
            assert!(c.get(k).is_some(), "missing {k}");
        }
        assert_eq!(body["summary"][0]["mean_wasted_pct"], 25.0);
    }

    #[test]
    fn history_serializes_with_the_documented_shape() {
        let body = serde_json::to_value(UsageHistory {
            account_id: Uuid::nil(),
            samples: vec![history_sample(HistoryRow {
                window_key: "session".into(),
                utilization: 12.0,
                amount_usd: None,
                resets_at: Some(t("2026-09-11T12:00:00Z")),
                sampled_at: t("2026-09-11T10:00:00Z"),
                source: "codex_event".into(),
            })],
        })
        .unwrap();
        let s = &body["samples"][0];
        assert_eq!(s["source"], "codex_event");
        assert_eq!(s["resets_at"], "2026-09-11T12:00:00Z");
        assert!(s["amount_usd"].is_null());
    }
}
