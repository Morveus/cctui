use std::collections::HashSet;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

use cctui_proto::api::{
    ApiError, HeatmapCell, ModelUsage, SessionStats, TokenUsageWindows, UsageAnalytics,
    UsageBucket, WindowTokenUsage,
};
use cctui_proto::models::SessionStatus;

use crate::auth::AuthContext;
use crate::live_sessions::live_sessions_predicate;
use crate::routes::sessions::{attention_from_bucket, bucket_from_signals, derive_status};
use crate::state::AppState;

/// One row of the per-session classification signals
/// (`tempo`, `agent_state`, `activity`, `soft_limit_reason`).
type SignalRow = (Option<String>, Option<String>, Option<String>, Option<String>);

/// Query params for `GET /sessions/recent-dirs` — the last working dirs used
/// on a given machine, for the spawn working-directory picker.
#[derive(Debug, Default, Deserialize)]
pub struct RecentDirsParams {
    pub machine_id: Option<String>,
}

/// `GET /sessions/recent-dirs?machine_id=…` → up to 5 distinct working dirs
/// most recently used on that machine (most-recent first). With no
/// `machine_id`, returns the most recent dirs across all machines.
pub async fn recent_dirs(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<RecentDirsParams>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ApiError>)> {
    // Scope to the caller's own sessions (admin sees all) via the
    // machine_uuid -> machines.user_id join, bound to owner_filter().
    let uid = ctx.owner_filter();
    let rows: Vec<(String,)> = match params.machine_id.as_deref() {
        Some(machine_id) => {
            sqlx::query_as(
                // normalize trailing slashes (keep root '/') so
                // `folder` and `folder/` collapse to one entry.
                "SELECT CASE WHEN s.working_dir ~ '^/+$' THEN '/' \
                    ELSE rtrim(s.working_dir, '/') END AS dir FROM sessions s \
             LEFT JOIN machines m ON m.id = s.machine_uuid \
             WHERE s.machine_id = $1 AND s.working_dir <> '' \
             AND ($2::uuid IS NULL OR m.user_id = $2) \
             GROUP BY dir \
             ORDER BY MAX(s.registered_at) DESC LIMIT 5",
            )
            .bind(machine_id)
            .bind(uid)
            .fetch_all(&state.pool)
            .await
        }
        None => {
            sqlx::query_as(
                // normalize trailing slashes (keep root '/') so
                // `folder` and `folder/` collapse to one entry.
                "SELECT CASE WHEN s.working_dir ~ '^/+$' THEN '/' \
                    ELSE rtrim(s.working_dir, '/') END AS dir FROM sessions s \
             LEFT JOIN machines m ON m.id = s.machine_uuid \
             WHERE s.working_dir <> '' \
             AND ($1::uuid IS NULL OR m.user_id = $1) \
             GROUP BY dir \
             ORDER BY MAX(s.registered_at) DESC LIMIT 5",
            )
            .bind(uid)
            .fetch_all(&state.pool)
            .await
        }
    }
    .map_err(|e| {
        tracing::error!("db error (recent dirs): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    Ok(Json(rows.into_iter().map(|(d,)| d).collect()))
}

/// IANA timezone from the browser; unknown or missing zones fall back to UTC.
#[derive(Debug, Default, Deserialize)]
pub struct SessionStatsParams {
    pub timezone: Option<String>,
}

// Calendar arithmetic happens before conversion to UTC, preserving DST boundaries.
// MATERIALIZED is load-bearing: inlined, the planner re-scans pg_timezone_names
// once per reference (eight times, 23-65 ms each).
const SESSION_COUNTS_SQL: &str = "
    WITH zone AS MATERIALIZED (
        SELECT COALESCE((SELECT name FROM pg_timezone_names WHERE name = $2), 'UTC') AS tz
    ), boundaries AS MATERIALIZED (
        SELECT
            date_trunc('day', $3::timestamptz AT TIME ZONE tz) AT TIME ZONE tz AS today,
            (date_trunc('day', $3::timestamptz AT TIME ZONE tz) - interval '1 day') AT TIME ZONE tz AS yesterday,
            date_trunc('week', $3::timestamptz AT TIME ZONE tz) AT TIME ZONE tz AS week,
            date_trunc('month', $3::timestamptz AT TIME ZONE tz) AT TIME ZONE tz AS month
        FROM zone
    )
    SELECT COUNT(*), COUNT(*) FILTER (WHERE s.status = 'archived'),
        COUNT(*) FILTER (WHERE s.registered_at >= b.today AND s.registered_at <= $3),
        COUNT(*) FILTER (WHERE s.registered_at >= b.yesterday AND s.registered_at < b.today),
        COUNT(*) FILTER (WHERE s.registered_at >= b.week AND s.registered_at <= $3),
        COUNT(*) FILTER (WHERE s.registered_at >= b.month AND s.registered_at <= $3)
    FROM sessions s LEFT JOIN machines m ON m.id = s.machine_uuid
    CROSS JOIN boundaries b
    WHERE ($1::uuid IS NULL OR m.user_id = $1)
";

/// `GET /sessions/stats` — aggregate session counts for the Overview page.
///
/// The session list is capped (`LIMIT 25`), so counting client-side over it
/// undercounts once there are more than 25 sessions. This computes the totals
/// straight from SQL aggregates (`total`, `archived`) and the live registry
/// (`live`), and counts `needs_input` by running the classifier over every
/// non-archived session's persisted signals.
pub async fn session_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<SessionStatsParams>,
) -> Result<Json<SessionStats>, (StatusCode, Json<ApiError>)> {
    let db_err = |e: sqlx::Error| {
        tracing::error!("db error (session stats): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    };
    let uid = ctx.owner_filter();

    // All counts scoped to the caller (NULL = admin sees all) via the
    // machine_uuid -> machines.user_id join.
    let (total, archived, today, yesterday, week, month): (i64, i64, i64, i64, i64, i64) =
        sqlx::query_as(SESSION_COUNTS_SQL)
            .bind(uid)
            .bind(params.timezone.as_deref().unwrap_or("UTC"))
            .bind(Utc::now())
            .fetch_one(&state.pool)
            .await
            .map_err(db_err)?;

    // Live = sessions currently in the registry whose derived status is
    // active/new (matches how the list surfaces "live"). Scope to the caller's
    // owned live ids for non-admins (resolved from the DB, like list_sessions).
    let owned_live_ids: Option<HashSet<String>> = if ctx.is_admin() {
        None
    } else {
        let live_ids: Vec<String> = {
            let registry = state.registry.read().await;
            registry.list().into_iter().map(|h| h.session.id.clone()).collect()
        };
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT s.id FROM sessions s \
             LEFT JOIN machines m ON m.id = s.machine_uuid \
             WHERE s.id = ANY($1) AND m.user_id = $2",
        )
        .bind(&live_ids)
        .bind(ctx.user_id)
        .fetch_all(&state.pool)
        .await
        .map_err(db_err)?;
        Some(rows.into_iter().map(|(id,)| id).collect())
    };
    let live: i64 = {
        let registry = state.registry.read().await;
        registry
            .list()
            .into_iter()
            .filter(|h| owned_live_ids.as_ref().is_none_or(|owned| owned.contains(&h.session.id)))
            .filter(|h| {
                matches!(
                    derive_status(h.session.registered_at, h.session.last_heartbeat),
                    SessionStatus::Active | SessionStatus::New
                )
            })
            .count()
            .try_into()
            .unwrap_or(i64::MAX)
    };

    // needs_input: classify every non-archived session from its persisted
    // signals and count the Blocked bucket — scoped to the caller.
    let signal_rows: Vec<SignalRow> = sqlx::query_as(concat!(
        "SELECT s.tempo, s.agent_state, s.activity, s.soft_limit_reason \
                 FROM sessions s LEFT JOIN machines m ON m.id = s.machine_uuid \
                 WHERE ",
        live_sessions_predicate!("s"),
        " AND ($1::uuid IS NULL OR m.user_id = $1)"
    ))
    .bind(uid)
    .fetch_all(&state.pool)
    .await
    .map_err(db_err)?;
    let needs_input: i64 = signal_rows
        .into_iter()
        .filter(|(tempo, agent_state, activity, soft_limit_reason)| {
            attention_from_bucket(bucket_from_signals(
                tempo.as_deref(),
                agent_state.as_deref(),
                activity.as_deref(),
                soft_limit_reason.as_deref(),
                &[],
                &std::collections::HashMap::new(),
            ))
            .is_some()
        })
        .count()
        .try_into()
        .unwrap_or(i64::MAX);

    Ok(Json(SessionStats { total, live, needs_input, archived, today, yesterday, week, month }))
}

/// Query params for `GET /sessions/stats/tokens`. `tz_offset` is the caller's
/// `Date.getTimezoneOffset()` (minutes; positive west of UTC), used only to
/// anchor the calendar "today" window to local midnight. Defaults to UTC.
#[derive(Debug, Default, Deserialize)]
pub struct TokenStatsParams {
    #[serde(default)]
    pub tz_offset: i32,
}

/// UTC instant of the most recent local midnight, given the caller's
/// `Date.getTimezoneOffset()` value. JS reports local = UTC − offset, so the
/// UTC instant of a local wall-clock time L is `L + offset`.
fn day_start_for_offset(now: DateTime<Utc>, tz_offset_minutes: i32) -> DateTime<Utc> {
    let offset = Duration::minutes(i64::from(tz_offset_minutes));
    let local_now = now - offset;
    // Truncate the local wall-clock time to midnight, then map back to UTC.
    let local_midnight =
        local_now.date_naive().and_hms_opt(0, 0, 0).unwrap_or_else(|| local_now.naive_utc());
    DateTime::<Utc>::from_naive_utc_and_offset(local_midnight, Utc) + offset
}

/// `GET /sessions/stats/tokens` — token totals across rolling time windows for
/// the Overview. One scan of `session_token_usage` with per-window conditional
/// aggregates. Each window reports the same three figures the session card
/// shows (`↑input ↓output ⚡cache_read`). Global, like `session_stats`.
pub async fn session_token_stats(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<TokenStatsParams>,
) -> Result<Json<TokenUsageWindows>, (StatusCode, Json<ApiError>)> {
    type Row = (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64);

    let uid = ctx.owner_filter();
    let now = Utc::now();
    let hour = now - Duration::hours(1);
    let today = day_start_for_offset(now, params.tz_offset);
    let day = now - Duration::hours(24);
    let week = now - Duration::days(7);
    let month = now - Duration::days(30);

    // 15 conditional sums (5 windows × 3 metrics) in one pass. COALESCE keeps
    // every column a non-null bigint even when no rows match the window.
    // The columns read here are the INCLUDE list of
    // `idx_session_token_usage_created_covering`: reading one more drops the
    // scan off index-only and back onto ~92,000 buffers of heap.
    let r: Row = sqlx::query_as(
        "SELECT \
            COALESCE(SUM(input_tokens)       FILTER (WHERE created_at >= $1), 0)::bigint, \
            COALESCE(SUM(output_tokens)      FILTER (WHERE created_at >= $1), 0)::bigint, \
            COALESCE(SUM(cache_read_tokens)  FILTER (WHERE created_at >= $1), 0)::bigint, \
            COALESCE(SUM(input_tokens)       FILTER (WHERE created_at >= $2), 0)::bigint, \
            COALESCE(SUM(output_tokens)      FILTER (WHERE created_at >= $2), 0)::bigint, \
            COALESCE(SUM(cache_read_tokens)  FILTER (WHERE created_at >= $2), 0)::bigint, \
            COALESCE(SUM(input_tokens)       FILTER (WHERE created_at >= $3), 0)::bigint, \
            COALESCE(SUM(output_tokens)      FILTER (WHERE created_at >= $3), 0)::bigint, \
            COALESCE(SUM(cache_read_tokens)  FILTER (WHERE created_at >= $3), 0)::bigint, \
            COALESCE(SUM(input_tokens)       FILTER (WHERE created_at >= $4), 0)::bigint, \
            COALESCE(SUM(output_tokens)      FILTER (WHERE created_at >= $4), 0)::bigint, \
            COALESCE(SUM(cache_read_tokens)  FILTER (WHERE created_at >= $4), 0)::bigint, \
            COALESCE(SUM(input_tokens)       FILTER (WHERE created_at >= $5), 0)::bigint, \
            COALESCE(SUM(output_tokens)      FILTER (WHERE created_at >= $5), 0)::bigint, \
            COALESCE(SUM(cache_read_tokens)  FILTER (WHERE created_at >= $5), 0)::bigint \
         FROM session_token_usage stu \
         LEFT JOIN sessions s ON s.id = stu.session_id \
         LEFT JOIN machines m ON m.id = s.machine_uuid \
         WHERE stu.created_at >= $5 AND ($6::uuid IS NULL OR m.user_id = $6)",
    )
    .bind(hour)
    .bind(today)
    .bind(day)
    .bind(week)
    .bind(month)
    .bind(uid)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error (token stats): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;

    let cast = |v: i64| u64::try_from(v).unwrap_or(0);
    let win = |i: usize, o: usize, c: usize, t: &Row| WindowTokenUsage {
        input: cast(field(t, i)),
        output: cast(field(t, o)),
        cache_read: cast(field(t, c)),
    };
    Ok(Json(TokenUsageWindows {
        hour: win(0, 1, 2, &r),
        today: win(3, 4, 5, &r),
        day: win(6, 7, 8, &r),
        week: win(9, 10, 11, &r),
        month: win(12, 13, 14, &r),
    }))
}

/// The 15-column token-stats row (the `query_as` row above), indexed
/// positionally by [`field`] to keep the window construction terse without a
/// 15-field named struct.
type TokenStatsRow = (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64);

/// Index a 15-tuple positionally (the `query_as` row above). Keeps the window
/// construction terse without a 15-field named struct.
const fn field(t: &TokenStatsRow, i: usize) -> i64 {
    match i {
        0 => t.0,
        1 => t.1,
        2 => t.2,
        3 => t.3,
        4 => t.4,
        5 => t.5,
        6 => t.6,
        7 => t.7,
        8 => t.8,
        9 => t.9,
        10 => t.10,
        11 => t.11,
        12 => t.12,
        13 => t.13,
        _ => t.14,
    }
}

/// Query params for `GET /sessions/stats/usage`. `days` is the reporting range
/// (clamped 1..=365, default 30); `tz_offset` is the caller's
/// `Date.getTimezoneOffset()` (minutes; positive west of UTC), used to anchor
/// day buckets and hour-of-week extraction to the caller's local wall clock —
/// consistent with how [`session_token_stats`] anchors `today`.
#[derive(Debug, Deserialize)]
pub struct UsageAnalyticsParams {
    #[serde(default = "default_days")]
    pub days: i64,
    #[serde(default)]
    pub tz_offset: i32,
}

const fn default_days() -> i64 {
    30
}

/// Bucket granularity for a range: per-hour for short ranges (last 24-48h),
/// per-day otherwise. Kept pure for unit testing.
const fn granularity_for_days(days: i64) -> &'static str {
    if days <= 2 { "hour" } else { "day" }
}

type BucketRow = (DateTime<Utc>, i64, i64, i64, i64);
type ModelRow = (String, i64, i64, i64, i64);
type HeatRow = (i32, i32, i64, i64);

/// Tokens-over-time aggregate. `$1` granularity (`day`/`hour`), `$2` tz-offset
/// minutes, `$3` window start, `$4` owner filter (NULL = all). Shared with the
/// aggregation test so both exercise the exact same SQL.
const USAGE_BUCKETS_SQL: &str = "SELECT \
        date_trunc($1, stu.created_at - make_interval(mins => $2)) \
            + make_interval(mins => $2) AS bucket, \
        COALESCE(SUM(stu.input_tokens), 0)::bigint, \
        COALESCE(SUM(stu.output_tokens), 0)::bigint, \
        COALESCE(SUM(stu.cache_read_tokens), 0)::bigint, \
        COALESCE(SUM(stu.cache_creation_tokens), 0)::bigint \
     FROM session_token_usage stu \
     LEFT JOIN sessions s ON s.id = stu.session_id \
     LEFT JOIN machines m ON m.id = s.machine_uuid \
     WHERE stu.created_at >= $3 AND ($4::uuid IS NULL OR m.user_id = $4) \
     GROUP BY bucket ORDER BY bucket";

/// Per-model breakdown. `$1` window start, `$2` owner filter (NULL = all).
const USAGE_MODELS_SQL: &str = "SELECT \
        COALESCE(NULLIF(stu.model, ''), NULLIF(s.model, ''), 'unknown') AS model, \
        COALESCE(SUM(stu.input_tokens), 0)::bigint, \
        COALESCE(SUM(stu.output_tokens), 0)::bigint, \
        COALESCE(SUM(stu.cache_read_tokens), 0)::bigint, \
        COUNT(*)::bigint \
     FROM session_token_usage stu \
     LEFT JOIN sessions s ON s.id = stu.session_id \
     LEFT JOIN machines m ON m.id = s.machine_uuid \
     WHERE stu.created_at >= $1 AND ($2::uuid IS NULL OR m.user_id = $2) \
     GROUP BY 1 ORDER BY SUM(stu.output_tokens) DESC NULLS LAST";

/// Hour-of-week heatmap. `$1` tz-offset minutes, `$2` window start, `$3` owner
/// filter (NULL = all).
const USAGE_HEATMAP_SQL: &str = "SELECT \
        EXTRACT(dow  FROM stu.created_at - make_interval(mins => $1))::int, \
        EXTRACT(hour FROM stu.created_at - make_interval(mins => $1))::int, \
        COUNT(*)::bigint, \
        COALESCE(SUM(stu.output_tokens), 0)::bigint \
     FROM session_token_usage stu \
     LEFT JOIN sessions s ON s.id = stu.session_id \
     LEFT JOIN machines m ON m.id = s.machine_uuid \
     WHERE stu.created_at >= $2 AND ($3::uuid IS NULL OR m.user_id = $3) \
     GROUP BY 1, 2";

/// `GET /sessions/stats/usage?days=30` — Overview usage analytics:
/// tokens-over-time buckets, per-model breakdown, and an hour-of-week activity
/// heatmap. One round-trip set (three aggregate scans of `session_token_usage`,
/// no per-bucket queries). Scoped to the caller like `session_token_stats`.
///
/// Bucketing and hour-of-week extraction are done in the caller's reporting
/// timezone: `created_at` is shifted by `tz_offset` to local wall-clock time
/// before `date_trunc`/`EXTRACT`, then day buckets are mapped back to a UTC
/// instant (same convention as `today` in [`session_token_stats`]).
///
/// Model attribution prefers the per-request `session_token_usage.model` and
/// falls back to the session's own, via the PK join `sessions.id =
/// stu.session_id`; NULL/empty models bucket under `unknown`. Missing time
/// buckets are zero-filled client-side, not in SQL.
pub async fn session_usage_analytics(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<UsageAnalyticsParams>,
) -> Result<Json<UsageAnalytics>, (StatusCode, Json<ApiError>)> {
    let uid = ctx.owner_filter();
    let days = params.days.clamp(1, 365);
    let tz = params.tz_offset;
    let granularity = granularity_for_days(days);
    let since = Utc::now() - Duration::days(days);

    let bucket_rows: Vec<BucketRow> = sqlx::query_as(USAGE_BUCKETS_SQL)
        .bind(granularity)
        .bind(tz)
        .bind(since)
        .bind(uid)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| usage_db_err(&e))?;
    let model_rows: Vec<ModelRow> = sqlx::query_as(USAGE_MODELS_SQL)
        .bind(since)
        .bind(uid)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| usage_db_err(&e))?;
    let heat_rows: Vec<HeatRow> = sqlx::query_as(USAGE_HEATMAP_SQL)
        .bind(tz)
        .bind(since)
        .bind(uid)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| usage_db_err(&e))?;

    let cast = |v: i64| u64::try_from(v).unwrap_or(0);
    let buckets = bucket_rows
        .into_iter()
        .map(|(bucket, input, output, cache_read, cache_creation)| UsageBucket {
            bucket: bucket.to_rfc3339(),
            input: cast(input),
            output: cast(output),
            cache_read: cast(cache_read),
            cache_creation: cast(cache_creation),
        })
        .collect();
    let models = model_rows
        .into_iter()
        .map(|(model, input, output, cache_read, messages)| ModelUsage {
            model,
            input: cast(input),
            output: cast(output),
            cache_read: cast(cache_read),
            messages: cast(messages),
        })
        .collect();
    let heatmap = heat_rows
        .into_iter()
        .map(|(dow, hour, messages, output)| HeatmapCell {
            dow: dow as u8,
            hour: hour as u8,
            messages: cast(messages),
            output: cast(output),
        })
        .collect();

    Ok(Json(UsageAnalytics { granularity: granularity.into(), buckets, models, heatmap }))
}

