//! The quota time series behind the real pace: one row per credential, per
//! window, per reading (see migrations 106 and 126).
//!
//! [`record`] appends the windows of a fresh reading and records the close of
//! any window instance that reading shows has reset; it never fails the
//! caller, because losing a sample costs a slightly worse rate later, while
//! failing a usage read would blank a gauge now. [`previous_for`] finds the
//! reading a two-point slope should rate against: old enough for the integer
//! percentages upstream to have moved, and in the same window instance, so a
//! reset in between never reads as negative growth.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::pace::{Sample, window_duration};
use crate::soft_limit::UsageWindow;

/// Youngest a previous sample may be for a slope to mean anything: upstream
/// percentages are integers, and on a weekly window one point is ~1h40 of
/// even spend, so a shorter base reads as zero or as a one-point jump.
pub const SLOPE_MIN_AGE: Duration = Duration::hours(2);

/// How long samples and window closes are kept (pruned by the reaper).
pub const RETENTION_DAYS: i64 = 90;

/// Minimum spacing between two samples of the same window, whichever writer
/// (poll, sampler, codex events, another replica) produced them.
pub const MIN_SAMPLE_SPACING: Duration = Duration::minutes(4);

/// A utilization fall this large across a passed `resets_at` is a reset even
/// when upstream reports the same `resets_at` again.
const RESET_DROP_POINTS: f64 = 30.0;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PreviousSample {
    pub window_key: String,
    pub utilization: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub sampled_at: DateTime<Utc>,
}

impl PreviousSample {
    pub const fn sample(&self) -> Sample {
        Sample { at: self.sampled_at, utilization: self.utilization }
    }
}

/// Whether a window is worth sampling: dollar windows have no percentage to
/// rate, and a window with no reset has no length to rate against.
fn samplable(w: &UsageWindow) -> bool {
    w.kind != "usd" && !w.key.starts_with("usd_") && w.resets_at.is_some()
}

/// The newest stored sample of one window.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LastSample {
    pub utilization: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub sampled_at: DateTime<Utc>,
}

/// A window instance that ended, measured from its last sample.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowClose {
    pub resets_at: DateTime<Utc>,
    pub final_utilization: f64,
    pub wasted_pct: f64,
}

/// Whether a new sample may be written given the newest stored one.
pub fn due(last: Option<&LastSample>, now: DateTime<Utc>) -> bool {
    last.is_none_or(|l| now - l.sampled_at >= MIN_SAMPLE_SPACING)
}

/// The close of `last`'s window instance, if `current` belongs to a later one:
/// its `resets_at` moved forward by more than a minute, or utilization fell by more than
/// [`RESET_DROP_POINTS`] after `last`'s reset time passed.
pub fn detect_close(
    last: &LastSample,
    current: &UsageWindow,
    now: DateTime<Utc>,
) -> Option<WindowClose> {
    let prev_reset = last.resets_at?;
    let moved = current.resets_at.is_some_and(|r| (r - prev_reset).num_seconds() > 60);
    let dropped = prev_reset <= now && last.utilization - current.utilization > RESET_DROP_POINTS;
    (moved || dropped).then(|| close_of(prev_reset, last.utilization))
}

fn close_of(resets_at: DateTime<Utc>, final_utilization: f64) -> WindowClose {
    WindowClose {
        resets_at,
        final_utilization,
        wasted_pct: (100.0 - final_utilization).clamp(0.0, 100.0),
    }
}

async fn last_sample(
    pool: &sqlx::PgPool,
    provider_id: Uuid,
    window_key: &str,
) -> Result<Option<LastSample>, sqlx::Error> {
    sqlx::query_as(
        "SELECT utilization, resets_at, sampled_at FROM account_usage_samples \
          WHERE provider_id = $1 AND window_key = $2 ORDER BY sampled_at DESC LIMIT 1",
    )
    .bind(provider_id)
    .bind(window_key)
    .fetch_optional(pool)
    .await
}

