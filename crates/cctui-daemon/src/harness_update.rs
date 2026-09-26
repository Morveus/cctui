//! Opt-in periodic `claude update` / `codex update`, then cycle the harness
//! process when idle.
//!
//! Claude's cycle is left to the adapter's `version_gate`,
//! which already cycles a `claude daemon` older than the CLI; codex goes
//! through `codex_version_gate`.
//!
//! Never restarts `cctui-daemon` or touches its unit. Worker pods report
//! `managed-by-image` and never update: their harness is the baked image.
//! A failure waits for the next interval like a success does.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use cctui_proto::harness::{
    HARNESS_CLAUDE_CODE, HARNESS_CODEX, HarnessOutcome, HarnessReport, HarnessUpdatePolicy,
    HarnessVersion, HarnessVersions,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::adapters::claude_code::version_gate as claude_gate;
use crate::adapters::codex::codex_version_gate::{self as codex_gate, CodexVersionGate, Decision};

const TICK: Duration = Duration::from_mins(1);
const UPDATE_TIMEOUT: Duration = Duration::from_mins(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const STATE_FILE: &str = "harness-update.json";
const OUTCOME_MAX_CHARS: usize = 300;

static POLICY: LazyLock<watch::Sender<Option<HarnessUpdatePolicy>>> =
    LazyLock::new(|| watch::Sender::new(None));

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::load()));

pub fn set_policy(policy: HarnessUpdatePolicy) {
    let policy = policy.normalized();
    POLICY.send_if_modified(|held| {
        if held.as_ref() == Some(&policy) {
            return false;
        }
        tracing::info!(?policy, "harness auto-update policy received");
        *held = Some(policy);
        true
    });
}

#[must_use]
pub fn report() -> HarnessReport {
    let state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    HarnessReport {
        policy: POLICY.borrow().clone(),
        versions: state.versions.clone(),
        outcomes: state.outcomes.values().cloned().collect(),
        managed_by_image: in_worker_pod(),
    }
}

/// `CCTUI_HARNESS_AUTOUPDATE=0` forces the feature off on this machine,
/// whatever the server says.
#[must_use]
pub fn kill_switch() -> bool {
    std::env::var("CCTUI_HARNESS_AUTOUPDATE").is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
    })
}

/// A dispatched worker: its machine key comes from the environment, or it
/// runs under Kubernetes.
#[must_use]
pub fn in_worker_pod() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty());
    set("CCTUI_MACHINE_KEY") || set("KUBERNETES_SERVICE_HOST")
}

/// The policy this daemon acts on: env kill switch, then worker pod, then
/// the server's (already machine-over-instance resolved) policy; off when
/// nothing was received.
#[must_use]
pub fn local_policy(
    server: Option<&HarnessUpdatePolicy>,
    kill_switch: bool,
    worker_pod: bool,
) -> Option<HarnessUpdatePolicy> {
    if kill_switch || worker_pod {
        return None;
    }
    server.filter(|p| p.enabled).cloned()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    last_run: BTreeMap<String, DateTime<Utc>>,
    #[serde(default)]
    outcomes: BTreeMap<String, HarnessOutcome>,
    #[serde(default)]
    versions: HarnessVersions,
}

