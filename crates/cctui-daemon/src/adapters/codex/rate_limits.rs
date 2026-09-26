//! Codex account rate-limit windows, from either carrier: the rollout's
//! `token_count` line (`rate_limits`, `snake_case`) or the app-server's
//! `account/rateLimits/updated` notification (`rateLimits`, camelCase).

use cctui_proto::adapter::{AdapterEvent, RateLimitWindow};
use serde_json::Value;

fn window(slot: &str, w: &Value, observed_at: Option<i64>) -> Option<RateLimitWindow> {
    let pick = |a: &str, b: &str| w.get(a).or_else(|| w.get(b)).filter(|v| !v.is_null());
    let used_percent = pick("used_percent", "usedPercent")?.as_f64()?;
    let window_minutes = pick("window_minutes", "windowDurationMins").and_then(Value::as_i64);
    let resets_at = pick("resets_at", "resetsAt").and_then(Value::as_i64).or_else(|| {
        let secs = w.get("resets_in_seconds").and_then(Value::as_i64)?;
        Some(observed_at? + secs)
    });
    Some(RateLimitWindow { used_percent, window_minutes, resets_at, slot: slot.to_owned() })
}

/// The windows of a `rate_limits` / `rateLimits` object.
pub fn windows(limits: &Value, observed_at: Option<i64>) -> Vec<RateLimitWindow> {
    ["primary", "secondary"]
        .iter()
        .filter_map(|slot| window(slot, limits.get(*slot)?, observed_at))
        .collect()
}

/// A rollout `event_msg`/`token_count` line → [`AdapterEvent::RateLimits`].
pub fn from_rollout_line(local_id: &str, value: &Value) -> Option<AdapterEvent> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let observed_at = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|t| t.timestamp());
    let windows = windows(payload.get("rate_limits")?, observed_at);
    (!windows.is_empty()).then(|| AdapterEvent::RateLimits {
        local_id: local_id.to_owned(),
        windows,
        observed_at,
    })
}

/// An `account/rateLimits/updated` notification → [`AdapterEvent::RateLimits`].
pub fn from_notification(local_id: &str, v: &Value) -> Option<AdapterEvent> {
    let windows = windows(v.pointer("/params/rateLimits")?, None);
    (!windows.is_empty()).then(|| AdapterEvent::RateLimits {
        local_id: local_id.to_owned(),
        windows,
        observed_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rollout_token_count_line_yields_both_windows() {
        let line = json!({
            "timestamp": "2026-05-30T07:36:59.740Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {
                    "primary": {"used_percent": 1.0, "window_minutes": 300, "resets_in_seconds": 600},
                    "secondary": {"used_percent": 33.0, "window_minutes": 10080, "resets_at": 1_780_000_000}
                }
            }
        });
        let Some(AdapterEvent::RateLimits { windows, observed_at, .. }) =
            from_rollout_line("s", &line)
        else {
            panic!("expected RateLimits");
        };
        let t = observed_at.unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].slot, "primary");
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[0].resets_at, Some(t + 600));
        assert_eq!(windows[1].resets_at, Some(1_780_000_000));
        assert!((windows[1].used_percent - 33.0).abs() < f64::EPSILON);
    }

    #[test]
    fn notification_reads_the_camel_case_snapshot() {
        let v = json!({"params": {"rateLimits": {
            "primary": {"usedPercent": 12, "windowDurationMins": 300, "resetsAt": 1_780_000_000},
            "secondary": null
        }}});
        let Some(AdapterEvent::RateLimits { windows, observed_at, .. }) =
            from_notification("s", &v)
        else {
            panic!("expected RateLimits");
        };
        assert_eq!(observed_at, None);
        assert_eq!(
            windows,
            vec![RateLimitWindow {
                used_percent: 12.0,
                window_minutes: Some(300),
                resets_at: Some(1_780_000_000),
                slot: "primary".into(),
            }]
        );
    }

    #[test]
    fn lines_without_rate_limits_yield_nothing() {
        let line = json!({"type": "event_msg", "payload": {"type": "token_count", "info": {}}});
        assert!(from_rollout_line("s", &line).is_none());
        assert!(from_notification("s", &json!({"params": {"rateLimits": {}}})).is_none());
    }
}