async fn insert_close(
    pool: &sqlx::PgPool,
    provider_id: Uuid,
    window_key: &str,
    close: &WindowClose,
    source: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO account_window_closes \
             (provider_id, window_key, resets_at, final_utilization, wasted_pct, source) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(provider_id)
    .bind(window_key)
    .bind(close.resets_at)
    .bind(close.final_utilization)
    .bind(close.wasted_pct)
    .bind(source)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Record one reading per samplable window: close the previous window instance
/// when this reading belongs to a new one, then append a sample unless one
/// younger than [`MIN_SAMPLE_SPACING`] exists (checked again in SQL, so
/// concurrent replicas cannot both insert). Best-effort by contract: errors are
/// logged, never returned.
pub async fn record(
    pool: &sqlx::PgPool,
    provider_id: Uuid,
    windows: &[UsageWindow],
    now: DateTime<Utc>,
    source: &str,
) {
    for w in windows.iter().filter(|w| samplable(w)) {
        if let Err(e) = record_one(pool, provider_id, w, now, source).await {
            tracing::warn!(
                provider_id = %provider_id, key = %w.key, error = %e,
                "recording usage sample failed"
            );
            return;
        }
    }
}

async fn record_one(
    pool: &sqlx::PgPool,
    provider_id: Uuid,
    w: &UsageWindow,
    now: DateTime<Utc>,
    source: &str,
) -> Result<(), sqlx::Error> {
    let last = last_sample(pool, provider_id, &w.key).await?;
    if let Some(close) = last.as_ref().and_then(|l| detect_close(l, w, now)) {
        insert_close(pool, provider_id, &w.key, &close, source).await?;
    }
    if !due(last.as_ref(), now) {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO account_usage_samples \
             (provider_id, window_key, utilization, amount_usd, resets_at, sampled_at, source) \
         SELECT $1, $2, $3, $4, $5, $6, $7 \
          WHERE NOT EXISTS (SELECT 1 FROM account_usage_samples \
                             WHERE provider_id = $1 AND window_key = $2 AND sampled_at > $8) \
         ON CONFLICT DO NOTHING",
    )
    .bind(provider_id)
    .bind(&w.key)
    .bind(w.utilization)
    .bind(w.amount_usd)
    .bind(w.resets_at)
    .bind(now)
    .bind(source)
    .bind(now - MIN_SAMPLE_SPACING)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Drop samples and closes older than [`RETENTION_DAYS`].
pub async fn prune(pool: &sqlx::PgPool, now: DateTime<Utc>) {
    let cutoff = now - Duration::days(RETENTION_DAYS);
    for sql in [
        "DELETE FROM account_usage_samples WHERE sampled_at < $1",
        "DELETE FROM account_window_closes WHERE closed_at < $1",
    ] {
        if let Err(e) = sqlx::query(sql).bind(cutoff).execute(pool).await {
            tracing::warn!(error = %e, "pruning usage history failed");
        }
    }
}

/// One stored sample, as the history route returns it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HistoryRow {
    pub window_key: String,
    pub utilization: f64,
    pub amount_usd: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub sampled_at: DateTime<Utc>,
    pub source: String,
}

pub async fn history(
    pool: &sqlx::PgPool,
    provider_id: Uuid,
    window_key: Option<&str>,
    from: DateTime<Utc>,
) -> Result<Vec<HistoryRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT window_key, utilization, amount_usd, resets_at, sampled_at, source \
           FROM account_usage_samples \
          WHERE provider_id = $1 AND ($2::text IS NULL OR window_key = $2) AND sampled_at >= $3 \
          ORDER BY window_key, sampled_at",
    )
    .bind(provider_id)
    .bind(window_key)
    .bind(from)
    .fetch_all(pool)
    .await
}

/// One closed window instance.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CloseRow {
    pub provider_id: Uuid,
    pub window_key: String,
    pub resets_at: DateTime<Utc>,
    pub final_utilization: f64,
    pub wasted_pct: f64,
    pub closed_at: DateTime<Utc>,
    pub source: String,
}

