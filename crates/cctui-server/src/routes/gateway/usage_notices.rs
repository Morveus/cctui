//! Usage ticker: a message delivered to the session whenever one of the
//! account's usage windows crossed a new `step_pct` bucket since the last
//! notice. Off by default.
//!
//! Delivery rides the session-messaging path, never the proxied request body:
//! re-serializing a body in flight reorders every JSON object's keys, which
//! busts the prompt cache for the whole history.

use std::fmt::Write as _;

use chrono::{DateTime, Datelike, Utc};

use super::{Account, session_id_for_token, usage_for_soft_limit};
use crate::soft_limit::{SoftLimits, UsageWindow};
use crate::state::AppState;

pub const DEFAULT_STEP_PCT: u32 = 10;

/// `{ "enabled": bool, "step_pct": int }` on the provider row. NULL ⇒ off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageNotices {
    pub enabled: bool,
    pub step_pct: u32,
}

impl Default for UsageNotices {
    fn default() -> Self {
        Self { enabled: false, step_pct: DEFAULT_STEP_PCT }
    }
}

impl UsageNotices {
    pub fn from_json(value: Option<&serde_json::Value>) -> Self {
        let obj = value.and_then(serde_json::Value::as_object);
        let enabled = obj
            .and_then(|o| o.get("enabled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let step_pct = obj
            .and_then(|o| o.get("step_pct"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (1..=100).contains(n))
            .unwrap_or(DEFAULT_STEP_PCT);
        Self { enabled, step_pct }
    }

    /// Validate a PATCH/create payload into the stored blob; `Ok(None)` clears the
    /// column (off).
    pub fn build_json(
        value: Option<&serde_json::Value>,
    ) -> Result<Option<serde_json::Value>, String> {
        let Some(v) = value.filter(|v| !v.is_null()) else { return Ok(None) };
        let Some(obj) = v.as_object() else { return Err("usage_notices must be an object".into()) };
        let enabled = match obj.get("enabled") {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => return Err("usage_notices.enabled must be a boolean".into()),
        };
        let step_pct = match obj.get("step_pct") {
            None | Some(serde_json::Value::Null) => DEFAULT_STEP_PCT,
            Some(n) => n
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=100).contains(n))
                .ok_or_else(|| "usage_notices.step_pct must be an integer in 1..=100".to_owned())?,
        };
        if !enabled && step_pct == DEFAULT_STEP_PCT {
            return Ok(None);
        }
        Ok(Some(serde_json::json!({ "enabled": enabled, "step_pct": step_pct })))
    }
}

pub fn bucket(utilization: f64, step_pct: u32) -> u32 {
    let step = f64::from(step_pct.max(1));
    (utilization.max(0.0) / step).floor() as u32
}

/// What a window's current bucket owes, given the step last notified for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAction {
    /// Climbed into a new bucket: notify, then record this step.
    Notify(u32),
    /// Dropped (window reset) or first sighting below the first step: record
    /// only, so the next climb notifies again.
    Record(u32),
    Unchanged,
}

pub const fn step_action(last: Option<u32>, now: u32) -> StepAction {
    match last {
        Some(l) if now > l => StepAction::Notify(now),
        Some(l) if now < l => StepAction::Record(now),
        Some(_) => StepAction::Unchanged,
        None if now > 0 => StepAction::Notify(now),
        None => StepAction::Record(0),
    }
}

/// Each percent window paired with the action its current utilization owes.
pub fn window_actions<'a>(
    last_steps: &std::collections::HashMap<String, u32>,
    windows: &'a [UsageWindow],
    step_pct: u32,
) -> Vec<(&'a UsageWindow, StepAction)> {
    windows
        .iter()
        .filter(|w| w.amount_usd.is_none())
        .map(|w| (w, step_action(last_steps.get(&w.key).copied(), bucket(w.utilization, step_pct))))
        .collect()
}

fn fmt_countdown(secs: i64) -> String {
    let mins = secs.max(0) / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h{:02}", hours, mins % 60);
    }
    format!("{}d{}h", hours / 24, hours % 24)
}

fn fmt_reset(resets_at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let clock = if resets_at.date_naive() == now.date_naive() {
        resets_at.format("%H:%M UTC").to_string()
    } else {
        format!("{} {}", resets_at.weekday(), resets_at.format("%H:%M UTC"))
    };
    format!("resets {clock} in {}", fmt_countdown((resets_at - now).num_seconds()))
}

/// One line per percent window, e.g.
/// `Account usage notice: 5h window at 60 % (soft limit 98 %), resets 01:30 UTC in 1h12. Weekly (all models) at 15 %, resets Mon 09:00 UTC in 3d4h.`
pub fn notice_text(windows: &[UsageWindow], limits: &SoftLimits, now: DateTime<Utc>) -> String {
    let mut out = String::from("Account usage notice:");
    for w in windows.iter().filter(|w| w.amount_usd.is_none()) {
        let name =
            if w.kind == "session" { format!("{} window", w.label) } else { w.label.clone() };
        let _ = write!(out, " {name} at {} %", w.utilization.round() as i64);
        if let Some(cap) = limits.limits.get(&w.key).and_then(|l| l.cap_pct) {
            let _ = write!(out, " (soft limit {cap} %)");
        }
        if let Some(r) = w.resets_at {
            let _ = write!(out, ", {}", fmt_reset(r, now));
        }
        out.push('.');
    }
    out
}

