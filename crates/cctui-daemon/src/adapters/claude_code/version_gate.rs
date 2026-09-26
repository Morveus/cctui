//! Cycle a `claude daemon` left behind by a CLI auto-update, but only while
//! nothing is running: the sole remedy the CLI offers, `daemon stop --any`,
//! kills every background worker.
//!
//! Idle must be agreed by two sources. Our roster is filtered
//! (`is_user_visible` drops spares); the `bg workers: N running` count is the
//! daemon's own.
//! Unknown counts as busy — a missed upgrade costs a stale daemon until the
//! next check, a wrong cycle costs somebody's session.
//!
//! Worker counts alone can deadlock the cycle: a stale daemon breaks its own
//! workers, the broken sessions park at the prompt and keep the counts
//! non-zero (spares inflate the daemon's count the same way), and the
//! mismatch defers forever while nothing on the machine can reach the API.
//! Hence the escalation: a mismatch deferred [`ESCALATE_AFTER`] with no
//! roster session seen busy the whole time cycles anyway — quiescent
//! sessions survive as resumable transcripts, and a session observed
//! mid-turn keeps resetting the clock. A live job cctui did not start vetoes
//! the escalation outright: `stop --any` would kill someone's own terminal
//! session, which is not ours to trade away.
//!
//! The versions cannot come from the control socket: `cliVersion` rides each
//! *job*, so an idle daemon reports none — exactly when cycling is safe.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Each check shells `claude` twice; upgrades land a few times a day at most.
const CHECK_MIN_INTERVAL: Duration = Duration::from_mins(5);

