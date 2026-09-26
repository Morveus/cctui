//! Cycle a shared `codex app-server daemon` left behind by a CLI update, the
//! codex counterpart of the claude `version_gate`.
//!
//! Busy is any live cctui codex session with an in-flight turn; a session
//! whose snapshot does not answer counts as busy. Unknown busy defers but can
//! escalate after [`ESCALATE_AFTER`]; a turn seen in flight resets that clock
//! and never escalates. Per-session stdio app-servers are not touched by the
//! restart: they finish on the old binary.

use std::time::{Duration, Instant};

use serde_json::Value;

pub const ESCALATE_AFTER: Duration = Duration::from_mins(30);

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Nothing,
    Deferred { running: String, local: String },
    Cycle { running: String, local: String, escalated: bool },
}

/// `Some(true)` a turn is in flight, `Some(false)` idle, `None` unknown.
pub type Busy = Option<bool>;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DaemonVersions {
    pub cli: Option<String>,
    pub app_server: Option<String>,
}

/// Parse `codex app-server daemon version`, whose JSON carries `cliVersion`
/// and `appServerVersion` (absent when the daemon is not running).
#[must_use]
pub fn parse_daemon_version(stdout: &str) -> DaemonVersions {
    let field = |v: &Value, k: &str| {
        v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
    };
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if v.is_object() {
            return DaemonVersions {
                cli: field(&v, "cliVersion"),
                app_server: field(&v, "appServerVersion"),
            };
        }
    }
    DaemonVersions::default()
}

/// Parse `codex --version`, e.g. `codex-cli 0.153.4`.
#[must_use]
pub fn parse_cli_version(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
}

#[must_use]
pub fn decide(running: Option<&str>, local: Option<&str>, busy: Busy) -> Decision {
    let (Some(running), Some(local)) = (running, local) else {
        return Decision::Nothing;
    };
    if running == local {
        return Decision::Nothing;
    }
    let (running, local) = (running.to_owned(), local.to_owned());
    if busy == Some(false) {
        Decision::Cycle { running, local, escalated: false }
    } else {
        Decision::Deferred { running, local }
    }
}

#[derive(Default)]
pub struct CodexVersionGate {
    /// Deferred pair and when a turn was last seen in flight (or the
    /// deferral began), whichever is later.
    deferred: Option<(String, String, Instant)>,
}

impl CodexVersionGate {
    /// Upgrade a deferral that sat [`ESCALATE_AFTER`] with no turn seen in
    /// flight to a cycle. Any other decision clears the clock.
    pub fn escalate(&mut self, decision: Decision, busy: Busy, now: Instant) -> Decision {
        let Decision::Deferred { running, local } = decision else {
            self.deferred = None;
            return decision;
        };
        let since = match self.deferred.take() {
            Some((r, l, since)) if r == running && l == local => {
                if busy == Some(true) {
                    now
                } else if now.duration_since(since) >= ESCALATE_AFTER {
                    return Decision::Cycle { running, local, escalated: true };
                } else {
                    since
                }
            }
            _ => now,
        };
        self.deferred = Some((running.clone(), local.clone(), since));
        Decision::Deferred { running, local }
    }

    pub fn check(&mut self, versions: &DaemonVersions, busy: Busy, now: Instant) -> Decision {
        let decision = decide(versions.app_server.as_deref(), versions.cli.as_deref(), busy);
        self.escalate(decision, busy, now)
    }
}

