//! Oneshot stream-json driver for the claude-code adapter.
//!
//! Runs claude as a one-shot `claude -p <prompt> --output-format stream-json
//! --verbose` invocation per turn, mapped onto the
//! [`AdapterCommand`](cctui_proto::adapter::AdapterCommand) /
//! [`AdapterEvent`](cctui_proto::adapter::AdapterEvent) surface so the server is
//! oblivious to the mode. It reuses the shared stream-json codec
//! ([`super::streamjson`]) for event mapping and the same ask/permission hook
//! listener ([`super::run_hook_listener`]) the `bg` driver uses — headless runs
//! fire `PreToolUse`/`AskUserQuestion` hooks just like an interactive worker.
//!
//! Lifecycle, per the design (`sub4-oneshot-driver.md`):
//!
//! - **Spawn** → `claude -p <prompt> --output-format stream-json --verbose
//!   --session-id <pre-minted uuid> [--model][--effort] [--permission-mode]
//!   [--settings <hook>]`, run in `spec.working_dir`. The pre-minted session id
//!   flows from `Spawn.session_id` exactly as `bg` uses it so the gateway-token
//!   binding stays intact. On the terminal `result` frame the
//!   driver emits an idle [`AdapterEvent::Status`] (NOT `SessionEnded`) so the
//!   conversation stays resumable, mirroring how `--bg` idles awaiting input.
//! - **Reply** → re-invoke `claude -p <text> --resume <session_id>`, a fresh
//!   child against the same id. Gateway env carried on the command is injected
//!   (cold-launch parity).
//! - **Fork** → `claude -p --resume <parent> --fork-session --session-id
//!   <child>` (optional first-turn prompt).
//! - **Resume** → revive without a reply: a no-op turn against `--resume`.
//! - **Kill / Interrupt** → terminate the in-flight child; the conversation
//!   stays resumable (oneshot has no live mid-turn turn to ESC into, so
//!   `Interrupt` == terminate the current `-p` process).
//! - **`PermissionResponse` / Ask / Plan** → through the reused `--settings` hook
//!   path; native keystroke answering is N/A for headless, so the perm hook's
//!   long-poll allow/deny carries the decision.
//! - **Rename** → stored daemon-side (no PTY/state.json round-trip).
//!   **Remove** → no worker to stop; clear local state.
//!   **`SetModel`** → unsupported in place (same "fork to change model" as bg).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context as _;
use cctui_proto::adapter::{AdapterCommand, AdapterEvent, EndReason, SessionMeta, SessionSpec};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::control::DriverConfig;
use super::streamjson::{self, LaunchArgs};
use super::{PendingAsks, PendingPermHooks, SessionMap};
use crate::adapter_runtime::AdapterCtx;

/// A single-shot `claude -p` driver.
///
/// Unlike the `bg` driver (which talks to a long-lived `claude daemon` control
/// socket), this driver owns each child process directly: one transient `claude
/// -p` per turn. It keeps no roster poll — session liveness is "is a child
/// currently running for this id".
pub(super) struct OneshotDriver {
    cfg: DriverConfig,
    events: mpsc::Sender<AdapterEvent>,
    commands: mpsc::Receiver<AdapterCommand>,
    shutdown: CancellationToken,
    server: Option<crate::client::ServerClient>,
    machine_key: Option<String>,
    /// `session_id → live child` for sessions with a turn in flight, so
    /// `Kill`/`Interrupt` can terminate the running `-p` process.
    running: HashMap<String, Child>,
    /// Daemon-side names (oneshot has no PTY/state.json to persist into).
    names: HashMap<String, String>,
    /// Working dir per session, captured at spawn so a later `Resume`/`Reply`
    /// re-invokes in the right place even without an on-disk job state.
    cwds: HashMap<String, String>,
    /// Per-session launch posture (permission flag / settings) captured at
    /// spawn so resume/reply turns keep the same hook wiring.
    settings_path: Option<String>,
    /// Shared maps for the ask/permission hook listener. The listener resolves
    /// the live `session_id` a hook reports through [`SessionMap`]; oneshot keys
    /// on the full session id so we register an identity mapping per session.
    session_map: SessionMap,
    pending_asks: PendingAsks,
    pending_perm_hooks: PendingPermHooks,
}

