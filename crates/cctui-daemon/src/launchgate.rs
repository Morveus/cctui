//! Hold a job launch while its model is soft-limit blocked.
//!
//! Launching into a blocked model spends the session's first turn on a 429.
//! The server already knows the answer (`CctuiUsage` /
//! `GET /sessions/{id}/limits`), so the daemon asks before it dispatches and
//! waits out `retry_after` instead.
//!
//! Every failure mode here fails OPEN: an unreachable server, a malformed
//! reply, or a hold that outlives [`MAX_HOLD`] releases the launch. A limits
//! endpoint having a bad day must never be able to stop work from starting.

use std::time::{Duration, Instant};

use serde_json::Value;

/// Longest a launch is ever held. Past this the launch proceeds and eats
/// whatever the upstream says, which is strictly better than a job that never
/// starts.
pub const MAX_HOLD: Duration = Duration::from_mins(30);

/// Wait applied when the server blocks without naming a `retry_after`.
const DEFAULT_RETRY: Duration = Duration::from_mins(1);

/// Longest single sleep between re-checks, so a long `retry_after` still
/// re-reads the decision periodically and resumes early when it clears.
const MAX_SLEEP: Duration = Duration::from_mins(1);

/// A refusal to launch right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub retry_after: Duration,
    pub reason: String,
}

impl Hold {
    /// One line for the session card.
    #[must_use]
    pub fn card_detail(&self) -> String {
        let secs = self.retry_after.as_secs();
        let when = if secs >= 60 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{secs}s")
        };
        format!("limit reached — retrying in {when} ({})", self.reason)
    }
}

/// The hold a limits payload implies, or `None` when the launch may proceed.
///
/// `decision` is the answer for the model the call asked about; `per_model` is
/// consulted only when the top-level decision is absent, so a payload shaped
/// either way is understood. Anything unparseable allows.
#[must_use]
pub fn hold_from_limits(limits: &Value, model: Option<&str>) -> Option<Hold> {
    let decision = limits
        .get("decision")
        .filter(|d| d.get("allow").is_some())
        .or_else(|| model.and_then(|m| limits.pointer(&format!("/per_model/{m}"))))?;
    if decision.get("allow").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let retry_after = decision
        .get("retry_after_secs")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .map_or(DEFAULT_RETRY, |s| Duration::from_secs(s.unsigned_abs()));
    let reason = decision
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("rate limited")
        .to_owned();
    Some(Hold { retry_after, reason })
}

/// The next sleep before re-checking a hold.
#[must_use]
pub fn backoff(hold: &Hold) -> Duration {
    hold.retry_after.min(MAX_SLEEP).max(Duration::from_secs(1))
}

/// Whether a hold that started at `began` has run out of patience.
#[must_use]
pub fn expired(began: Instant) -> bool {
    began.elapsed() >= MAX_HOLD
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RETRY, Duration, MAX_SLEEP, backoff, hold_from_limits};
    use serde_json::json;

    #[test]
    fn an_allowing_decision_does_not_hold() {
        assert!(hold_from_limits(&json!({ "decision": { "allow": true } }), None).is_none());
    }

    #[test]
    fn a_blocking_decision_carries_its_retry_and_reason() {
        let limits = json!({
            "decision": { "allow": false, "retry_after_secs": 180, "reason": "weekly cap" }
        });
        let hold = hold_from_limits(&limits, None).expect("a blocked model holds the launch");
        assert_eq!(hold.retry_after, Duration::from_mins(3));
        assert_eq!(hold.reason, "weekly cap");
        assert!(hold.card_detail().contains("3m00s"), "{}", hold.card_detail());
        assert!(hold.card_detail().contains("weekly cap"));
    }

    #[test]
    fn a_block_without_a_retry_still_waits_a_sane_default() {
        let hold = hold_from_limits(&json!({ "decision": { "allow": false } }), None).unwrap();
        assert_eq!(hold.retry_after, DEFAULT_RETRY);
        assert_eq!(hold.reason, "rate limited");
    }

    /// An unreadable payload must allow: the gate fails open.
    #[test]
    fn a_payload_without_a_decision_allows() {
        assert!(hold_from_limits(&json!({}), None).is_none());
        assert!(hold_from_limits(&json!({ "stale": true }), Some("opus")).is_none());
        assert!(hold_from_limits(&json!("nonsense"), None).is_none());
    }

    #[test]
    fn the_per_model_decision_is_used_when_there_is_no_top_level_one() {
        let limits = json!({
            "per_model": { "claude-opus-5": { "allow": false, "retry_after_secs": 30 } }
        });
        let hold = hold_from_limits(&limits, Some("claude-opus-5")).expect("blocked for the model");
        assert_eq!(hold.retry_after, Duration::from_secs(30));
        assert!(
            hold_from_limits(&limits, Some("claude-haiku-4-5")).is_none(),
            "another model's block must not hold this launch"
        );
    }

    #[test]
    fn a_long_retry_is_rechecked_rather_than_slept_through() {
        let hold = hold_from_limits(
            &json!({ "decision": { "allow": false, "retry_after_secs": 3600 } }),
            None,
        )
        .unwrap();
        assert_eq!(backoff(&hold), MAX_SLEEP);
    }
}
