//! Keeps the quota history filled while nobody looks at a gauge: every
//! [`SAMPLE_EVERY`] each usage-bearing credential is read through the same
//! rate-limited path the soft limit uses (which appends the samples), rows
//! past retention are pruned, and the windows an agent reports on its own
//! (codex) are recorded against the credential its session is bound to.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use cctui_proto::adapter::RateLimitWindow;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::soft_limit::{KEY_SESSION, KEY_WEEKLY_ALL, UsageWindow};
use crate::state::AppState;

/// An agent reading older than this is a replay, not the current state.
const AGENT_READING_MAX_AGE: chrono::Duration = chrono::Duration::minutes(10);

const SAMPLE_EVERY: Duration = Duration::from_mins(5);
const PRUNE_EVERY: Duration = Duration::from_hours(1);

static LAST_SAMPLE: Mutex<Option<Instant>> = Mutex::new(None);
static LAST_PRUNE: Mutex<Option<Instant>> = Mutex::new(None);

fn claim(slot: &Mutex<Option<Instant>>, every: Duration, now: Instant) -> bool {
    let mut last = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.is_some_and(|at| now.duration_since(at) < every) {
        return false;
    }
    *last = Some(now);
    true
}

/// Called from the reaper tick; does the work in the background when due.
pub fn sweep(state: &AppState) {
    let now = Instant::now();
    if claim(&LAST_SAMPLE, SAMPLE_EVERY, now) {
        let state = state.clone();
        tokio::spawn(async move { sample_all(&state).await });
    }
    if claim(&LAST_PRUNE, PRUNE_EVERY, now) {
        let pool = state.pool.clone();
        tokio::spawn(async move {
            crate::store::usage_samples::prune(&pool, chrono::Utc::now()).await;
        });
    }
}

async fn sample_all(state: &AppState) {
    let ids: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT id FROM account_providers WHERE provider IN ('anthropic', 'openai')",
    )
    .fetch_all(&state.pool)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "listing credentials for usage sampling failed");
            return;
        }
    };
    for id in ids {
        crate::routes::gateway::usage_for_soft_limit(state, id).await;
    }
}

fn agent_window_key(w: &RateLimitWindow) -> Option<(&'static str, &'static str)> {
    let session = (KEY_SESSION, "5h");
    let weekly = (KEY_WEEKLY_ALL, "Weekly (all models)");
    match (w.window_minutes, w.slot.as_str()) {
        (Some(300), _) | (None, "primary") => Some(session),
        (Some(10_080), _) | (None, "secondary") => Some(weekly),
        _ => None,
    }
}

/// The agent-reported windows worth recording: current (not a replay), with a
/// known length and a reset still ahead.
pub fn agent_windows(
    windows: &[RateLimitWindow],
    observed_at: Option<i64>,
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    if let Some(at) = observed_at.and_then(|s| DateTime::from_timestamp(s, 0))
        && now - at > AGENT_READING_MAX_AGE
    {
        return Vec::new();
    }
    windows
        .iter()
        .filter_map(|w| {
            let (key, label) = agent_window_key(w)?;
            let resets_at = DateTime::from_timestamp(w.resets_at?, 0).filter(|r| *r > now)?;
            Some(UsageWindow {
                key: key.to_owned(),
                kind: key.to_owned(),
                label: label.to_owned(),
                utilization: w.used_percent,
                amount_usd: None,
                resets_at: Some(resets_at),
                model_id: None,
                model_display_name: None,
            })
        })
        .collect()
}

/// Record an agent's own rate-limit report, off the event path.
pub fn record_agent_limits(
    state: &AppState,
    session_id: String,
    windows: &[RateLimitWindow],
    observed_at: Option<i64>,
) {
    let now = Utc::now();
    let windows = agent_windows(windows, observed_at, now);
    if windows.is_empty() || session_id.is_empty() {
        return;
    }
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let provider: Option<Uuid> = match sqlx::query_scalar(
            "SELECT account_id FROM session_tokens WHERE session_id = $1 \
             ORDER BY (revoked_at IS NULL) DESC, created_at DESC LIMIT 1",
        )
        .bind(&session_id)
        .fetch_optional(&pool)
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%session_id, error = %e, "resolving credential for rate limits failed");
                return;
            }
        };
        if let Some(provider_id) = provider {
            crate::store::usage_samples::record(&pool, provider_id, &windows, now, "codex_event")
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(slot: &str, minutes: Option<i64>, resets_at: Option<i64>) -> RateLimitWindow {
        RateLimitWindow {
            used_percent: 42.0,
            window_minutes: minutes,
            resets_at,
            slot: slot.into(),
        }
    }

    #[test]
    fn agent_windows_map_to_the_canonical_keys() {
        let now = DateTime::from_timestamp(1_780_000_000, 0).unwrap();
        let later = Some(1_780_003_600);
        let out = agent_windows(
            &[
                rl("primary", Some(300), later),
                rl("secondary", None, later),
                rl("primary", Some(60), later),
            ],
            None,
            now,
        );
        let keys: Vec<&str> = out.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["session", "weekly_all"]);
        assert!((out[0].utilization - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn replayed_or_expired_agent_readings_are_dropped() {
        let now = DateTime::from_timestamp(1_780_000_000, 0).unwrap();
        let w = [rl("primary", Some(300), Some(1_780_003_600))];
        assert!(agent_windows(&w, Some(1_780_000_000 - 3600), now).is_empty());
        assert_eq!(agent_windows(&w, Some(1_780_000_000 - 60), now).len(), 1);
        assert!(
            agent_windows(&[rl("primary", Some(300), Some(1_779_999_000))], None, now).is_empty()
        );
        assert!(agent_windows(&[rl("primary", Some(300), None)], None, now).is_empty());
    }

    #[test]
    fn claim_fires_once_per_period() {
        let slot = Mutex::new(None);
        let t0 = Instant::now();
        assert!(claim(&slot, SAMPLE_EVERY, t0));
        assert!(!claim(&slot, SAMPLE_EVERY, t0 + Duration::from_mins(1)));
        assert!(claim(&slot, SAMPLE_EVERY, t0 + SAMPLE_EVERY));
    }
}
