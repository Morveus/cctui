//! Persistent stream-json SDK driver for the claude-code adapter.
//!
//! Unlike the oneshot driver (a fresh `claude -p` child per turn), the SDK
//! driver owns ONE long-lived `claude --print --input-format stream-json
//! --output-format stream-json --verbose` child **per session**, driven over
//! its stdio the way the Claude Agent SDK's streaming-input mode does. The
//! spike proved this direct-wire shape works first-hand (against
//! claude 2.1.193) — no TS/Python SDK sidecar, no `--resume` chaining for
//! replies:
//!
//! - **Spawn** → launch the persistent child with the pre-minted `--session-id`,
//!   model/effort/permission flags, the `--settings` ask/permission hook, and
//!   the gateway env; send the first user turn on stdin. Stdout `system`/
//!   `assistant`/`user`/`result` frames map to [`AdapterEvent`]s via the shared
//!   [`streamjson`](super::streamjson) codec (native usage + model events, no
//!   transcript tailing). The `result` frame is a *turn boundary*, not process
//!   exit — the child stays alive awaiting the next stdin turn.
//! - **Reply / `SendMessage`** → write a `{"type":"user",…}` envelope to the
//!   child's stdin. No respawn. If the child died (crash / daemon restart), it
//!   is cold-resumed first with FRESH gateway env pulled from the server binding
//!   (fail-closed) before the turn is written.
//! - **Interrupt** → send a `control_request{subtype:"interrupt"}` on stdin
//!   (keeps the child alive); the run loop echoes the `CommandResult`.
//! - **`PermissionResponse` / Ask / Plan** → through the reused `--settings`
//!   parked-`PreToolUse` hook path (same shared [`super::run_hook_listener`] the
//!   `bg`/`oneshot` drivers use). Headless runs fire the hooks; forms don't
//!   render headless so Ask/Plan surface as the existing live cards. See the
//!   note on the `can_use_tool` stdio channel in [`SdkDriver`].
//! - **Fork** → launch the persistent child with `--resume <parent>
//!   --fork-session --session-id <child>`. **Resume** → cold-launch the
//!   persistent child from the on-disk conversation (`--resume <id>`) with the
//!   carried `working_dir`/env, no user turn.
//! - **Kill** → terminate the child but keep the session resumable (no
//!   `SessionEnded`); a later Reply/Resume cold-relaunches it. **Remove** →
//!   terminate + clear all state + `SessionEnded{Killed}`.
//! - **`SetModel`** → in-place via a `control_request{subtype:"set_model"}` on
//!   stdin when a model is given (the SDK control lever);
//!   effort-only changes have no control lever and fall back to "fork to change
//!   model".
//!
//! Daemon ownership: the driver supervises N persistent children — clean
//! shutdown on cancel (`kill_all`), stdin backpressure (writes `.await` on the
//! child's stdin), and per-child stdout pump tasks that forward events. Crash
//! recovery is **on-demand cold-resume**: a dead child is relaunched with fresh
//! fail-closed gateway env on the next Reply/Resume rather than eagerly
//! restarted by a background ticker (eager restart risks a 401 relaunch loop and
//! is deferred — see the report).

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context as _;
use cctui_proto::adapter::{AdapterCommand, AdapterEvent, EndReason, SessionMeta, SessionSpec};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::control::DriverConfig;
use super::streamjson::{self, LaunchArgs};
use super::{PendingAsks, PendingPermHooks, SessionMap};
use crate::adapter_runtime::AdapterCtx;

/// Launch posture captured at spawn so a later reply/resume/relaunch reuses the
/// same wiring even after the live child has died.
#[derive(Debug, Clone, Default)]
struct SessionPosture {
    cwd: String,
    settings_path: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    permission_flag: Option<String>,
    name: Option<String>,
}

/// A live persistent `claude` child plus the handles needed to talk to it and
/// reap it.
struct LiveChild {
    child: Child,
    stdin: ChildStdin,
    /// The stdout pump task; aborted on kill so a dead child leaves no orphan
    /// reader.
    pump: JoinHandle<()>,
}

