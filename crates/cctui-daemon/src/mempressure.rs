//! Put idle sessions to sleep when the host runs short of memory.
//!
//! A Claude Code worker that has finished its turn still holds 1.3 to 1.4 GB
//! (its node process plus the stdio MCP servers it launched). On 19/09/2026 a
//! burst of 24 launches filled a 61 GB host whose 44 live sessions counted 18
//! such finished ones, and the machine froze for 27 minutes without a single
//! OOM kill. Stopping a finished worker loses nothing: its job state stays on
//! disk, the session shows as hibernated, and the next reply revives it through
//! the existing resume path.
//!
//! Nothing sleeps while memory is plentiful. Only when the kernel's
//! `MemAvailable` drops under a share of `MemTotal` does the driver stop the
//! session that has been idle the longest, one per check, then waits for the
//! freed memory to show before deciding again.
//!
//! A session is a candidate only when its turn is over (`tempo:"idle"` with
//! `state:"done"`): a worker idling on a background task or a monitor reports
//! `state:"working"` and is never touched, nor is one with a pending prompt or
//! a live subagent.
//!
//! Tunables, read from the daemon's environment:
//! - `CCTUI_SLEEP_BELOW_MEM_AVAILABLE_PCT`: threshold as a percentage of
//!   `MemTotal` (default 20, `0` disables the feature).
//! - `CCTUI_SLEEP_MIN_IDLE_SECS`: how long a session must have been quiet
//!   before it may be put to sleep (default 120).

use std::time::{Duration, SystemTime};

/// Default threshold: sleep idle sessions once less than this share of RAM is
/// available. 20 % of a 61 GB host is about 12 GB, several launches' worth.
const DEFAULT_THRESHOLD_PCT: f64 = 20.0;

/// A session that finished a turn a moment ago is likely to get an answer
/// soon; waking it costs a few seconds, so leave it a short grace.
const DEFAULT_MIN_IDLE: Duration = Duration::from_mins(2);

/// Pause after putting a session to sleep, so the freed memory shows in
/// `MemAvailable` before the next decision (one worker per step, never a
/// sweep that empties the roster on a single stale reading).
pub const COOLDOWN: Duration = Duration::from_secs(20);

/// The thresholds in force for this daemon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// `MemAvailable` below this share of `MemTotal` (0..=100) is pressure.
    pub threshold_pct: f64,
    /// Minimum quiet time before a session may be put to sleep.
    pub min_idle: Duration,
}

impl Policy {
    /// The policy from the daemon's environment; `None` when disabled.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_vars(
            std::env::var("CCTUI_SLEEP_BELOW_MEM_AVAILABLE_PCT").ok().as_deref(),
            std::env::var("CCTUI_SLEEP_MIN_IDLE_SECS").ok().as_deref(),
        )
    }

    fn from_vars(pct: Option<&str>, min_idle: Option<&str>) -> Option<Self> {
        let threshold_pct = match pct.map(str::trim) {
            None | Some("") => DEFAULT_THRESHOLD_PCT,
            Some(v) => match v.parse::<f64>() {
                Ok(p) if p <= 0.0 => return None,
                Ok(p) if p.is_finite() => p.min(100.0),
                _ => {
                    tracing::warn!(
                        value = v,
                        "invalid CCTUI_SLEEP_BELOW_MEM_AVAILABLE_PCT; using default"
                    );
                    DEFAULT_THRESHOLD_PCT
                }
            },
        };
        let min_idle = min_idle
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(DEFAULT_MIN_IDLE, Duration::from_secs);
        Some(Self { threshold_pct, min_idle })
    }

    /// Whether `mem` is under the threshold.
    #[must_use]
    pub fn under_pressure(&self, mem: Memory) -> bool {
        if mem.total == 0 {
            return false;
        }
        #[allow(clippy::cast_precision_loss)]
        let avail_pct = mem.available as f64 / mem.total as f64 * 100.0;
        avail_pct < self.threshold_pct
    }
}