async fn last_steps(
    pool: &sqlx::PgPool,
    session_id: &str,
) -> std::collections::HashMap<String, u32> {
    sqlx::query_as::<_, (String, i32)>(
        "SELECT window_key, step FROM usage_notice_steps WHERE session_id = $1",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(k, s)| (k, u32::try_from(s).unwrap_or(0)))
    .collect()
}

/// Claim a step for one window. `true` only for the caller that actually moved
/// the stored step up, so two replicas racing the same climb notify once.
async fn claim_step(pool: &sqlx::PgPool, session_id: &str, window_key: &str, step: u32) -> bool {
    sqlx::query_scalar::<_, i32>(
        "INSERT INTO usage_notice_steps (session_id, window_key, step) VALUES ($1, $2, $3) \
         ON CONFLICT (session_id, window_key) DO UPDATE \
             SET step = EXCLUDED.step, notified_at = now() \
             WHERE usage_notice_steps.step < EXCLUDED.step \
         RETURNING step",
    )
    .bind(session_id)
    .bind(window_key)
    .bind(i32::try_from(step).unwrap_or(i32::MAX))
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .is_some()
}

async fn forget_step(pool: &sqlx::PgPool, session_id: &str, window_key: &str) {
    let _ = sqlx::query("DELETE FROM usage_notice_steps WHERE session_id = $1 AND window_key = $2")
        .bind(session_id)
        .bind(window_key)
        .execute(pool)
        .await;
}

async fn record_step(pool: &sqlx::PgPool, session_id: &str, window_key: &str, step: u32) {
    let _ = sqlx::query(
        "INSERT INTO usage_notice_steps (session_id, window_key, step) VALUES ($1, $2, $3) \
         ON CONFLICT (session_id, window_key) DO UPDATE \
             SET step = EXCLUDED.step, notified_at = now()",
    )
    .bind(session_id)
    .bind(window_key)
    .bind(i32::try_from(step).unwrap_or(i32::MAX))
    .execute(pool)
    .await;
}