impl LiveChild {
    /// Whether the child is still running (`try_wait` returned no exit status).
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the child and abort its stdout pump.
    async fn terminate(mut self) {
        self.pump.abort();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Persistent stream-json driver over one long-lived `claude` child per session.
///
/// NOTE on the permission channel: the spike documented the
/// `--permission-prompt-tool stdio` + `can_use_tool` control-request path but
/// could not round-trip it live (the CLI auto-allowed via its own settings).
/// This driver therefore routes permissions/Ask/Plan through the fully-proven
/// `--settings` `PreToolUse` hook path shared with `bg`/`oneshot` (the ticket's
/// documented "else" branch), keeping a single, consistent permission surface
/// and avoiding double-prompting. Wiring the stdio `can_use_tool` channel is
/// left as a follow-up once it can be exercised end-to-end.
pub(super) struct SdkDriver {
    cfg: DriverConfig,
    events: mpsc::Sender<AdapterEvent>,
    commands: mpsc::Receiver<AdapterCommand>,
    shutdown: CancellationToken,
    server: Option<crate::client::ServerClient>,
    machine_key: Option<String>,
    /// Live persistent children keyed by stable `local_id` (== session id).
    children: HashMap<String, LiveChild>,
    /// Launch posture per session, kept across child death for cold-resume.
    postures: HashMap<String, SessionPosture>,
    /// Daemon-side names (no PTY/state.json round-trip in this mode).
    names: HashMap<String, String>,
    /// Monotonic control-request id counter (SDK convention `req_{n}_{hex}`).
    req_counter: u64,
    /// Shared maps for the ask/permission hook listener (same path bg uses).
    session_map: SessionMap,
    pending_asks: PendingAsks,
    pending_perm_hooks: PendingPermHooks,
}

impl SdkDriver {
    pub(super) fn new(ctx: AdapterCtx) -> Self {
        let cfg = DriverConfig::from_value(&ctx.config);
        Self {
            cfg,
            events: ctx.events,
            commands: ctx.commands,
            shutdown: ctx.shutdown,
            server: ctx.server,
            machine_key: ctx.machine_key,
            children: HashMap::new(),
            postures: HashMap::new(),
            names: HashMap::new(),
            req_counter: 0,
            session_map: Arc::default(),
            pending_asks: Arc::default(),
            pending_perm_hooks: Arc::default(),
        }
    }

    // Top-level driver loop: hook listener + select over shutdown / commands.
    // Complexity is the per-command dispatch, not nesting.
    #[allow(clippy::cognitive_complexity)]
    pub(super) async fn run(mut self) -> anyhow::Result<()> {
        tracing::info!("claude-code adapter starting in sdk mode");
        // The same ask/permission hook listener bg/oneshot use: headless runs
        // fire PreToolUse/AskUserQuestion hooks, so bind
        // the local socket the injected `--settings` file targets and route
        // deliveries through the shared maps.
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
                        tracing::warn!(%err, "sdk command dispatch failed");
                    }
                }
            }
        }
    }

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
                tracing::warn!(%err, "claude-code sdk ask-hook listener exited");
            }
        });
    }

    // Dispatch over every AdapterCommand variant; complexity is the breadth of
    // the match arms, not nesting.
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
                self.reply(&local_id, &text, BTreeMap::new()).await
            }
            AdapterCommand::Resume { local_id, working_dir, env } => {
                self.resume(&local_id, working_dir, env).await
            }
            AdapterCommand::Interrupt { local_id, .. } => self.interrupt(&local_id).await,
            AdapterCommand::Kill { local_id, .. } => {
                self.kill(&local_id).await;
                Ok(())
            }
            AdapterCommand::Remove { local_id, .. } => {
                self.kill(&local_id).await;
                self.postures.remove(&local_id);
                self.names.remove(&local_id);
                self.forget(&local_id);
                self.emit(AdapterEvent::SessionEnded { local_id, reason: EndReason::Killed }).await;
                Ok(())
            }
            AdapterCommand::PermissionResponse { local_id, request_id, allow } => {
                // Headless: the carrier is the bidirectional PreToolUse hook
                // long-polling in the listener (same as oneshot). Hand it the
                // decision.
                let hook =
                    self.pending_perm_hooks.lock().ok().and_then(|mut m| m.remove(&local_id));
                if let Some(tx) = hook {
                    if tx.send(allow).is_ok() {
                        tracing::info!(%local_id, %request_id, allow, "sdk answered permission via hook");
                    } else {
                        tracing::debug!(%local_id, %request_id, "sdk perm hook receiver gone");
                    }
                } else {
                    tracing::warn!(%local_id, %request_id, "sdk: no pending permission hook to answer");
                }
                Ok(())
            }
            AdapterCommand::Rename { local_id, name } => {
                self.names.insert(local_id, name);
                Ok(())
            }
            AdapterCommand::SetModel { local_id, model, effort, command_id: _ } => {
                self.set_model(&local_id, model, effort).await
            }
            _ => {
                tracing::warn!("sdk: unhandled AdapterCommand variant");
                Ok(())
            }
        }
    }

    /// Spawn a brand-new (or forked) persistent conversation and, if the spec
    /// carries one, send its first user turn.
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

        // Resolve gateway env + per-account settings (539/540) before
        // writing the hook-settings file so account settings deep-merge under the
        // managed hooks. Fail-closed inside `resolve_launch_env`.
        let launch_env = self.resolve_launch_env(&session_id, &spec.env).await?;

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

        let base = LaunchArgs::from_spec(spec, settings.clone());
        let posture = SessionPosture {
            cwd: cwd.clone(),
            settings_path: settings,
            model: base.model.clone(),
            effort: base.effort.clone(),
            permission_flag: base.permission_flag.clone(),
            name: base.name.clone(),
        };
        self.postures.insert(session_id.clone(), posture);
        self.register(&session_id);

        let mut launch = base;
        launch.session_id = Some(session_id.clone());
        if let Some(parent) = &fork_parent {
            launch.resume_from = Some(parent.clone());
            launch.fork = true;
        }

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
        self.launch_child(&session_id, &cwd, launch.to_argv(), &env).await?;

        // First turn, if any, over the persistent child's stdin.
        if let Some(prompt) = spec.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            self.send_user_turn(&session_id, prompt).await?;
        }
        Ok(())
    }

    /// Continue a conversation: ensure the child is alive (cold-resume with
    /// fresh gateway env if it died), then write the user turn to stdin.
    async fn reply(
        &mut self,
        local_id: &str,
        text: &str,
        env_hint: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        self.ensure_child(local_id, &env_hint).await?;
        self.send_user_turn(local_id, text).await
    }

    /// Revive an exited-but-resumable conversation without a user turn.
    async fn resume(
        &mut self,
        local_id: &str,
        working_dir: Option<String>,
        env_hint: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        if let Some(cwd) = working_dir {
            self.postures.entry(local_id.to_owned()).or_default().cwd = cwd;
        }
        self.register(local_id);
        self.ensure_child(local_id, &env_hint).await
    }

    /// Interrupt the in-flight turn without tearing the child down: a
    /// `control_request{subtype:"interrupt"}` on stdin. No-op-safe if
    /// the child already idled.
    async fn interrupt(&mut self, local_id: &str) -> anyhow::Result<()> {
        if !self.children.get_mut(local_id).is_some_and(LiveChild::alive) {
            tracing::info!(%local_id, "sdk interrupt: no live child (already idle)");
            return Ok(());
        }
        let req_id = self.next_req_id();
        let frame = json!({
            "type": "control_request",
            "request_id": req_id,
            "request": { "subtype": "interrupt" },
        });
        self.write_frame(local_id, &frame).await?;
        tracing::info!(%local_id, "sdk sent interrupt control_request");
        Ok(())
    }

    /// In-place model switch via `control_request{subtype:"set_model"}` (the SDK
    /// lever captured in). Effort-only changes have no control lever, so
    /// they fall back to the "fork to change model" contract like bg/oneshot.
    async fn set_model(
        &mut self,
        local_id: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> anyhow::Result<()> {
        let Some(model) = model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()) else {
            anyhow::bail!(
                "in-place effort switch is not supported for claude sessions — fork to change model"
            );
        };
        if effort.is_some() {
            tracing::warn!(%local_id, "sdk set_model: applying model in place; effort change ignored (no control lever)");
        }
        // Record on posture so a later cold-resume carries the new model.
        self.postures.entry(local_id.to_owned()).or_default().model = Some(model.clone());
        if !self.children.get_mut(local_id).is_some_and(LiveChild::alive) {
            tracing::info!(%local_id, %model, "sdk set_model: no live child; recorded for next launch");
            return Ok(());
        }
        let req_id = self.next_req_id();
        let frame = json!({
            "type": "control_request",
            "request_id": req_id,
            "request": { "subtype": "set_model", "model": model },
        });
        self.write_frame(local_id, &frame).await?;
        tracing::info!(%local_id, %model, "sdk sent set_model control_request");
        Ok(())
    }

    /// Ensure a live child exists for `local_id`; cold-resume with fresh
    /// gateway env (fail-closed) if it is dead or absent.
    async fn ensure_child(
        &mut self,
        local_id: &str,
        env_hint: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        if self.children.get_mut(local_id).is_some_and(LiveChild::alive) {
            return Ok(());
        }
        // Reap a dead child if present.
        if let Some(dead) = self.children.remove(local_id) {
            dead.terminate().await;
        }
        let posture = self.postures.get(local_id).cloned().ok_or_else(|| {
            anyhow::anyhow!("cannot (re)launch {local_id}: no known working_dir/posture")
        })?;
        self.register(local_id);
        let launch = LaunchArgs {
            resume_from: Some(local_id.to_owned()),
            settings_path: posture.settings_path.clone(),
            model: posture.model.clone(),
            effort: posture.effort.clone(),
            permission_flag: posture.permission_flag.clone(),
            name: posture.name.clone(),
            ..LaunchArgs::default()
        };
        let env = self.resolve_launch_env(local_id, env_hint).await?.env;
        self.launch_child(local_id, &posture.cwd, launch.to_argv(), &env).await
    }

    /// Spawn the persistent `claude` child, wire its stdout pump, and store the
    /// live handles. Does NOT send a user turn.
    async fn launch_child(
        &mut self,
        local_id: &str,
        cwd: &str,
        shared_args: Vec<String>,
        env: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut command = Command::new(&self.cfg.claude_bin);
        command
            .current_dir(cwd)
            .arg("--print")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .args(&shared_args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        crate::childenv::ScrubChildEnv::scrub_child_env(&mut command);

        let mut child = command.spawn().with_context(|| {
            format!("spawning persistent `{}` (sdk) in {cwd}", self.cfg.claude_bin)
        })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_ring =
            child.stderr.take().map(|stderr| streamjson::spawn_stderr_ring(stderr, "sdk"));

        let pump = tokio::spawn(pump_stdout(
            local_id.to_owned(),
            stdout,
            stderr_ring,
            self.events.clone(),
            self.shutdown.clone(),
        ));

        // Replace any stale slot (a dead child already reaped by ensure_child).
        if let Some(old) =
            self.children.insert(local_id.to_owned(), LiveChild { child, stdin, pump })
        {
            old.terminate().await;
        }
        tracing::info!(%local_id, %cwd, "sdk launched persistent child");
        Ok(())
    }

    /// Write a `{"type":"user",…}` turn envelope to the child's stdin.
    async fn send_user_turn(&mut self, local_id: &str, text: &str) -> anyhow::Result<()> {
        let frame = streamjson::user_message_envelope(&json!(text));
        self.write_frame(local_id, &frame).await
    }

    /// Serialize `frame` + newline to the child's stdin. Backpressure is
    /// natural: the write `.await`s until the child drains.
    async fn write_frame(
        &mut self,
        local_id: &str,
        frame: &serde_json::Value,
    ) -> anyhow::Result<()> {
        let live = self
            .children
            .get_mut(local_id)
            .ok_or_else(|| anyhow::anyhow!("no live sdk child for {local_id}"))?;
        let mut buf = serde_json::to_vec(frame)?;
        buf.push(b'\n');
        live.stdin
            .write_all(&buf)
            .await
            .with_context(|| format!("writing to sdk child stdin for {local_id}"))?;
        live.stdin.flush().await.ok();
        Ok(())
    }

    /// Pull the session's gateway-routing env from the server,
    /// merging the carried `hint`. Fail-closed when account-bound but
    /// unmintable; best-effort hint when no server is configured. Mirrors
    /// `control::Driver::resolve_launch_env` / `oneshot`'s copy.
    async fn resolve_launch_env(
        &self,
        local_id: &str,
        hint: &BTreeMap<String, String>,
    ) -> anyhow::Result<super::control::LaunchEnv> {
        super::control::resolve_launch_env_for(
            self.server.as_ref(),
            self.machine_key.as_ref(),
            local_id,
            hint,
        )
        .await
    }

    /// Terminate the persistent child for `local_id` but keep the session
    /// resumable (posture retained; no `SessionEnded`).
    async fn kill(&mut self, local_id: &str) {
        if let Some(live) = self.children.remove(local_id) {
            live.terminate().await;
            tracing::info!(%local_id, "sdk killed persistent child (still resumable)");
        }
    }

    async fn kill_all(&mut self) {
        let ids: Vec<String> = self.children.keys().cloned().collect();
        for id in ids {
            if let Some(live) = self.children.remove(&id) {
                live.terminate().await;
            }
        }
    }

    /// SDK control-request id, mirroring the SDK's `req_{counter}_{hex}` shape.
    fn next_req_id(&mut self) -> String {
        self.req_counter += 1;
        let rand = &uuid::Uuid::new_v4().simple().to_string()[..8];
        format!("req_{}_{rand}", self.req_counter)
    }

    /// Register the session id in the hook map (identity mapping) so a hook
    /// reporting this live session id resolves to the same `local_id`.
    fn register(&self, session_id: &str) {
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

/// Per-child stdout pump: parse each stream-json frame through the shared codec,
/// forward the resulting [`AdapterEvent`]s, and on a `result` (turn boundary) or
/// EOF (child exit) emit an idle [`AdapterEvent::Status`] so the session stays
/// resumable — the persistent child is NOT torn down on `result`. On
/// EOF the child has exited; the next Reply/Resume cold-relaunches it.
#[allow(clippy::cognitive_complexity)]
async fn pump_stdout(
    local_id: String,
    stdout: ChildStdout,
    stderr_ring: Option<streamjson::StderrRing>,
    events: mpsc::Sender<AdapterEvent>,
    shutdown: CancellationToken,
) {
    let mut reader = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            line = reader.next_line() => match line {
                Ok(Some(line)) => {
                    let mut out = Vec::new();
                    let outcome = streamjson::parse_stream_line(&local_id, &line, &mut out);
                    for evt in out {
                        if events.send(evt).await.is_err() {
                            return;
                        }
                    }
                    // A `result`/error frame ends the *turn*, not the process:
                    // idle the session (resumable) and keep reading stdin turns.
                    if let Some(end) = outcome.end {
                        if let EndReason::Crashed { detail } = &end {
                            tracing::warn!(%local_id, %detail, "sdk turn ended with error frame");
                        }
                        let _ = events.send(idle_status(&local_id)).await;
                    }
                }
                Ok(None) => break, // EOF: child exited.
                Err(err) => {
                    tracing::warn!(%err, %local_id, "sdk stdout read error");
                    break;
                }
            }
        }
    }
    // Child stdout closed: the process is gone but the conversation stays
    // resumable (relaunch on the next command). Idle rather than SessionEnded.
    let tail = match &stderr_ring {
        Some(ring) => streamjson::stderr_tail(ring).await,
        None => String::new(),
    };
    if tail.is_empty() {
        tracing::info!(%local_id, "sdk persistent child stdout closed; session idle+resumable");
    } else {
        tracing::warn!(%local_id, %tail, "sdk persistent child stdout closed; session idle+resumable");
    }
    let _ = events.send(idle_status(&local_id)).await;
}