async fn run(bin: &str, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .env("PATH", crate::childenv::child_path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
    Ok(tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await??)
}

pub async fn probe_versions(bin: &str) -> DaemonVersions {
    match run(bin, &["app-server", "daemon", "version"]).await {
        Ok(out) => parse_daemon_version(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => DaemonVersions::default(),
    }
}

/// `codex app-server daemon restart`. The shared connection in
/// [`super::daemon`] reconnects to the same socket on its own.
pub async fn restart(bin: &str) -> anyhow::Result<()> {
    let out = run(bin, &["app-server", "daemon", "restart"]).await?;
    anyhow::ensure!(
        out.status.success(),
        "`codex app-server daemon restart` failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"{"status":"running","backend":"pid","managedCodexPath":"/home/you/.codex/packages/standalone/current/codex","managedCodexVersion":"0.153.4","socketPath":"/home/you/.codex/app-server-control/app-server-control.sock","cliVersion":"0.155.0","appServerVersion":"0.153.4"}"#;

    fn cycle(r: &str, l: &str, escalated: bool) -> Decision {
        Decision::Cycle { running: r.into(), local: l.into(), escalated }
    }

    fn deferred(r: &str, l: &str) -> Decision {
        Decision::Deferred { running: r.into(), local: l.into() }
    }

    #[test]
    fn parses_daemon_version_json() {
        let v = parse_daemon_version(RUNNING);
        assert_eq!(v.cli.as_deref(), Some("0.155.0"));
        assert_eq!(v.app_server.as_deref(), Some("0.153.4"));
    }

    #[test]
    fn a_stopped_daemon_has_no_app_server_version() {
        let v = parse_daemon_version(
            "warning: stale pid\n{\"status\":\"stopped\",\"cliVersion\":\"0.155.0\"}\n",
        );
        assert_eq!(v.cli.as_deref(), Some("0.155.0"));
        assert_eq!(v.app_server, None);
        assert_eq!(parse_daemon_version("not json"), DaemonVersions::default());
    }

    #[test]
    fn parses_cli_banner() {
        assert_eq!(parse_cli_version("codex-cli 0.153.4\n").as_deref(), Some("0.153.4"));
        assert_eq!(parse_cli_version("codex-cli\n"), None);
    }

    #[test]
    fn decision_table() {
        assert_eq!(decide(Some("0.155.0"), Some("0.155.0"), Some(false)), Decision::Nothing);
        assert_eq!(decide(Some("0.155.0"), Some("0.155.0"), Some(true)), Decision::Nothing);
        assert_eq!(decide(None, Some("0.155.0"), Some(false)), Decision::Nothing);
        assert_eq!(
            decide(Some("0.153.4"), Some("0.155.0"), Some(false)),
            cycle("0.153.4", "0.155.0", false)
        );
        assert_eq!(
            decide(Some("0.153.4"), Some("0.155.0"), Some(true)),
            deferred("0.153.4", "0.155.0")
        );
        assert_eq!(decide(Some("0.153.4"), Some("0.155.0"), None), deferred("0.153.4", "0.155.0"));
    }

    #[test]
    fn unknown_busy_escalates_after_the_window() {
        let mut g = CodexVersionGate::default();
        let t0 = Instant::now();
        let d = || deferred("0.153.4", "0.155.0");
        assert_eq!(g.escalate(d(), None, t0), d());
        assert_eq!(g.escalate(d(), None, t0 + ESCALATE_AFTER), cycle("0.153.4", "0.155.0", true));
    }

    #[test]
    fn a_turn_in_flight_never_escalates_and_resets_the_clock() {
        let mut g = CodexVersionGate::default();
        let t0 = Instant::now();
        let d = || deferred("0.153.4", "0.155.0");
        g.escalate(d(), None, t0);
        assert_eq!(g.escalate(d(), Some(true), t0 + ESCALATE_AFTER), d());
        assert_eq!(g.escalate(d(), None, t0 + ESCALATE_AFTER + Duration::from_secs(1)), d());
        assert_eq!(
            g.escalate(d(), None, t0 + ESCALATE_AFTER * 2),
            cycle("0.153.4", "0.155.0", true)
        );
    }

    #[test]
    fn a_new_pair_or_a_resolution_restarts_the_clock() {
        let mut g = CodexVersionGate::default();
        let t0 = Instant::now();
        g.escalate(deferred("0.153.4", "0.155.0"), None, t0);
        assert_eq!(
            g.escalate(deferred("0.153.4", "0.156.0"), None, t0 + ESCALATE_AFTER),
            deferred("0.153.4", "0.156.0")
        );
        g.escalate(Decision::Nothing, None, t0 + ESCALATE_AFTER);
        assert_eq!(
            g.escalate(deferred("0.153.4", "0.156.0"), None, t0 + ESCALATE_AFTER * 2),
            deferred("0.153.4", "0.156.0")
        );
    }

    #[test]
    fn check_reads_app_server_as_running_and_cli_as_local() {
        let mut g = CodexVersionGate::default();
        let v = parse_daemon_version(RUNNING);
        assert_eq!(g.check(&v, Some(false), Instant::now()), cycle("0.153.4", "0.155.0", false));
    }
}