impl State {
    fn load() -> Self {
        crate::runtime::state_candidates(STATE_FILE)
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn persist(&self) {
        let Ok(json) = serde_json::to_string_pretty(self) else { return };
        if crate::runtime::record_at(&crate::runtime::state_candidates(STATE_FILE), &json).is_none()
        {
            tracing::debug!("failed to persist harness update state");
        }
    }
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    let out = f(&mut state);
    state.persist();
    out
}

fn record(harness: &str, outcome: String) {
    tracing::info!(harness, %outcome, "harness auto-update");
    with_state(|s| {
        s.outcomes.insert(
            harness.to_owned(),
            HarnessOutcome { harness: harness.to_owned(), outcome, at: Utc::now() },
        );
    });
}

fn record_versions(harness: &str, version: HarnessVersion) {
    with_state(|s| match harness {
        HARNESS_CLAUDE_CODE => s.versions.claude_code = Some(version),
        HARNESS_CODEX => s.versions.codex = Some(version),
        _ => {}
    });
}

#[must_use]
pub fn due(last: Option<DateTime<Utc>>, now: DateTime<Utc>, interval_hours: u32) -> bool {
    last.is_none_or(|t| now - t >= chrono::Duration::hours(i64::from(interval_hours)))
}

/// Outcome of one updater run, from the CLI version before and after and the
/// process result. The version comparison decides; the output only explains
/// a failure.
#[must_use]
pub fn classify(
    before: Option<&str>,
    after: Option<&str>,
    success: bool,
    stdout: &str,
    stderr: &str,
) -> String {
    if !success {
        return format!(
            "failed: {}",
            last_line(stderr).or_else(|| last_line(stdout)).unwrap_or("updater exited non-zero")
        );
    }
    match (before, after) {
        (Some(a), Some(b)) if a != b => format!("updated {a}→{b}"),
        (_, Some(_)) => "up to date".to_owned(),
        (_, None) => "failed: version unreadable after update".to_owned(),
    }
}

fn last_line(text: &str) -> Option<&str> {
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    Some(line.char_indices().nth(OUTCOME_MAX_CHARS).map_or(line, |(i, _)| &line[..i]))
}

pub type BusyProbe =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = codex_gate::Busy> + Send>> + Send + Sync>;

pub struct Runner {
    claude_bin: String,
    codex_bin: String,
    codex_busy: BusyProbe,
    codex_gate: CodexVersionGate,
}

impl Runner {
    #[must_use]
    pub fn new(claude_bin: String, codex_bin: String, codex_busy: BusyProbe) -> Self {
        Self { claude_bin, codex_bin, codex_busy, codex_gate: CodexVersionGate::default() }
    }

    fn bin(&self, harness: &str) -> &str {
        if harness == HARNESS_CODEX { &self.codex_bin } else { &self.claude_bin }
    }

    /// One pass: update every due harness in turn (never two at once), then
    /// give the codex gate a chance to cycle a stale app-server.
    pub async fn tick(&mut self, policy: &HarnessUpdatePolicy) {
        for harness in &policy.harnesses {
            let last =
                STATE.lock().unwrap_or_else(PoisonError::into_inner).last_run.get(harness).copied();
            if !due(last, Utc::now(), policy.interval_hours) {
                continue;
            }
            with_state(|s| s.last_run.insert(harness.clone(), Utc::now()));
            self.update(harness).await;
        }
        if policy.covers(HARNESS_CODEX) {
            self.cycle_codex().await;
        }
    }

    async fn cli_version(&self, harness: &str) -> Option<String> {
        let out = run(self.bin(harness), &["--version"], PROBE_TIMEOUT).await.ok()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if harness == HARNESS_CODEX {
            codex_gate::parse_cli_version(&stdout)
        } else {
            claude_gate::parse_cli_version(&stdout)
        }
    }

    async fn update(&self, harness: &str) {
        let Some(before) = self.cli_version(harness).await else {
            record(harness, "not installed".to_owned());
            return;
        };
        let outcome = match run(self.bin(harness), &["update"], UPDATE_TIMEOUT).await {
            Ok(out) => {
                let after = self.cli_version(harness).await;
                classify(
                    Some(&before),
                    after.as_deref(),
                    out.status.success(),
                    &String::from_utf8_lossy(&out.stdout),
                    &String::from_utf8_lossy(&out.stderr),
                )
            }
            Err(err) => format!("failed: {err}"),
        };
        record(harness, outcome);
        self.refresh_versions(harness).await;
    }

    async fn refresh_versions(&self, harness: &str) {
        let version = if harness == HARNESS_CODEX {
            let v = codex_gate::probe_versions(&self.codex_bin).await;
            HarnessVersion { cli: v.cli, daemon: v.app_server }
        } else {
            let daemon = run(&self.claude_bin, &["daemon", "status"], PROBE_TIMEOUT)
                .await
                .ok()
                .and_then(|o| {
                    claude_gate::parse_daemon_status(&String::from_utf8_lossy(&o.stdout)).version
                });
            HarnessVersion { cli: self.cli_version(harness).await, daemon }
        };
        record_versions(harness, version);
    }