/// How long a mismatch must stay deferred, with the roster quiescent the
/// whole time, before cycling over non-zero worker counts.
const ESCALATE_AFTER: Duration = Duration::from_mins(30);

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Decision {
    Nothing,
    Deferred { running: String, local: String },
    Cycle { running: String, local: String, escalated: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CycleMethod {
    ManagedService,
    /// For a daemon started outside our unit (`origin: foreground`), which a
    /// unit restart would not touch.
    StopAny,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    pub version: Option<String>,
    /// `None` when the count was not reported — treated as busy.
    pub running_workers: Option<usize>,
}

/// Parse `claude --version`, whose output is `2.1.218 (Claude Code)`.
pub fn parse_cli_version(stdout: &str) -> Option<String> {
    let tok = stdout.split_whitespace().next()?;
    tok.starts_with(|c: char| c.is_ascii_digit()).then(|| tok.to_string())
}

/// Parse the header of `claude daemon status`. Tolerant by construction: an
/// unrecognised line leaves that fact `None`, which steers away from cycling.
pub fn parse_daemon_status(stdout: &str) -> DaemonStatus {
    let mut out = DaemonStatus::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version:") {
            let v = rest.trim();
            if v.starts_with(|c: char| c.is_ascii_digit()) {
                out.version = Some(v.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("bg workers:") {
            out.running_workers = parse_running_workers(rest);
        }
    }
    out
}

/// `2 running (control.sock), 2 in roster.json` -> `Some(2)`, but
/// `0 in roster.json (control unreachable)` -> `None`: with no socket the
/// count says nothing about what is alive.
fn parse_running_workers(rest: &str) -> Option<usize> {
    let mut toks = rest.split_whitespace().peekable();
    while let Some(tok) = toks.next() {
        if toks.peek() == Some(&"running") {
            return tok.parse().ok();
        }
    }
    None
}

/// Decide from the two versions and the idle evidence.
///
/// `live_workers` is `None` when the count is unknown — which blocks cycling
/// just as a non-zero count does.
pub(super) fn decide(
    running: Option<&str>,
    local: Option<&str>,
    live_workers: Option<usize>,
) -> Decision {
    let (Some(running), Some(local)) = (running, local) else {
        return Decision::Nothing;
    };
    if running == local {
        return Decision::Nothing;
    }
    let (running, local) = (running.to_string(), local.to_string());
    if live_workers == Some(0) {
        Decision::Cycle { running, local, escalated: false }
    } else {
        Decision::Deferred { running, local }
    }
}

/// Fold our roster size together with the daemon's own count. Either source
/// seeing work, or the daemon's count being unknown, means busy.
pub(super) fn live_workers(roster_len: usize, reported: Option<usize>) -> Option<usize> {
    reported.map(|n| n.max(roster_len))
}

/// Rate-limited version check driving the idle auto-cycle.
pub(super) struct VersionGate {
    claude_bin: String,
    last: Mutex<Option<Instant>>,
    /// The mismatch most recently logged, so a deferred upgrade warns once per
    /// version pair rather than on every check for as long as work is running.
    warned: Mutex<Option<(String, String)>>,
    /// Escalation clock: the deferred version pair and when the roster was
    /// last seen busy (or the deferral first seen) — whichever is later.
    deferred: Mutex<Option<(String, String, Instant)>>,
    /// The pair whose escalation a live foreign job most recently vetoed, so
    /// the veto logs once per version pair instead of every check.
    native_vetoed: Mutex<Option<(String, String)>>,
}

impl VersionGate {
    pub(super) const fn new(claude_bin: String) -> Self {
        Self {
            claude_bin,
            last: Mutex::new(None),
            warned: Mutex::new(None),
            deferred: Mutex::new(None),
            native_vetoed: Mutex::new(None),
        }
    }

    /// Reset the escalation clock. Called from every roster poll that sees a
    /// busy session, so escalation requires quiescence across the whole
    /// window, not just at check time.
    pub(super) fn note_roster_busy(&self) {
        let mut deferred = self.deferred.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((_, _, since)) = deferred.as_mut() {
            *since = Instant::now();
        }
    }

    /// Upgrade a stale [`Decision::Deferred`] to an escalated cycle once the
    /// same pair has sat quiet for [`ESCALATE_AFTER`]. Any other decision
    /// clears the clock.
    ///
    /// `native_live` vetoes the escalation for as long as it holds: cycling
    /// runs `daemon stop --any`, which would kill a claude job cctui did not
    /// start. The clock is left armed so the cycle happens the moment that job
    /// is gone.
    fn maybe_escalate(&self, decision: Decision, now: Instant, native_live: bool) -> Decision {
        let Decision::Deferred { running, local } = decision else {
            *self.deferred.lock().unwrap_or_else(PoisonError::into_inner) = None;
            return decision;
        };
        let due = {
            let mut deferred = self.deferred.lock().unwrap_or_else(PoisonError::into_inner);
            match deferred.as_ref() {
                Some((r, l, since)) if *r == running && *l == local => {
                    let due = now.duration_since(*since) >= ESCALATE_AFTER;
                    if due && !native_live {
                        *deferred = None;
                    }
                    due
                }
                _ => {
                    *deferred = Some((running.clone(), local.clone(), now));
                    false
                }
            }
        };
        if !due {
            return Decision::Deferred { running, local };
        }
        if native_live {
            if self.first_native_veto_for(&running, &local) {
                tracing::warn!(
                    %running,
                    %local,
                    "version mismatch is past the escalation window but a claude job cctui did \
                     not start is live; deferring until it is gone"
                );
            }
            return Decision::Deferred { running, local };
        }
        Decision::Cycle { running, local, escalated: true }
    }

    /// Record `now` and report whether the probe interval has elapsed. Pure —
    /// unit-tested without spawning anything.
    fn gate(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let permit = last.is_none_or(|t| now.duration_since(t) >= CHECK_MIN_INTERVAL);
        if permit {
            *last = Some(now);
        }
        permit
    }

    /// True the first time this exact mismatch is seen, so the deferred case
    /// logs once instead of every check.
    fn first_warning_for(&self, running: &str, local: &str) -> bool {
        let mut warned = self.warned.lock().unwrap_or_else(PoisonError::into_inner);
        let pair = (running.to_string(), local.to_string());
        if warned.as_ref() == Some(&pair) {
            return false;
        }
        *warned = Some(pair);
        true
    }

    /// True the first time this exact pair is vetoed by a live foreign job.
    fn first_native_veto_for(&self, running: &str, local: &str) -> bool {
        let mut vetoed = self.native_vetoed.lock().unwrap_or_else(PoisonError::into_inner);
        let pair = (running.to_string(), local.to_string());
        if vetoed.as_ref() == Some(&pair) {
            return false;
        }
        *vetoed = Some(pair);
        true
    }

    async fn probe(&self, args: &[&str]) -> Option<String> {
        let out = tokio::time::timeout(
            PROBE_TIMEOUT,
            tokio::process::Command::new(&self.claude_bin)
                .args(args)
                .env("PATH", crate::childenv::child_path())
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run one check if the interval has elapsed, returning the decision taken
    /// and (when cycling) how to bounce the daemon. `None` when the check was
    /// skipped or nothing needs doing.
    pub(super) async fn check(&self, roster_len: usize, native_live: bool) -> Option<Decision> {
        if !self.gate(Instant::now()) {
            return None;
        }
        let status = parse_daemon_status(&self.probe(&["daemon", "status"]).await?);
        let local = parse_cli_version(&self.probe(&["--version"]).await?);
        let decision = decide(
            status.version.as_deref(),
            local.as_deref(),
            live_workers(roster_len, status.running_workers),
        );
        let decision = self.maybe_escalate(decision, Instant::now(), native_live);
        match &decision {
            Decision::Nothing => None,
            Decision::Deferred { running, local } => {
                if self.first_warning_for(running, local) {
                    tracing::warn!(
                        %running,
                        %local,
                        roster_len,
                        reported_workers = ?status.running_workers,
                        "claude daemon is older than the installed CLI; deferring the cycle \
                         until no workers are running"
                    );
                }
                Some(decision)
            }
            Decision::Cycle { running, local, escalated } => {
                if *escalated {
                    tracing::warn!(
                        %running,
                        %local,
                        roster_len,
                        reported_workers = ?status.running_workers,
                        "version mismatch deferred past the escalation window with a quiescent \
                         roster; cycling over non-zero worker counts"
                    );
                }
                Some(decision)
            }
        }
    }

    /// Bounce the supervisor. Prefers the managed unit; falls back to the
    /// CLI's own `stop --any` when the live daemon was not started by it.
    /// Best-effort: the caller re-establishes the socket either way.
    pub(super) async fn cycle(&self, method: CycleMethod) -> anyhow::Result<()> {
        match method {
            CycleMethod::ManagedService => {
                let bin = self.claude_bin.clone();
                tokio::task::spawn_blocking(move || super::claude_service::restart(&bin)).await?
            }
            CycleMethod::StopAny => {
                let out = tokio::time::timeout(
                    PROBE_TIMEOUT,
                    tokio::process::Command::new(&self.claude_bin)
                        .args(["daemon", "stop", "--any"])
                        .env("PATH", crate::childenv::child_path())
                        .stdin(std::process::Stdio::null())
                        .output(),
                )
                .await??;
                anyhow::ensure!(
                    out.status.success(),
                    "`claude daemon stop --any` failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUSY_STATUS: &str = "\
pid:     100001
version: 2.1.218
uptime:  161528s
origin:  foreground
config:  /home/you/.claude/daemon.json
log:     /home/you/.claude/daemon.log

bg sessions:
  sock dir:     /tmp/cc-daemon-9999/9a6631b1
  control.sock: reachable
  bg workers:   2 running (control.sock), 2 in roster.json
  roster.json:  updated 187s ago
";

    const IDLE_STATUS: &str = "\
pid:     100002
version: 2.1.220
uptime:  11s
origin:  foreground

bg sessions:
  control.sock: reachable
  bg workers:   0 running (control.sock), 0 in roster.json
";

    const DOWN_STATUS: &str = "\
not running

bg sessions:
  sock dir:     /tmp/cc-daemon-9999/f47e2fbc
  control.sock: unreachable (connect ENOENT)
  bg workers:   0 in roster.json (control unreachable)
";

    #[test]
    fn parses_cli_version_from_the_version_banner() {
        assert_eq!(parse_cli_version("2.1.218 (Claude Code)\n").as_deref(), Some("2.1.218"));
        assert_eq!(parse_cli_version(""), None);
        assert_eq!(parse_cli_version("some unexpected banner"), None);
    }

    #[test]
    fn parses_version_and_running_workers_from_status() {
        let busy = parse_daemon_status(BUSY_STATUS);
        assert_eq!(busy.version.as_deref(), Some("2.1.218"));
        assert_eq!(busy.running_workers, Some(2));

        let idle = parse_daemon_status(IDLE_STATUS);
        assert_eq!(idle.version.as_deref(), Some("2.1.220"));
        assert_eq!(idle.running_workers, Some(0));
    }

    #[test]
    fn a_stopped_daemon_yields_no_version_and_no_worker_count() {
        let down = parse_daemon_status(DOWN_STATUS);
        assert_eq!(down.version, None);
        // "0 in roster.json (control unreachable)" is NOT a running count: the
        // daemon could not see the socket, so it must not read as idle.
        assert_eq!(down.running_workers, None);
    }

    #[test]
    fn matching_versions_do_nothing() {
        assert_eq!(decide(Some("2.1.220"), Some("2.1.220"), Some(0)), Decision::Nothing);
    }

    #[test]
    fn a_missing_probe_does_nothing() {
        assert_eq!(decide(None, Some("2.1.220"), Some(0)), Decision::Nothing);
        assert_eq!(decide(Some("2.1.212"), None, Some(0)), Decision::Nothing);
    }

    #[test]
    fn mismatch_with_no_workers_cycles() {
        assert_eq!(
            decide(Some("2.1.212"), Some("2.1.220"), Some(0)),
            Decision::Cycle {
                running: "2.1.212".into(),
                local: "2.1.220".into(),
                escalated: false
            }
        );
    }

    #[test]
    fn mismatch_with_a_live_worker_defers() {
        assert_eq!(
            decide(Some("2.1.212"), Some("2.1.220"), Some(1)),
            Decision::Deferred { running: "2.1.212".into(), local: "2.1.220".into() }
        );
    }

    #[test]
    fn an_unknown_worker_count_defers_rather_than_cycling() {
        assert_eq!(
            decide(Some("2.1.212"), Some("2.1.220"), None),
            Decision::Deferred { running: "2.1.212".into(), local: "2.1.220".into() }
        );
    }

    #[test]
    fn our_roster_can_veto_the_daemons_idle_report() {
        // The daemon says nothing is running but we are tracking a session:
        // busy wins, because either source seeing work means work exists.
        assert_eq!(live_workers(1, Some(0)), Some(1));
        assert_eq!(live_workers(0, Some(2)), Some(2));
        assert_eq!(live_workers(0, Some(0)), Some(0));
    }

    #[test]
    fn an_unreported_count_stays_unknown_even_with_an_empty_roster() {
        assert_eq!(live_workers(0, None), None);
    }

    /// Guards the fixtures above against CLI output drift. Needs a real
    /// `claude` with a running daemon, so it is not part of the normal run:
    /// `cargo test -p cctui-daemon -- --ignored parsers_match_the_live_cli`.
    #[test]
    #[ignore = "requires a live `claude` daemon"]
    fn parsers_match_the_live_cli() {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("claude").args(args).output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let status = parse_daemon_status(&run(&["daemon", "status"]));
        assert!(status.version.is_some(), "no version parsed from live `claude daemon status`");
        assert!(status.running_workers.is_some(), "no worker count parsed; daemon down?");
        assert!(parse_cli_version(&run(&["--version"])).is_some());
    }

    #[test]
    fn gate_permits_first_then_backs_off() {
        let g = VersionGate::new("claude".into());
        let t0 = Instant::now();
        assert!(g.gate(t0));
        assert!(!g.gate(t0 + Duration::from_secs(1)));
        assert!(g.gate(t0 + CHECK_MIN_INTERVAL));
    }

    fn deferred(r: &str, l: &str) -> Decision {
        Decision::Deferred { running: r.into(), local: l.into() }
    }

    #[test]
    fn deferral_escalates_to_a_cycle_after_the_quiet_window() {
        let g = VersionGate::new("claude".into());
        let t0 = Instant::now();
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0, false),
            deferred("2.1.212", "2.1.220")
        );
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0 + ESCALATE_AFTER, false),
            Decision::Cycle { running: "2.1.212".into(), local: "2.1.220".into(), escalated: true }
        );
    }

    #[test]
    fn roster_activity_resets_the_escalation_clock() {
        let g = VersionGate::new("claude".into());
        let t0 = Instant::now();
        g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0, false);
        g.note_roster_busy();
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0 + ESCALATE_AFTER, false),
            deferred("2.1.212", "2.1.220")
        );
    }

    #[test]
    fn a_new_version_pair_restarts_the_escalation_clock() {
        let g = VersionGate::new("claude".into());
        let t0 = Instant::now();
        g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0, false);
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.221"), t0 + ESCALATE_AFTER, false),
            deferred("2.1.212", "2.1.221")
        );
    }

    #[test]
    fn a_resolved_mismatch_clears_the_escalation_clock() {
        let g = VersionGate::new("claude".into());
        let t0 = Instant::now();
        g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0, false);
        assert_eq!(
            g.maybe_escalate(Decision::Nothing, t0 + Duration::from_secs(1), false),
            Decision::Nothing
        );
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0 + ESCALATE_AFTER, false),
            deferred("2.1.212", "2.1.220")
        );
    }

    #[test]
    fn busy_note_without_an_active_deferral_is_a_no_op() {
        let g = VersionGate::new("claude".into());
        g.note_roster_busy();
        let t0 = Instant::now();
        g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0, false);
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0 + ESCALATE_AFTER, false),
            Decision::Cycle { running: "2.1.212".into(), local: "2.1.220".into(), escalated: true }
        );
    }

    #[test]
    fn a_live_foreign_job_vetoes_the_escalation() {
        let g = VersionGate::new("claude".into());
        let t0 = Instant::now();
        g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0, true);
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0 + ESCALATE_AFTER, true),
            deferred("2.1.212", "2.1.220"),
            "a job cctui did not start must never be cycled away"
        );
        // The clock stays armed: the cycle happens as soon as that job is gone.
        assert_eq!(
            g.maybe_escalate(deferred("2.1.212", "2.1.220"), t0 + ESCALATE_AFTER, false),
            Decision::Cycle { running: "2.1.212".into(), local: "2.1.220".into(), escalated: true }
        );
    }

    #[test]
    fn deferred_mismatch_warns_once_per_version_pair() {
        let g = VersionGate::new("claude".into());
        assert!(g.first_warning_for("2.1.212", "2.1.220"));
        assert!(!g.first_warning_for("2.1.212", "2.1.220"));
        // A newer CLI is a new fact and warns again.
        assert!(g.first_warning_for("2.1.212", "2.1.221"));
    }
}