impl OneshotDriver {
    pub(super) fn new(ctx: AdapterCtx) -> Self {
        let cfg = DriverConfig::from_value(&ctx.config);
        Self {
            cfg,
            events: ctx.events,
            commands: ctx.commands,
            shutdown: ctx.shutdown,
            server: ctx.server,
            machine_key: ctx.machine_key,
            running: HashMap::new(),
            names: HashMap::new(),
            cwds: HashMap::new(),
            settings_path: None,
            session_map: Arc::default(),
            pending_asks: Arc::default(),
            pending_perm_hooks: Arc::default(),
        }
    }

    // Top-level driver event loop (hook listener + select over shutdown / commands
    // / turn completion); complexity is the per-branch dispatch, not nesting.
    // Splitting the select arms would obscure the loop's lifecycle.
    #[allow(clippy::cognitive_complexity)]
    pub(super) async fn run(mut self) -> anyhow::Result<()> {
        tracing::info!("claude-code adapter starting in oneshot mode");
        // The same ask/permission hook listener the bg driver uses: headless
        // `-p` runs fire PreToolUse/AskUserQuestion hooks, so
        // bind the local socket the injected `--settings` file targets and route
        // deliveries through the shared maps. Spawned as a sibling task; it
        // exits on the shared shutdown token.
        self.spawn_hook_listener();

        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    self.kill_all().await;
                    return Ok(());
                }
                cmd = self.commands.recv() => {
                    let Some(cmd) = cmd else {
                        // Sender closed: drain to shutdown.
                        self.shutdown.cancelled().await;
                        self.kill_all().await;
                        return Ok(());
                    };
                    let command_id = cmd.command_id();
                    let res = self.handle_command(cmd).await;
                    if let Some(command_id) = command_id {
                        let (ok, error) = match &res {
                            Ok(()) => (true, None),
                            Err(err) => (false, Some(err.to_string())),
                        };
                        let _ = self
                            .events
                            .send(AdapterEvent::CommandResult { command_id, ok, error })
                            .await;
                    }
                    if let Err(err) = res {
                        tracing::warn!(%err, "oneshot command dispatch failed");
                    }
                }
            }
        }
    }

    /// Bind the shared ask/permission hook socket and route deliveries through
    /// the same handler the bg driver uses.
    fn spawn_hook_listener(&self) {
        let sock = self.cfg.hook_socket_path.clone();
        let events = self.events.clone();
        let shutdown = self.shutdown.clone();
        let session_map = self.session_map.clone();
        let pending_asks = self.pending_asks.clone();
        let pending_perm_hooks = self.pending_perm_hooks.clone();
        tokio::spawn(async move {
            if let Err(err) = super::run_hook_listener(
                sock,
                events,
                shutdown,
                session_map,
                pending_asks,
                pending_perm_hooks,
                // These drivers don't serve the diagnose aggregation
                // (is bg-only); a fresh log satisfies the listener.
                super::HookLog::default(),
            )
            .await
            {
                tracing::warn!(%err, "claude-code oneshot ask-hook listener exited");
            }
        });
    }

    // Dispatch over every `AdapterCommand` variant; complexity is the breadth of
    // the match arms, not nesting. Per-arm helpers would be pure churn.
    #[allow(clippy::cognitive_complexity)]
    async fn handle_command(&mut self, cmd: AdapterCommand) -> anyhow::Result<()> {
        match cmd {
            AdapterCommand::Spawn { spec, session_id, .. } => {
                let session_id = session_id
                    .map_or_else(|| uuid::Uuid::new_v4().to_string(), |id| id.to_string());
                self.spawn(&spec, session_id, None).await
            }
            AdapterCommand::Fork { parent_local_id, spec, session_id, .. } => {
                let child_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                self.spawn(&spec, child_id, Some(parent_local_id)).await
            }
            AdapterCommand::Reply { local_id, text, env, .. } => {
                self.reply(&local_id, &text, env).await
            }
            AdapterCommand::SendMessage { local_id, text } => {
                self.reply(&local_id, &text, std::collections::BTreeMap::new()).await
            }
            AdapterCommand::Resume { local_id, working_dir, .. } => {
                self.resume(&local_id, working_dir).await
            }
            AdapterCommand::Kill { local_id, .. } | AdapterCommand::Interrupt { local_id, .. } => {
                self.kill(&local_id).await;
                Ok(())
            }
            AdapterCommand::Remove { local_id, .. } => {
                self.kill(&local_id).await;
                self.names.remove(&local_id);
                self.cwds.remove(&local_id);
                self.forget(&local_id);
                self.emit(AdapterEvent::SessionEnded { local_id, reason: EndReason::Killed }).await;
                Ok(())
            }
            AdapterCommand::PermissionResponse { local_id, request_id, allow } => {
                // Headless: the only carrier is the bidirectional PreToolUse
                // hook long-polling in the listener. Hand it the decision; there
                // is no PTY keystroke fallback in oneshot mode.
                let hook =
                    self.pending_perm_hooks.lock().ok().and_then(|mut m| m.remove(&local_id));
                if let Some(tx) = hook {
                    if tx.send(allow).is_ok() {
                        tracing::info!(%local_id, %request_id, allow, "oneshot answered permission via hook");
                        return Ok(());
                    }
                    tracing::debug!(%local_id, %request_id, "oneshot perm hook receiver gone");
                } else {
                    tracing::warn!(%local_id, %request_id, "oneshot: no pending permission hook to answer");
                }
                Ok(())
            }
            AdapterCommand::Rename { local_id, name } => {
                self.names.insert(local_id, name);
                Ok(())
            }
            AdapterCommand::SetModel { local_id, .. } => {
                tracing::warn!(%local_id, "claude oneshot: in-place model/effort switch not supported; fork to change model");
                anyhow::bail!(
                    "in-place model/effort switch is not supported for claude sessions — fork to change model"
                );
            }
            _ => {
                tracing::warn!("oneshot: unhandled AdapterCommand variant");
                Ok(())
            }
        }
    }

    /// Spawn a brand-new (or forked) conversation as a first `-p` turn.
    async fn spawn(
        &mut self,
        spec: &SessionSpec,
        session_id: String,
        fork_parent: Option<String>,
    ) -> anyhow::Result<()> {
        let cwd = spec
            .working_dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("spawn: working_dir required"))?
            .to_owned();
        if !std::path::Path::new(&cwd).is_dir() {
            anyhow::bail!("spawn: working_dir does not exist or is not a directory: {cwd}");
        }

        // Resolve gateway env + per-account settings (539/540) BEFORE
        // writing the hook-settings file, so the account settings can be
        // deep-merged under the managed hooks. Fail-closed inside
        // `resolve_launch_env`.
        let launch_env = self.resolve_launch_env(&session_id, &spec.env).await?;

        // Ensure the ask/permission hook is wired for this run (idempotent),
        // per-session so distinct sessions don't clobber each other's settings.
        let whip = spec.permission_mode.is_some_and(cctui_proto::adapter::PermissionMode::is_whip);
        let short = &session_id[..8.min(session_id.len())];
        let settings = super::control::ensure_hook_settings(
            &self.cfg.hook_socket_path,
            whip,
            short,
            launch_env.settings.as_ref(),
            &launch_env.env,
            None,
            None,
            launch_env.whip_phrases.as_ref(),
            None,
        )
        .map(|p| p.to_string_lossy().into_owned());
        self.settings_path.clone_from(&settings);

        let mut launch = LaunchArgs::from_spec(spec, settings);
        launch.session_id = Some(session_id.clone());
        if let Some(parent) = &fork_parent {
            launch.resume_from = Some(parent.clone());
            launch.fork = true;
        }

        // Register the session id in the hook map (identity) so a hook reporting
        // this live session id resolves to the same local_id we key on, and pin
        // the cwd for later resume/reply turns.
        self.register(&session_id, &cwd);

        // Announce the session before its first events so the server has a row.
        self.emit(AdapterEvent::SessionStarted {
            local_id: session_id.clone(),
            meta: SessionMeta {
                working_dir: Some(cwd.clone()),
                parent_local_id: fork_parent,
                extra: serde_json::Value::Null,
            },
        })
        .await;

        let env = launch_env.env;
        let prompt = spec.prompt.clone().unwrap_or_default();
        self.launch_turn(&session_id, &cwd, launch.to_argv(), &prompt, &env).await
    }

    /// Continue a conversation with another `-p` turn (`--resume <id>`).
    async fn reply(
        &mut self,
        local_id: &str,
        text: &str,
        env_hint: std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let cwd = self
            .cwds
            .get(local_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("reply: unknown session {local_id}"))?;
        let launch = LaunchArgs {
            resume_from: Some(local_id.to_owned()),
            settings_path: self.settings_path.clone(),
            ..LaunchArgs::default()
        };
        let env = self.resolve_launch_env(local_id, &env_hint).await?.env;
        self.launch_turn(local_id, &cwd, launch.to_argv(), text, &env).await
    }

    /// Revive an exited-but-resumable conversation without a reply: a no-op
    /// `--resume` turn with an empty prompt.
    async fn resume(&mut self, local_id: &str, working_dir: Option<String>) -> anyhow::Result<()> {
        let cwd = working_dir
            .or_else(|| self.cwds.get(local_id).cloned())
            .ok_or_else(|| anyhow::anyhow!("resume: no working_dir for {local_id}"))?;
        self.register(local_id, &cwd);
        let launch = LaunchArgs {
            resume_from: Some(local_id.to_owned()),
            settings_path: self.settings_path.clone(),
            ..LaunchArgs::default()
        };
        let env = self.resolve_launch_env(local_id, &std::collections::BTreeMap::new()).await?.env;
        self.launch_turn(local_id, &cwd, launch.to_argv(), "", &env).await
    }

    /// Spawn one `claude -p` child, stream its stdout through the shared codec,
    /// forward events, and on the terminal frame emit an idle `Status` so the
    /// session stays resumable. Blocks until the turn completes (or shutdown).
    // Linear spawn → stream → forward → terminal-frame pipeline with a select over
    // shutdown / child output; complexity is the per-line/per-event handling, not
    // nesting. Splitting would fragment the single-turn lifecycle.
    #[allow(clippy::cognitive_complexity)]
    async fn launch_turn(
        &mut self,
        local_id: &str,
        cwd: &str,
        shared_args: Vec<String>,
        prompt: &str,
        env: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut command = Command::new(&self.cfg.claude_bin);
        command
            .current_dir(cwd)
            .arg("--print")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .args(&shared_args)
            .arg("--")
            .arg(prompt)
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::childenv::ScrubChildEnv::scrub_child_env(&mut command);

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning `{} -p` in {cwd}", self.cfg.claude_bin))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_ring =
            child.stderr.take().map(|stderr| streamjson::spawn_stderr_ring(stderr, "oneshot"));
        // Register so Kill/Interrupt can terminate this turn.
        self.running.insert(local_id.to_owned(), child);

        let mut reader = BufReader::new(stdout).lines();
        let mut crashed: Option<EndReason> = None;
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    self.kill(local_id).await;
                    return Ok(());
                }
                line = reader.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            let mut out = Vec::new();
                            let outcome = streamjson::parse_stream_line(local_id, &line, &mut out);
                            for evt in out {
                                self.emit(evt).await;
                            }
                            if let Some(end) = outcome.end {
                                if let EndReason::Crashed { .. } = end {
                                    crashed = Some(end);
                                }
                                break;
                            }
                        }
                        // EOF: child closed stdout — turn done.
                        Ok(None) => break,
                        Err(err) => {
                            tracing::warn!(%err, %local_id, "oneshot stdout read error");
                            break;
                        }
                    }
                }
            }
        }

        // Reap the child and clear the running slot.
        if let Some(mut child) = self.running.remove(local_id) {
            match child.wait().await {
                Ok(status) if !status.success() && crashed.is_none() => {
                    let tail = match &stderr_ring {
                        Some(ring) => streamjson::stderr_tail(ring).await,
                        None => String::new(),
                    };
                    crashed = Some(EndReason::Crashed {
                        detail: streamjson::exit_detail("claude -p", status, &tail),
                    });
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, %local_id, "oneshot child wait failed"),
            }
        }

        if let Some(reason) = crashed {
            // A failed turn ends the session; the transcript still resumes, but
            // surface the failure rather than masking it as idle.
            self.emit(AdapterEvent::SessionEnded { local_id: local_id.to_owned(), reason }).await;
        } else {
            // Successful turn: keep the session resumable (do NOT SessionEnded).
            // Mirror how --bg idles awaiting input.
            self.emit(idle_status(local_id)).await;
        }
        Ok(())
    }

    /// Pull the session's gateway-routing env from the server,
    /// merging the carried `hint`. Fail-closed when account-bound but
    /// unmintable; best-effort hint when no server is configured.
    async fn resolve_launch_env(
        &self,
        local_id: &str,
        hint: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<super::control::LaunchEnv> {
        super::control::resolve_launch_env_for(
            self.server.as_ref(),
            self.machine_key.as_ref(),
            local_id,
            hint,
        )
        .await
    }

    /// Terminate the in-flight child for `local_id`, if any.
    async fn kill(&mut self, local_id: &str) {
        if let Some(mut child) = self.running.remove(local_id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            tracing::info!(%local_id, "oneshot killed in-flight turn");
        }
    }

    async fn kill_all(&mut self) {
        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            self.kill(&id).await;
        }
    }

    /// Register a session id in the hook map (identity mapping) and pin its cwd.
    fn register(&mut self, session_id: &str, cwd: &str) {
        self.cwds.insert(session_id.to_owned(), cwd.to_owned());
        if let Ok(mut m) = self.session_map.lock() {
            m.insert(session_id.to_owned(), session_id.to_owned());
        }
    }

    fn forget(&self, session_id: &str) {
        if let Ok(mut m) = self.session_map.lock() {
            m.remove(session_id);
        }
    }

    async fn emit(&self, evt: AdapterEvent) {
        let _ = self.events.send(evt).await;
    }
}

/// The idle [`AdapterEvent::Status`] emitted after a successful oneshot turn:
/// the conversation is done with this turn but stays resumable (mirrors how the
/// bg worker idles awaiting input).
fn idle_status(local_id: &str) -> AdapterEvent {
    AdapterEvent::Status {
        local_id: local_id.to_owned(),
        tempo: Some("idle".to_owned()),
        state: Some("done".to_owned()),
        detail: None,
        activity: None,
        name: None,
        intent: None,
        model: None,
        effort: None,
        permission_mode: None,
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_status_is_resumable_not_ended() {
        // A successful oneshot turn must keep the session resumable: an idle
        // Status, never a SessionEnded.
        match idle_status("sid-1") {
            AdapterEvent::Status { local_id, tempo, state, .. } => {
                assert_eq!(local_id, "sid-1");
                assert_eq!(tempo.as_deref(), Some("idle"));
                assert_eq!(state.as_deref(), Some("done"));
            }
            other => panic!("expected idle Status, got {other:?}"),
        }
    }
}