    async fn cycle_codex(&mut self) {
        let versions = codex_gate::probe_versions(&self.codex_bin).await;
        if versions.cli.is_none() || versions.cli == versions.app_server {
            return;
        }
        let busy = (self.codex_busy)().await;
        match self.codex_gate.check(&versions, busy, std::time::Instant::now()) {
            Decision::Nothing => {}
            Decision::Deferred { .. } => {
                let already = STATE
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .outcomes
                    .get(HARNESS_CODEX)
                    .is_some_and(|o| o.outcome == "deferred: busy");
                if !already {
                    record(HARNESS_CODEX, "deferred: busy".to_owned());
                }
            }
            Decision::Cycle { running, local, escalated } => {
                tracing::info!(%running, %local, escalated, "cycling idle codex app-server onto the new CLI");
                let outcome = match codex_gate::restart(&self.codex_bin).await {
                    Ok(()) => format!("updated {running}→{local}"),
                    Err(err) => format!("failed: {err}"),
                };
                record(HARNESS_CODEX, outcome);
                self.refresh_versions(HARNESS_CODEX).await;
            }
        }
    }
}

async fn run(bin: &str, args: &[&str], timeout: Duration) -> anyhow::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .env("PATH", crate::childenv::child_path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
    tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("`{bin} {}` timed out", args.join(" ")))?
        .map_err(Into::into)
}

/// Background ticker. Idle until an enabled policy arrives; a daemon that
/// never receives one never runs anything.
pub fn spawn_loop(shutdown: CancellationToken) {
    let busy: BusyProbe = Arc::new(|| Box::pin(crate::adapters::codex::turns_in_flight()));
    let mut runner = Runner::new("claude".to_owned(), "codex".to_owned(), busy);
    let mut policy_rx = POLICY.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = tick.tick() => {}
                _ = policy_rx.changed() => {}
            }
            let server = policy_rx.borrow_and_update().clone();
            let Some(policy) = local_policy(server.as_ref(), kill_switch(), in_worker_pod()) else {
                continue;
            };
            runner.tick(&policy).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_UPDATED: &str = "\
Current version: 2.1.279
Checking for updates to latest version...
Successfully updated from 2.1.279 to version 2.1.280
";
    const CLAUDE_LATEST: &str = "\
Current version: 2.1.280
Checking for updates to latest version...
Claude Code is up to date (2.1.280)
";
    const CLAUDE_ERROR: &str = "\
Error: Failed to install update
Check permissions on /home/you/.local/share/claude
";
    const CODEX_UPDATED: &str =
        "Updating Codex via `npm install -g @openai/codex`...\nUpdated codex 0.153.4 -> 0.155.0\n";
    const CODEX_ERROR: &str = "Error: failed to download release: network unreachable\n";

    fn on(harnesses: &[&str]) -> HarnessUpdatePolicy {
        HarnessUpdatePolicy {
            enabled: true,
            interval_hours: 24,
            harnesses: harnesses.iter().map(|h| (*h).to_owned()).collect(),
        }
    }

    #[test]
    fn claude_update_outcomes() {
        assert_eq!(
            classify(Some("2.1.279"), Some("2.1.280"), true, CLAUDE_UPDATED, ""),
            "updated 2.1.279→2.1.280"
        );
        assert_eq!(
            classify(Some("2.1.280"), Some("2.1.280"), true, CLAUDE_LATEST, ""),
            "up to date"
        );
        assert_eq!(
            classify(Some("2.1.280"), Some("2.1.280"), false, "", CLAUDE_ERROR),
            "failed: Check permissions on /home/you/.local/share/claude"
        );
    }

    #[test]
    fn codex_update_outcomes() {
        assert_eq!(
            classify(Some("0.153.4"), Some("0.155.0"), true, CODEX_UPDATED, ""),
            "updated 0.153.4→0.155.0"
        );
        assert_eq!(classify(Some("0.155.0"), Some("0.155.0"), true, "", ""), "up to date");
        assert_eq!(
            classify(Some("0.153.4"), Some("0.153.4"), false, "", CODEX_ERROR),
            "failed: Error: failed to download release: network unreachable"
        );
        assert_eq!(
            classify(Some("0.153.4"), None, true, "", ""),
            "failed: version unreadable after update"
        );
        assert_eq!(classify(None, None, false, "", ""), "failed: updater exited non-zero");
    }

    #[test]
    fn failure_text_is_bounded() {
        let long = "x".repeat(OUTCOME_MAX_CHARS * 2);
        assert_eq!(
            classify(None, None, false, "", &long).len(),
            "failed: ".len() + OUTCOME_MAX_CHARS
        );
    }

    #[test]
    fn policy_precedence() {
        let server = on(&["codex"]);
        assert_eq!(local_policy(Some(&server), false, false), Some(server.clone()));
        assert_eq!(local_policy(Some(&server), true, false), None, "env kill switch wins");
        assert_eq!(local_policy(Some(&server), false, true), None, "worker pods never self-update");
        assert_eq!(local_policy(None, false, false), None, "off when nothing was received");
        let off = HarnessUpdatePolicy { enabled: false, ..server };
        assert_eq!(local_policy(Some(&off), false, false), None);
        let resolved = HarnessUpdatePolicy::resolve(
            Some(&on(&["codex"])),
            Some(&HarnessUpdatePolicy::default()),
        );
        assert_eq!(
            local_policy(Some(&resolved), false, false),
            None,
            "machine override beats instance default"
        );
    }

    #[test]
    fn interval_gates_reruns() {
        let t0 = Utc::now();
        assert!(due(None, t0, 24));
        assert!(!due(Some(t0), t0 + chrono::Duration::hours(23), 24));
        assert!(due(Some(t0), t0 + chrono::Duration::hours(24), 24));
    }

    /// Fake `codex` on disk: `update` bumps the CLI version file, `app-server
    /// daemon restart` copies it to the app-server file and logs the call.
    #[cfg(unix)]
    fn fake_codex(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("codex");
        let d = dir.display();
        std::fs::write(dir.join("cli"), "0.153.4").unwrap();
        std::fs::write(dir.join("server"), "0.153.4").unwrap();
        std::fs::write(
            &bin,
            format!(
                r#"#!/bin/sh
case "$*" in
  --version) echo "codex-cli $(cat {d}/cli)" ;;
  update) echo 0.155.0 > {d}/cli; echo "Updated" ;;
  "app-server daemon version") printf '{{"status":"running","cliVersion":"%s","appServerVersion":"%s"}}\n' "$(cat {d}/cli)" "$(cat {d}/server)" ;;
  "app-server daemon restart") cp {d}/cli {d}/server; echo restart >> {d}/log ;;
  *) exit 2 ;;