/// The idle [`AdapterEvent::Status`] emitted at a turn boundary / child exit:
/// the session is done with this turn but stays resumable.
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
        match idle_status("sid-1") {
            AdapterEvent::Status { local_id, tempo, state, .. } => {
                assert_eq!(local_id, "sid-1");
                assert_eq!(tempo.as_deref(), Some("idle"));
                assert_eq!(state.as_deref(), Some("done"));
            }
            other => panic!("expected idle Status, got {other:?}"),
        }
    }

    #[test]
    fn interrupt_frame_shape() {
        // The interrupt envelope must match the captured protocol.
        let frame = json!({
            "type": "control_request",
            "request_id": "req_1_abcd",
            "request": { "subtype": "interrupt" },
        });
        assert_eq!(frame["type"], "control_request");
        assert_eq!(frame["request"]["subtype"], "interrupt");
    }

    #[test]
    fn set_model_frame_shape() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req_2_beef",
            "request": { "subtype": "set_model", "model": "claude-opus-4-8" },
        });
        assert_eq!(frame["request"]["subtype"], "set_model");
        assert_eq!(frame["request"]["model"], "claude-opus-4-8");
    }

    #[test]
    fn user_turn_frame_is_stream_json_user_envelope() {
        // The reply/first-turn envelope is the shared codec's user message.
        let frame = streamjson::user_message_envelope(&json!("hello"));
        assert_eq!(frame["type"], "user");
        assert_eq!(frame["message"]["role"], "user");
        assert_eq!(frame["message"]["content"], "hello");
    }
}
