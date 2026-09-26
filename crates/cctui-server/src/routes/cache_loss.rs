//! Dollars lost to prompt-cache busts per local day and reason, recomputed
//! from `session_token_usage` with the same per-turn verdict the conversation
//! view shows.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use cctui_proto::api::ApiError;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::AuthContext;
use crate::cache_bust::{Reason, Turn, compute, ttl_window};
use crate::state::AppState;

const MAX_DAYS: i64 = 90;

#[derive(Debug, Deserialize)]
pub struct CacheLossParams {
    #[serde(default = "default_days")]
    pub days: i64,
    /// `Date.getTimezoneOffset()`: minutes to subtract from UTC for local time.
    #[serde(default)]
    pub tz_offset: i32,
}

const fn default_days() -> i64 {
    14
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct DailyCacheLoss {
    /// Local calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub ttl_expired: f64,
    pub gateway_rewrote_body: f64,
    pub unknown: f64,
    pub total: f64,
    pub busts: u64,
}

/// Sum each bust's `lost_usd` into its local day and reason, oldest day first.
pub fn aggregate(
    busts: impl IntoIterator<Item = (DateTime<Utc>, Reason, f64)>,
    tz_offset: i32,
) -> Vec<DailyCacheLoss> {
    let mut days: BTreeMap<NaiveDate, DailyCacheLoss> = BTreeMap::new();
    for (at, reason, usd) in busts {
        let day = (at - Duration::minutes(i64::from(tz_offset))).date_naive();
        let d = days.entry(day).or_default();
        match reason {
            Reason::TtlExpired => d.ttl_expired += usd,
            Reason::GatewayRewroteBody => d.gateway_rewrote_body += usd,
            Reason::Unknown => d.unknown += usd,
        }
        d.total += usd;
        d.busts += 1;
    }
    days.into_iter()
        .map(|(day, d)| DailyCacheLoss { day: day.format("%Y-%m-%d").to_string(), ..d })
        .collect()
}

type UsageRow = (String, String, Option<String>, i64, i64, i64, bool, DateTime<Utc>);

/// `GET /sessions/stats/cache-busts?days=&tz_offset=`
pub async fn cache_loss(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<CacheLossParams>,
) -> Result<Json<Vec<DailyCacheLoss>>, (StatusCode, Json<ApiError>)> {
    let since = Utc::now() - Duration::days(params.days.clamp(1, MAX_DAYS));
    // Turns just before the range are the predecessors its first turns are
    // judged against.
    let rows: Vec<UsageRow> = sqlx::query_as(
        "SELECT stu.session_id, stu.message_id, stu.model, stu.input_tokens, \
                stu.cache_read_tokens, stu.cache_creation_tokens, stu.gateway_rewrote_body, \
                stu.created_at \
         FROM session_token_usage stu \
         LEFT JOIN sessions s ON s.id = stu.session_id \
         LEFT JOIN machines m ON m.id = s.machine_uuid \
         WHERE stu.created_at >= $1 AND ($2::uuid IS NULL OR m.user_id = $2)",
    )
    .bind(since - ttl_window() - Duration::minutes(5))
    .bind(ctx.owner_filter())
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error (cache loss): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;

    let mut by_session: HashMap<String, Vec<Turn>> = HashMap::new();
    for (session_id, message_id, model, input, cache_read, cache_creation, rewrote, at) in rows {
        by_session.entry(session_id).or_default().push(Turn {
            message_id,
            model,
            input,
            cache_read,
            cache_creation,
            created_at: at,
            gateway_rewrote_body: rewrote,
        });
    }
    let ids: Vec<String> = by_session.keys().cloned().collect();
    let catalogs = crate::routes::sessions::session_catalogs(&state, &ids).await;

    let mut busts = Vec::new();
    for (session_id, turns) in &by_session {
        let at: HashMap<&str, DateTime<Utc>> =
            turns.iter().map(|t| (t.message_id.as_str(), t.created_at)).collect();
        for (message_id, bust) in compute(turns, catalogs.get(session_id)) {
            if let Some(&when) = at.get(message_id.as_str())
                && when >= since
            {
                busts.push((when, bust.reason, bust.lost_usd));
            }
        }
    }
    Ok(Json(aggregate(busts, params.tz_offset)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn busts_sum_per_day_and_reason() {
        let out = aggregate(
            [
                (t("2026-09-20T10:00:00Z"), Reason::TtlExpired, 1.5),
                (t("2026-09-20T11:00:00Z"), Reason::GatewayRewroteBody, 0.25),
                (t("2026-09-20T12:00:00Z"), Reason::TtlExpired, 0.5),
                (t("2026-09-19T12:00:00Z"), Reason::Unknown, 2.0),
            ],
            0,
        );
        assert_eq!(
            out,
            vec![
                DailyCacheLoss {
                    day: "2026-09-19".into(),
                    unknown: 2.0,
                    total: 2.0,
                    busts: 1,
                    ..Default::default()
                },
                DailyCacheLoss {
                    day: "2026-09-20".into(),
                    ttl_expired: 2.0,
                    gateway_rewrote_body: 0.25,
                    total: 2.25,
                    busts: 3,
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn days_are_local_to_the_callers_offset() {
        // UTC+9 reports -540: 20:00Z on the 19th is already the 20th locally.
        let out = aggregate([(t("2026-09-19T20:00:00Z"), Reason::Unknown, 1.0)], -540);
        assert_eq!(out[0].day, "2026-09-20");
        let out = aggregate([(t("2026-09-20T02:00:00Z"), Reason::Unknown, 1.0)], 300);
        assert_eq!(out[0].day, "2026-09-19");
    }

    #[test]
    fn no_busts_is_an_empty_series() {
        assert!(aggregate([], 0).is_empty());
    }

    #[test]
    fn serializes_one_field_per_reason() {
        let v =
            serde_json::to_value(DailyCacheLoss { day: "2026-09-20".into(), ..Default::default() })
                .unwrap();
        for k in ["day", "ttl_expired", "gateway_rewrote_body", "unknown", "total", "busts"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
    }
}