esac
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "spawns fake harness shims and writes the shared harness-update state file"]
    async fn update_then_cycle_only_when_idle() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_codex(dir.path());
        let busy_flag = Arc::new(AtomicBool::new(true));
        let flag = busy_flag.clone();
        let probe: BusyProbe = Arc::new(move || {
            let busy = flag.load(Ordering::SeqCst);
            Box::pin(async move { Some(busy) })
        });
        let mut runner =
            Runner::new("claude-not-installed".into(), bin.display().to_string(), probe);
        with_state(|s| {
            s.last_run.remove(HARNESS_CODEX);
        });
        let policy = on(&[HARNESS_CODEX]);
        let log = dir.path().join("log");

        runner.tick(&policy).await;
        assert!(!log.exists(), "busy: no restart");
        assert_eq!(
            report().outcomes.iter().find(|o| o.harness == HARNESS_CODEX).unwrap().outcome,
            "deferred: busy"
        );

        busy_flag.store(false, Ordering::SeqCst);
        runner.tick(&policy).await;
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1, "idle: one restart");
        let r = report();
        assert_eq!(
            r.outcomes.iter().find(|o| o.harness == HARNESS_CODEX).unwrap().outcome,
            "updated 0.153.4→0.155.0"
        );
        let codex = r.versions.codex.unwrap();
        assert_eq!(codex.cli.as_deref(), Some("0.155.0"));
        assert_eq!(codex.daemon.as_deref(), Some("0.155.0"));

        runner.tick(&policy).await;
        assert_eq!(
            std::fs::read_to_string(&log).unwrap().lines().count(),
            1,
            "not due, in sync: nothing"
        );
    }
}