/// Closes since `from` for the given credentials, newest first.
pub async fn closes(
    pool: &sqlx::PgPool,
    provider_ids: &[Uuid],
    window_key: Option<&str>,
    from: DateTime<Utc>,
) -> Result<Vec<CloseRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT provider_id, window_key, resets_at, final_utilization, wasted_pct, closed_at, source \
           FROM account_window_closes \
          WHERE provider_id = ANY($1) AND ($2::text IS NULL OR window_key = $2) AND resets_at >= $3 \
          ORDER BY resets_at DESC",
    )
    .bind(provider_ids)
    .bind(window_key)
    .bind(from)
    .fetch_all(pool)
    .await
}

/// For each of `windows`, the newest sample at least [`SLOPE_MIN_AGE`] old
/// that belongs to the same window instance (`resets_at` within a minute) and
/// is no older than the window's own length. One query per credential; a
/// window with no qualifying sample is simply absent from the result.
pub async fn previous_for(
    exec: impl PgExecutor<'_>,
    provider_id: Uuid,
    windows: &[UsageWindow],
    now: DateTime<Utc>,
) -> Result<Vec<PreviousSample>, sqlx::Error> {
    let cutoff = now - SLOPE_MIN_AGE;
    // No window is longer than a week, so this only trims the scan; the
    // per-window bound is applied in `matches_window`.
    let oldest = now - Duration::days(7);
    let rows: Vec<PreviousSample> = sqlx::query_as(
        "SELECT DISTINCT ON (window_key) window_key, utilization, resets_at, sampled_at \
           FROM account_usage_samples \
          WHERE provider_id = $1 AND sampled_at <= $2 AND sampled_at >= $3 \
          ORDER BY window_key, sampled_at DESC",
    )
    .bind(provider_id)
    .bind(cutoff)
    .bind(oldest)
    .fetch_all(exec)
    .await?;
    Ok(rows.into_iter().filter(|r| matches_window(r, windows, now)).collect())
}