fn usage_db_err(e: &sqlx::Error) -> (StatusCode, Json<ApiError>) {
    tracing::error!("db error (usage analytics): {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
}

#[cfg(test)]
mod tests {
    use super::{day_start_for_offset, granularity_for_days};
    use chrono::{DateTime, Duration, TimeZone, Utc};

    #[test]
    fn the_timezone_ctes_stay_materialized() {
        for cte in ["WITH zone AS MATERIALIZED (", "), boundaries AS MATERIALIZED ("] {
            assert!(
                super::SESSION_COUNTS_SQL.contains(cte),
                "`{cte}` was dropped: pg_timezone_names goes back to eight scans per call, which \
                 no correctness test can see"
            );
        }
    }

    #[tokio::test]
    async fn session_calendar_counts_over_db() {
        let Some(url) = crate::routes::gateway::test_db_url("session_calendar_counts_over_db")
        else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.expect("test database");
        let mut tx = pool.begin().await.unwrap();
        // Temporary tables isolate the exact production query from shared fixtures.
        sqlx::raw_sql(
            "CREATE TEMP TABLE machines (id uuid, user_id uuid) ON COMMIT DROP;
             CREATE TEMP TABLE sessions (machine_uuid uuid, status text, registered_at timestamptz) ON COMMIT DROP;
             INSERT INTO machines VALUES
                ('00000000-0000-0000-0000-000000000001', '00000000-0000-0000-0000-000000000011'),
                ('00000000-0000-0000-0000-000000000002', '00000000-0000-0000-0000-000000000022');
             INSERT INTO sessions
                SELECT '00000000-0000-0000-0000-000000000001'::uuid, 'archived', '2026-10-26 00:00Z'::timestamptz
                FROM generate_series(1, 30);
             INSERT INTO sessions VALUES
                ('00000000-0000-0000-0000-000000000001', 'active', '2026-10-24 22:00Z'),
                ('00000000-0000-0000-0000-000000000001', 'active', '2026-10-25 22:59:59Z'),
                ('00000000-0000-0000-0000-000000000001', 'active', '2026-09-30 22:00Z'),
                ('00000000-0000-0000-0000-000000000001', 'active', '2026-09-30 21:59:59Z'),
                ('00000000-0000-0000-0000-000000000002', 'active', '2026-10-26 01:00Z');"
        ).execute(&mut *tx).await.unwrap();
        let owner = uuid::Uuid::from_u128(17);
        let now = Utc.with_ymd_and_hms(2026, 10, 26, 12, 0, 0).unwrap();
        for (uid, timezone, expected) in [
            // Monday following the DST change: yesterday lasted 25 hours.
            (Some(owner), "Europe/Paris", (34, 30, 30, 2, 30, 33)),
            (None, "Europe/Paris", (35, 30, 31, 2, 31, 34)),
            (Some(owner), "UTC", (34, 30, 30, 1, 30, 32)),
            (Some(owner), "unknown/timezone", (34, 30, 30, 1, 30, 32)),
            (Some(uuid::Uuid::from_u128(99)), "Europe/Paris", (0, 0, 0, 0, 0, 0)),
        ] {
            let counts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(super::SESSION_COUNTS_SQL)
                .bind(uid)
                .bind(timezone)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(counts, expected, "timezone={timezone}, owner={uid:?}");
        }
        // New year, month and week boundaries are independent calendar periods.
        sqlx::raw_sql(
            "TRUNCATE sessions; INSERT INTO sessions VALUES
            (NULL, 'archived', '2026-12-31 23:00Z'),
            (NULL, 'active', '2026-12-31 22:59:59Z'),
            (NULL, 'active', '2026-12-27 23:00Z'),
            (NULL, 'active', '2026-12-27 22:59:59Z');",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        let counts: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(super::SESSION_COUNTS_SQL)
            .bind(None::<uuid::Uuid>)
            .bind("Europe/Paris")
            .bind(Utc.with_ymd_and_hms(2027, 1, 1, 12, 0, 0).unwrap())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(counts, (4, 1, 1, 1, 3, 1));
        tx.rollback().await.unwrap();
    }

    #[test]
    fn granularity_hourly_for_short_ranges() {
        assert_eq!(granularity_for_days(1), "hour");
        assert_eq!(granularity_for_days(2), "hour");
        assert_eq!(granularity_for_days(3), "day");
        assert_eq!(granularity_for_days(30), "day");
    }

    // SQL aggregation test: needs a migrated Postgres. Point
    // DATABASE_URL/TEST_DATABASE_URL at one and it runs; otherwise it skips.
    // Exercises the exact handler query strings (the USAGE_*_SQL consts) for
    // day bucketing, per-model grouping (incl. NULL model → 'unknown'), and
    // hour-of-week heatmap extraction.
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn usage_aggregation_over_db() {
        use super::{USAGE_BUCKETS_SQL, USAGE_HEATMAP_SQL, USAGE_MODELS_SQL};
        use sqlx::Row as _;

        let Some(url) = crate::routes::gateway::test_db_url("usage_aggregation_over_db") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let prefix = "cct707-agg";
        let cleanup = |p: sqlx::PgPool| async move {
            sqlx::query("DELETE FROM sessions WHERE id LIKE $1")
                .bind(format!("{prefix}-%"))
                .execute(&p)
                .await
                .expect("cleanup");
        };
        cleanup(pool.clone()).await;

        let seed = |n: i64,
                    model: Option<&'static str>,
                    at: DateTime<Utc>,
                    input: i64,
                    output: i64,
                    cache_read: i64,
                    cache_creation: i64| {
            let pool = pool.clone();
            async move {
                let sid = format!("{prefix}-s{n}");
                sqlx::query(
                    "INSERT INTO sessions (id, machine_id, working_dir, status, model) \
                     VALUES ($1, 'test-machine', '/tmp', 'active', $2) \
                     ON CONFLICT (id) DO UPDATE SET model = EXCLUDED.model",
                )
                .bind(&sid)
                .bind(model)
                .execute(&pool)
                .await
                .expect("insert session");
                sqlx::query(
                    "INSERT INTO session_token_usage (session_id, message_id, input_tokens, \
                     output_tokens, cache_read_tokens, cache_creation_tokens, created_at) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(&sid)
                .bind(format!("{prefix}-m{n}"))
                .bind(input)
                .bind(output)
                .bind(cache_read)
                .bind(cache_creation)
                .bind(at)
                .execute(&pool)
                .await
                .expect("insert usage");
            }
        };

        let now = Utc::now();
        // Anchor at midday UTC so the +2h sibling row can't cross a UTC date
        // boundary (near-midnight runs split the bucket otherwise).
        let midday = |d: DateTime<Utc>| {
            d.date_naive().and_hms_opt(12, 0, 0).expect("valid midday").and_utc()
        };
        let day_a = midday(now - Duration::days(3));
        let day_a2 = day_a + Duration::hours(2);
        let day_b = midday(now - Duration::days(5));
        // Two rows on day_a (one claude, one NULL model → 'unknown'), one on day_b.
        seed(1, Some("claude-x"), day_a, 100, 10, 5, 1).await;
        seed(2, None, day_a2, 200, 20, 0, 0).await;
        seed(3, Some("claude-x"), day_b, 50, 5, 2, 0).await;

        let since = now - Duration::days(30);
        let no_uid = Option::<uuid::Uuid>::None;

        // Buckets: day_a merges its two rows (100+200), day_b holds 50. Other
        // rows in a shared DB are ignored by matching our known dates + totals.
        let rows = sqlx::query(USAGE_BUCKETS_SQL)
            .bind("day")
            .bind(0_i32)
            .bind(since)
            .bind(no_uid)
            .fetch_all(&pool)
            .await
            .expect("buckets query");
        let (mut a_in, mut b_in) = (0i64, 0i64);
        for r in &rows {
            let bucket: DateTime<Utc> = r.get(0);
            let input: i64 = r.get(1);
            if bucket.date_naive() == day_a.date_naive() {
                a_in += input;
            } else if bucket.date_naive() == day_b.date_naive() {
                b_in += input;
            }
        }
        assert!(a_in >= 300, "day_a bucket must include our 300 input, got {a_in}");
        assert!(b_in >= 50, "day_b bucket must include our 50 input, got {b_in}");

        // Models: claude-x present with >=2 messages, NULL grouped under 'unknown'.
        let rows = sqlx::query(USAGE_MODELS_SQL)
            .bind(since)
            .bind(no_uid)
            .fetch_all(&pool)
            .await
            .expect("models query");
        let (mut claude_msgs, mut unknown_msgs) = (0i64, 0i64);
        for r in &rows {
            let model: String = r.get(0);
            let messages: i64 = r.get(4);
            match model.as_str() {
                "claude-x" => claude_msgs += messages,
                "unknown" => unknown_msgs += messages,
                _ => {}
            }
        }
        assert!(claude_msgs >= 2, "claude-x should have >=2 messages, got {claude_msgs}");
        assert!(unknown_msgs >= 1, "NULL model must bucket under 'unknown'");

        // Heatmap: a cell exists at day_a's (dow, hour).
        let rows = sqlx::query(USAGE_HEATMAP_SQL)
            .bind(0_i32)
            .bind(since)
            .bind(no_uid)
            .fetch_all(&pool)
            .await
            .expect("heatmap query");
        let want_dow: i32 = day_a.format("%w").to_string().parse().unwrap();
        let want_hour: i32 = day_a.format("%H").to_string().parse().unwrap();
        let hit = rows.iter().any(|r| {
            let dow: i32 = r.get(0);
            let hour: i32 = r.get(1);
            let messages: i64 = r.get(2);
            dow == want_dow && hour == want_hour && messages >= 1
        });
        assert!(hit, "heatmap should have a cell at dow={want_dow} hour={want_hour}");

        cleanup(pool.clone()).await;
    }

    #[test]
    fn day_start_utc_when_no_offset() {
        // 2026-06-11T09:30Z with offset 0 → midnight the same UTC day.
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 9, 30, 0).unwrap();
        let start = day_start_for_offset(now, 0);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap());
    }

    #[test]
    fn day_start_uses_local_midnight_west_of_utc() {
        // tz_offset +480 (UTC−8). At 2026-06-11T05:00Z it's still 2026-06-10
        // 21:00 locally, so "today" started at 2026-06-10T08:00Z (local midnight).
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 5, 0, 0).unwrap();
        let start = day_start_for_offset(now, 480);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 6, 10, 8, 0, 0).unwrap());
    }

    #[test]
    fn day_start_uses_local_midnight_east_of_utc() {
        // tz_offset −120 (UTC+2, Europe/Paris summer). At 2026-06-11T01:00Z it's
        // already 03:00 on the 11th locally, so local midnight was 2026-06-10T22:00Z.
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 1, 0, 0).unwrap();
        let start = day_start_for_offset(now, -120);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 6, 10, 22, 0, 0).unwrap());
        // Sanity: the boundary is in the past relative to `now`.
        let _: DateTime<Utc> = start;
        assert!(start < now);
    }
}