/// Host memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    pub available: u64,
    pub total: u64,
}

impl Memory {
    /// The current reading from `/proc/meminfo`; `None` off Linux or when the
    /// file cannot be read, in which case nothing is ever put to sleep.
    #[must_use]
    pub fn read() -> Option<Self> {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let (available, total) = crate::resources::parse_mem_available(&info)?;
        Some(Self { available, total })
    }
}

/// A session the driver could put to sleep, and since when it has been quiet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub short: String,
    pub quiet_since: SystemTime,
}

/// The candidate quiet the longest, provided it has been quiet at least
/// `min_idle` at `now`.
#[must_use]
pub fn pick(candidates: &[Candidate], min_idle: Duration, now: SystemTime) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| now.duration_since(c.quiet_since).is_ok_and(|d| d >= min_idle))
        .min_by(|a, b| a.quiet_since.cmp(&b.quiet_since).then_with(|| a.short.cmp(&b.short)))
}

/// Whether a worker's turn is over with nothing running on its behalf.
///
/// That is idle tempo, `done` state and no pending prompt. A worker waiting on
/// a background task or a monitor reports `state:"working"`.
#[must_use]
pub fn turn_is_over(tempo: Option<&str>, state: Option<&str>, needs: Option<&str>) -> bool {
    tempo == Some("idle") && state == Some("done") && needs.is_none_or(|n| n.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn cand(short: &str, secs_ago: u64, now: SystemTime) -> Candidate {
        Candidate { short: short.into(), quiet_since: now - Duration::from_secs(secs_ago) }
    }

    #[test]
    fn policy_defaults_and_overrides() {
        let p = Policy::from_vars(None, None).unwrap();
        assert!((p.threshold_pct - 20.0).abs() < f64::EPSILON);
        assert_eq!(p.min_idle, Duration::from_mins(2));
        let p = Policy::from_vars(Some("12.5"), Some("600")).unwrap();
        assert!((p.threshold_pct - 12.5).abs() < f64::EPSILON);
        assert_eq!(p.min_idle, Duration::from_mins(10));
        assert_eq!(Policy::from_vars(Some("0"), None), None, "0 disables");
        let p = Policy::from_vars(Some("nope"), Some("x")).unwrap();
        assert!((p.threshold_pct - 20.0).abs() < f64::EPSILON, "garbage falls back");
        assert_eq!(p.min_idle, Duration::from_mins(2));
    }

    #[test]
    fn pressure_is_available_share_under_threshold() {
        let p = Policy::from_vars(Some("20"), None).unwrap();
        assert!(p.under_pressure(Memory { available: 10 * GB, total: 61 * GB }));
        assert!(!p.under_pressure(Memory { available: 16 * GB, total: 61 * GB }));
        assert!(
            !p.under_pressure(Memory { available: 0, total: 0 }),
            "unknown total is no pressure"
        );
    }

    #[test]
    fn picks_the_longest_quiet_past_the_grace() {
        let now = SystemTime::now();
        let c = [cand("young", 30, now), cand("older", 3600, now), cand("old", 600, now)];
        assert_eq!(pick(&c, Duration::from_mins(2), now).unwrap().short, "older");
        assert_eq!(pick(&c[..1], Duration::from_mins(2), now), None, "too fresh to sleep");
        assert_eq!(pick(&[], Duration::ZERO, now), None);
    }

    #[test]
    fn only_a_finished_turn_is_asleep_material() {
        assert!(turn_is_over(Some("idle"), Some("done"), Some("")));
        assert!(turn_is_over(Some("idle"), Some("done"), None));
        // Idle tempo but still working: waiting on a background task/monitor.
        assert!(!turn_is_over(Some("idle"), Some("working"), None));
        assert!(!turn_is_over(Some("active"), Some("working"), None));
        assert!(!turn_is_over(Some("idle"), Some("done"), Some("approve Bash: ls")));
        assert!(!turn_is_over(Some("blocked"), Some("done"), None));
    }
}