/// Hand the notice to the running session as an ordinary turn.
async fn deliver(state: &AppState, session_id: &str, text: String) -> bool {
    let row: Option<(Option<uuid::Uuid>, Option<String>)> =
        sqlx::query_as("SELECT machine_uuid, adapter_id FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten();
    let Some((Some(machine), adapter_id)) = row else { return false };
    let frame = cctui_proto::ws::DaemonFrameDown::Command {
        adapter_id: adapter_id.unwrap_or_else(|| "claude-code".to_owned()),
        command: Box::new(cctui_proto::adapter::AdapterCommand::SendMessage {
            local_id: session_id.to_owned(),
            text,
        }),
    };
    state.bus.command_daemon_for_session(machine, session_id, frame).await.is_ok()
}

/// Deliver a usage notice to the session if one of the account's windows just
/// crossed a step. Never touches the proxied request.
pub async fn deliver_if_due(state: &AppState, acct: &Account, session_token: &str) {
    if !acct.usage_notices.enabled {
        return;
    }
    let Some(session_id) = session_id_for_token(state, session_token).await else { return };
    let Some(usage) = usage_for_soft_limit(state, acct.id).await else { return };
    let windows = crate::soft_limit::normalize_usage_windows(&usage);
    let stored = last_steps(&state.pool, &session_id).await;
    let actions = window_actions(&stored, &windows, acct.usage_notices.step_pct);

    let mut claimed: Vec<(String, Option<u32>)> = Vec::new();
    for (w, action) in actions {
        match action {
            StepAction::Notify(step) => {
                if claim_step(&state.pool, &session_id, &w.key, step).await {
                    claimed.push((w.key.clone(), stored.get(&w.key).copied()));
                }
            }
            StepAction::Record(step) => record_step(&state.pool, &session_id, &w.key, step).await,
            StepAction::Unchanged => {}
        }
    }
    if claimed.is_empty() {
        return;
    }
    let text = notice_text(&windows, &acct.soft_limits, Utc::now());
    if deliver(state, &session_id, text).await {
        tracing::debug!(account = %acct.id, session = %session_id, "usage notice delivered");
        return;
    }
    // Undelivered: give the step back so the next turn tries again rather than
    // swallowing the notice for this bucket forever.
    for (key, prev) in claimed {
        if let Some(step) = prev {
            record_step(&state.pool, &session_id, &key, step).await;
        } else {
            forget_step(&state.pool, &session_id, &key).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn window(
        key: &str,
        kind: &str,
        label: &str,
        utilization: f64,
        resets_at: Option<DateTime<Utc>>,
    ) -> UsageWindow {
        UsageWindow {
            key: key.into(),
            kind: kind.into(),
            label: label.into(),
            utilization,
            amount_usd: None,
            resets_at,
            model_id: None,
            model_display_name: None,
        }
    }

    fn session_at(pct: f64) -> Vec<UsageWindow> {
        vec![window("session", "session", "5h", pct, None)]
    }

    /// Apply the actions to a step map the way the DB layer does, reporting
    /// whether the sweep would have notified.
    fn tick(steps: &mut std::collections::HashMap<String, u32>, pct: f64) -> bool {
        let windows = session_at(pct);
        let mut notified = false;
        for (w, action) in window_actions(steps, &windows, 10) {
            match action {
                StepAction::Notify(s) => {
                    notified = true;
                    steps.insert(w.key.clone(), s);
                }
                StepAction::Record(s) => {
                    steps.insert(w.key.clone(), s);
                }
                StepAction::Unchanged => {}
            }
        }
        notified
    }

    #[test]
    fn notifies_after_52_and_81_only() {
        let mut steps = std::collections::HashMap::new();
        let notified: Vec<bool> =
            [4.0, 52.0, 58.0, 81.0].into_iter().map(|pct| tick(&mut steps, pct)).collect();
        assert_eq!(notified, [false, true, false, true]);
        assert_eq!(bucket(52.0, 10), 5);
        assert_eq!(bucket(81.0, 10), 8);
    }

    #[test]
    fn sessions_are_independent_and_resets_rearm() {
        let mut s1 = std::collections::HashMap::new();
        let mut s2 = std::collections::HashMap::new();
        assert!(tick(&mut s1, 52.0));
        assert!(!tick(&mut s1, 55.0));
        assert!(tick(&mut s2, 55.0));
        assert!(!tick(&mut s1, 3.0));
        assert!(tick(&mut s1, 12.0));
    }

    #[test]
    fn usd_windows_never_tick() {
        let steps = std::collections::HashMap::new();
        let w = crate::soft_limit::usd_window(crate::soft_limit::KEY_SESSION_USD, 5.0, None);
        assert!(window_actions(&steps, &[w], 10).is_empty());
    }

    #[test]
    fn step_action_covers_climb_hold_and_reset() {
        assert_eq!(step_action(None, 0), StepAction::Record(0));
        assert_eq!(step_action(None, 5), StepAction::Notify(5));
        assert_eq!(step_action(Some(5), 5), StepAction::Unchanged);
        assert_eq!(step_action(Some(5), 8), StepAction::Notify(8));
        assert_eq!(step_action(Some(8), 1), StepAction::Record(1));
    }

    #[test]
    fn message_text_format() {
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 0, 18, 0).unwrap();
        let windows = [
            window(
                "session",
                "session",
                "5h",
                60.2,
                Some(Utc.with_ymd_and_hms(2026, 9, 5, 1, 30, 0).unwrap()),
            ),
            window(
                "weekly_all",
                "weekly_all",
                "Weekly (all models)",
                15.0,
                Some(Utc.with_ymd_and_hms(2026, 9, 7, 9, 0, 0).unwrap()),
            ),
            window("weekly_model:fable", "weekly_scoped", "Weekly Fable", 30.0, None),
        ];
        let limits =
            SoftLimits::from_json(Some(&serde_json::json!({ "session": { "cap_pct": 98 } })));
        assert_eq!(
            notice_text(&windows, &limits, now),
            "Account usage notice: 5h window at 60 % (soft limit 98 %), resets 01:30 UTC in 1h12. \
             Weekly (all models) at 15 %, resets Mon 09:00 UTC in 2d8h. Weekly Fable at 30 %."
        );
    }

    #[test]
    fn setting_parses_and_validates() {
        assert_eq!(UsageNotices::from_json(None), UsageNotices::default());
        assert!(!UsageNotices::default().enabled);
        let on =
            UsageNotices::from_json(Some(&serde_json::json!({ "enabled": true, "step_pct": 25 })));
        assert_eq!(on, UsageNotices { enabled: true, step_pct: 25 });
        assert_eq!(
            UsageNotices::from_json(Some(&serde_json::json!({ "enabled": true, "step_pct": 0 })))
                .step_pct,
            DEFAULT_STEP_PCT
        );
        assert_eq!(UsageNotices::build_json(None).unwrap(), None);
        assert_eq!(
            UsageNotices::build_json(Some(&serde_json::json!({ "enabled": false }))).unwrap(),
            None
        );
        assert_eq!(
            UsageNotices::build_json(Some(&serde_json::json!({ "enabled": true }))).unwrap(),
            Some(serde_json::json!({ "enabled": true, "step_pct": 10 }))
        );
        assert!(
            UsageNotices::build_json(Some(
                &serde_json::json!({ "enabled": true, "step_pct": 101 })
            ))
            .is_err()
        );
        assert!(UsageNotices::build_json(Some(&serde_json::json!([]))).is_err());
    }
}