/// Same window instance and within the window's length. Pure so the match
/// rule is testable without a database.
pub fn matches_window(row: &PreviousSample, windows: &[UsageWindow], now: DateTime<Utc>) -> bool {
    let Some(w) = windows.iter().find(|w| w.key == row.window_key) else {
        return false;
    };
    let Some(len) = window_duration(&w.key) else {
        return false;
    };
    if now - row.sampled_at > len {
        return false;
    }
    match (row.resets_at, w.resets_at) {
        (Some(a), Some(b)) => (a - b).num_seconds().abs() < 60,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn window(key: &str, resets: &str) -> UsageWindow {
        UsageWindow {
            key: key.into(),
            kind: "weekly_all".into(),
            label: "7d".into(),
            utilization: 50.0,
            amount_usd: None,
            resets_at: Some(t(resets)),
            model_id: None,
            model_display_name: None,
        }
    }

    fn row(key: &str, resets: &str, at: &str) -> PreviousSample {
        PreviousSample {
            window_key: key.into(),
            utilization: 40.0,
            resets_at: Some(t(resets)),
            sampled_at: t(at),
        }
    }

    #[test]
    fn same_window_instance_matches() {
        let now = t("2026-09-11T12:00:00Z");
        let w = [window("weekly_all", "2026-09-15T04:00:00.362Z")];
        let r = row("weekly_all", "2026-09-15T04:00:00Z", "2026-09-11T09:00:00Z");
        assert!(matches_window(&r, &w, now));
    }

    #[test]
    fn a_reset_in_between_does_not_match() {
        let now = t("2026-09-11T12:00:00Z");
        let w = [window("weekly_all", "2026-09-15T04:00:00Z")];
        let r = row("weekly_all", "2026-09-08T04:00:00Z", "2026-09-11T09:00:00Z");
        assert!(!matches_window(&r, &w, now));
    }

    #[test]
    fn older_than_the_window_does_not_match() {
        let now = t("2026-09-11T12:00:00Z");
        let w = [window("session", "2026-09-11T13:00:00Z")];
        let r = row("session", "2026-09-11T13:00:00Z", "2026-09-11T06:00:00Z");
        assert!(!matches_window(&r, &w, now));
    }

    #[test]
    fn unknown_key_does_not_match() {
        let now = t("2026-09-11T12:00:00Z");
        let w = [window("weekly_all", "2026-09-15T04:00:00Z")];
        let r = row("session", "2026-09-15T04:00:00Z", "2026-09-11T09:00:00Z");
        assert!(!matches_window(&r, &w, now));
    }

    #[test]
    fn dollar_and_resetless_windows_are_not_sampled() {
        let mut usd = window("usd_7d", "2026-09-15T04:00:00Z");
        usd.kind = "usd".into();
        assert!(!samplable(&usd));
        let mut no_reset = window("weekly_all", "2026-09-15T04:00:00Z");
        no_reset.resets_at = None;
        assert!(!samplable(&no_reset));
        assert!(samplable(&window("weekly_all", "2026-09-15T04:00:00Z")));
    }

    fn last(util: f64, resets: &str, at: &str) -> LastSample {
        LastSample { utilization: util, resets_at: Some(t(resets)), sampled_at: t(at) }
    }

    #[test]
    fn a_moved_reset_closes_the_previous_window() {
        let now = t("2026-09-11T12:02:00Z");
        let l = last(72.0, "2026-09-11T12:00:00Z", "2026-09-11T11:58:00Z");
        let mut w = window("session", "2026-09-11T17:00:00Z");
        w.utilization = 1.0;
        let close = detect_close(&l, &w, now).unwrap();
        assert_eq!(close.resets_at, t("2026-09-11T12:00:00Z"));
        assert!((close.final_utilization - 72.0).abs() < f64::EPSILON);
        assert!((close.wasted_pct - 28.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_same_window_instance_does_not_close() {
        let now = t("2026-09-11T11:00:00Z");
        let l = last(40.0, "2026-09-11T12:00:00Z", "2026-09-11T10:55:00Z");
        let w = window("session", "2026-09-11T12:00:00.400Z");
        assert!(detect_close(&l, &w, now).is_none());
    }

    #[test]
    fn a_large_drop_after_a_passed_reset_closes_even_with_the_same_resets_at() {
        let now = t("2026-09-11T12:05:00Z");
        let l = last(90.0, "2026-09-11T12:00:00Z", "2026-09-11T11:59:00Z");
        let mut w = window("session", "2026-09-11T12:00:00Z");
        w.utilization = 2.0;
        assert!(detect_close(&l, &w, now).is_some());
    }

    #[test]
    fn a_large_drop_before_the_reset_passed_is_not_a_close() {
        let now = t("2026-09-11T11:00:00Z");
        let l = last(90.0, "2026-09-11T12:00:00Z", "2026-09-11T10:59:00Z");
        let mut w = window("session", "2026-09-11T12:00:00Z");
        w.utilization = 2.0;
        assert!(detect_close(&l, &w, now).is_none());
    }

    #[test]
    fn an_older_reading_never_closes_the_current_window() {
        let now = t("2026-09-11T12:02:00Z");
        let l = last(40.0, "2026-09-11T17:00:00Z", "2026-09-11T12:00:00Z");
        let mut w = window("session", "2026-09-11T12:00:00Z");
        w.utilization = 90.0;
        assert!(detect_close(&l, &w, now).is_none());
    }

    #[test]
    fn overage_never_reports_negative_waste() {
        let now = t("2026-09-11T12:02:00Z");
        let l = last(104.0, "2026-09-11T12:00:00Z", "2026-09-11T11:58:00Z");
        let w = window("session", "2026-09-11T17:00:00Z");
        assert!(detect_close(&l, &w, now).unwrap().wasted_pct.abs() < f64::EPSILON);
    }

    #[test]
    fn samples_are_throttled_to_the_minimum_spacing() {
        let now = t("2026-09-11T12:00:00Z");
        assert!(due(None, now));
        assert!(!due(Some(&last(1.0, "2026-09-11T17:00:00Z", "2026-09-11T11:57:00Z")), now));
        assert!(due(Some(&last(1.0, "2026-09-11T17:00:00Z", "2026-09-11T11:56:00Z")), now));
    }
}
