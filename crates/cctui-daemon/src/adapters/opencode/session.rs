//! One `opencode serve` subprocess per cctui session.
//!
//! Serve-per-session (not one shared server) because the provider credential is
//! a per-session gateway token injected through the child's env, and opencode
//! resolves `{env:…}` once per server process — a shared server could only ever
//! hold one session's credential.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result};
use cctui_proto::adapter::{AdapterEvent, EndReason, SessionMeta};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::client::{
    CreateSession, OpenCodeClient, Part, PartInput, PromptModelRef, PromptRequest, SessionModelRef,
};
use super::config::{ModelRef, SessionHome, session_config};
use super::events::{OcEvent, SseDecoder, StatusKind, status_kind};
use super::normalize::{self, Kind};

/// A just-started server accepts the connection before its handlers are wired
/// and never answers that first request; probes are bounded so the startup poll
/// keeps retrying instead of blocking on it.
const HEALTH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// A live turn always emits part/step updates well inside this window, so
/// silence this long means the upstream stream is dead rather than slow.
const STREAM_INACTIVITY: std::time::Duration = std::time::Duration::from_mins(2);

pub type LiveRegistry = Arc<Mutex<HashMap<String, LiveSession>>>;

/// `meta` is retained so a reconnect can replay the original `SessionStarted`.
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub commands: mpsc::Sender<SessionCommand>,
    pub meta: SessionMeta,
}

#[derive(Debug)]
pub enum SessionCommand {
    Prompt { session_id: String, text: String, command_id: Option<Uuid> },
    Kill { session_id: String },
    Fork { parent: String, prompt: Option<String>, name: Option<String>, command_id: Option<Uuid> },
    Permission { session_id: String, request_id: String, allow: bool },
}

impl SessionCommand {
    #[must_use]
    pub const fn command_id(&self) -> Option<Uuid> {
        match self {
            Self::Prompt { command_id, .. } | Self::Fork { command_id, .. } => *command_id,
            Self::Kill { .. } | Self::Permission { .. } => None,
        }
    }
}

/// Adapter-level knobs from `adapters_enabled.config`.
#[derive(Debug, Clone)]
pub struct OpenCodeConfig {
    pub bin: String,
    pub hostname: String,
    pub state_root: std::path::PathBuf,
    pub startup_timeout_ms: u64,
    pub default_agent: Option<String>,
    pub default_model: Option<String>,
}

impl Default for OpenCodeConfig {
    fn default() -> Self {
        Self {
            bin: "opencode".to_owned(),
            hostname: "127.0.0.1".to_owned(),
            state_root: std::path::PathBuf::from("/tmp/cctui-opencode"),
            startup_timeout_ms: 60_000,
            default_agent: None,
            default_model: None,
        }
    }
}

impl OpenCodeConfig {
    #[must_use]
    pub fn from_value(v: &serde_json::Value) -> Self {
        let d = Self::default();
        Self {
            bin: str_of(v, "bin").unwrap_or(d.bin),
            hostname: str_of(v, "hostname").unwrap_or(d.hostname),
            state_root: str_of(v, "state_root").map_or(d.state_root, std::path::PathBuf::from),
            startup_timeout_ms: v
                .get("startup_timeout_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(d.startup_timeout_ms),
            default_agent: str_of(v, "agent"),
            default_model: str_of(v, "model"),
        }
    }
}

fn str_of(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Probe the binary. The version is not enforced (opencode ships several
/// releases a week) but a mismatch against the pinned one is logged, and an
/// absent binary fails loudly instead of at the first spawn.
pub async fn probe_version(bin: &str) -> Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .env("PATH", crate::childenv::child_path())
        .output()
        .await
        .with_context(|| format!("`{bin} --version` failed to run — is opencode installed?"))?;
    if !out.status.success() {
        anyhow::bail!("`{bin} --version` exited {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

pub struct SpawnParams {
    pub cfg: OpenCodeConfig,
    pub key: String,
    pub cwd: String,
    pub env: std::collections::BTreeMap<String, String>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub attachments: Vec<String>,
    pub command_id: Option<Uuid>,
    /// Set for a `CctuiAgent` child so the session registers nested under the
    /// caller instead of as a top-level session.
    pub parent_local_id: Option<String>,
    /// `CctuiAgent` relay to declare in this session's config, when the server
    /// granted it spawn rights. `None` leaves the tool absent.
    pub agent_mcp: Option<crate::adapters::agent_mcp::AgentMcp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stall {
    Reattach,
    Crashed,
}

pub struct OpenCodeSession {
    params: SpawnParams,
    events: mpsc::Sender<AdapterEvent>,
    live: LiveRegistry,
    shutdown: CancellationToken,
    commands: mpsc::Receiver<SessionCommand>,
    commands_tx: mpsc::Sender<SessionCommand>,
    owned: HashSet<String>,
    roles: HashMap<String, String>,
    emitted_parts: HashSet<String>,
    emitted_usage: HashSet<String>,
    /// `CctuiAgent` child: `opencode serve` never exits on its own, so idle
    /// after assistant output — and any terminal error — must end the session.
    oneshot: bool,
    saw_assistant: bool,
    in_flight: bool,
}

impl OpenCodeSession {
    #[must_use]
    pub fn new(
        params: SpawnParams,
        events: mpsc::Sender<AdapterEvent>,
        live: LiveRegistry,
        shutdown: CancellationToken,
    ) -> Self {
        let (commands_tx, commands) = mpsc::channel(64);
        let oneshot = params.parent_local_id.is_some();
        Self {
            params,
            events,
            live,
            shutdown,
            commands,
            commands_tx,
            owned: HashSet::new(),
            roles: HashMap::new(),
            emitted_parts: HashSet::new(),
            emitted_usage: HashSet::new(),
            oneshot,
            saw_assistant: false,
            in_flight: false,
        }
    }

    pub async fn run(mut self) {
        let command_id = self.params.command_id.take();
        let failure = match self.run_inner(command_id).await {
            Ok(()) => None,
            Err(err) => {
                tracing::error!(%err, "opencode session ended in error");
                if let Some(command_id) = command_id {
                    let _ = self
                        .events
                        .send(AdapterEvent::CommandResult {
                            command_id,
                            ok: false,
                            error: Some(err.to_string()),
                        })
                        .await;
                }
                Some(err.to_string())
            }
        };
        if let Some(detail) = failure {
            self.crash_all(&detail).await;
            return;
        }
        for id in self.owned.clone() {
            self.live.lock().await.remove(&id);
            let _ = self
                .events
                .send(AdapterEvent::SessionEnded { local_id: id, reason: EndReason::Completed })
                .await;
        }
    }

    #[allow(clippy::too_many_lines, clippy::similar_names, clippy::cognitive_complexity)]
    async fn run_inner(&mut self, command_id: Option<Uuid>) -> Result<()> {
        let cwd = std::path::PathBuf::from(&self.params.cwd);
        anyhow::ensure!(cwd.is_dir(), "working_dir does not exist: {}", self.params.cwd);

        let version = probe_version(&self.params.cfg.bin).await?;
        if !version.contains(super::client::OPENCODE_PINNED_VERSION) {
            tracing::warn!(
                %version,
                pinned = super::client::OPENCODE_PINNED_VERSION,
                "opencode version differs from the pinned one"
            );
        }

        let model = self
            .params
            .model
            .as_deref()
            .or(self.params.cfg.default_model.as_deref())
            .and_then(ModelRef::parse);
        let home = SessionHome::under(&self.params.cfg.state_root, &self.params.key);
        home.write_config(&super::config::with_agent_mcp(
            session_config(model.as_ref(), &self.params.env),
            self.params.agent_mcp.as_ref(),
        ))?;

        let port = free_port(&self.params.cfg.hostname)?;
        let password = Uuid::new_v4().to_string();

        let mut cmd = Command::new(&self.params.cfg.bin);
        cmd.arg("serve")
            .arg("--hostname")
            .arg(&self.params.cfg.hostname)
            .arg("--port")
            .arg(port.to_string());
        for (k, v) in &self.params.env {
            cmd.env(k, v);
        }
        for (k, v) in home.env() {
            cmd.env(k, v);
        }
        cmd.env("OPENCODE_SERVER_PASSWORD", &password);
        crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
        // Own process group: `opencode serve` runs the model turn in children of
        // its own, and killing only the direct child leaves them generating (and
        // spending) — the whole group has to go.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .current_dir(&cwd)
            .env("PATH", crate::childenv::child_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn `{} serve`", self.params.cfg.bin))?;
        drain_child_logs(&mut child);

        let client = Arc::new(OpenCodeClient::new(
            format!("http://{}:{port}", self.params.cfg.hostname),
            password,
        ));
        self.await_health(&client).await?;

        let session = client
            .create_session(&CreateSession {
                title: self.params.name.clone(),
                agent: self.agent(),
                model: model.as_ref().map(|m| SessionModelRef {
                    id: m.model_id.clone(),
                    provider_id: m.provider_id.clone(),
                }),
                parent_id: None,
            })
            .await?;

        let parent = self.params.parent_local_id.clone();
        self.register(&session.id, parent, Some(self.params.cwd.clone())).await;
        if let Some(command_id) = command_id {
            let _ = self
                .events
                .send(AdapterEvent::CommandResult { command_id, ok: true, error: None })
                .await;
        }
        if let Some(m) = model.as_ref() {
            let _ = self
                .events
                .send(AdapterEvent::SessionModel {
                    local_id: session.id.clone(),
                    model: m.qualified(),
                })
                .await;
        }

        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut stream = tokio::spawn(pump_sse(client.clone(), evt_tx, self.shutdown.clone()));

        if let Some(text) = self.first_turn()
            && !self.prompt_or_crash(&client, &session.id, &text, model.as_ref(), None).await
        {
            stream.abort();
            shutdown_serve(&mut child).await;
            return Ok(());
        }

        let mut reattached = false;
        loop {
            if !self.in_flight {
                reattached = false;
            }
            tokio::select! {
                () = self.shutdown.cancelled() => break,
                status = child.wait() => {
                    tracing::info!(?status, "opencode serve exited");
                    if self.in_flight {
                        self.crash_all(&format!("`opencode serve` exited mid-turn ({status:?})"))
                            .await;
                    }
                    break;
                }
                evt = evt_rx.recv() => {
                    let Some(evt) = evt else { break };
                    if !self.on_event(&client, evt).await {
                        break;
                    }
                }
                cmd = self.commands.recv() => {
                    let Some(cmd) = cmd else { break };
                    if !self.on_command(&client, cmd, model.as_ref()).await {
                        break;
                    }
                }
                () = tokio::time::sleep(STREAM_INACTIVITY), if self.in_flight => {
                    if self.on_stall(&client, reattached).await == Stall::Crashed {
                        break;
                    }
                    reattached = true;
                    stream.abort();
                    let (tx, rx) = mpsc::channel(256);
                    evt_rx = rx;
                    stream = tokio::spawn(pump_sse(client.clone(), tx, self.shutdown.clone()));
                }
            }
        }

        stream.abort();
        self.fail_buffered_commands().await;
        self.abort_owned(&client).await;
        shutdown_serve(&mut child).await;
        Ok(())
    }

    async fn fail_buffered_commands(&mut self) {
        self.commands.close();
        while let Ok(cmd) = self.commands.try_recv() {
            if let Some(command_id) = cmd.command_id() {
                tracing::warn!(?cmd, "opencode: command dropped, session is ending");
                let _ = self
                    .events
                    .send(AdapterEvent::CommandResult {
                        command_id,
                        ok: false,
                        error: Some("opencode session ended before the command ran".to_owned()),
                    })
                    .await;
            }
        }
    }

    /// Abort every in-flight turn before the server goes away: a killed session
    /// that only loses its supervisor keeps generating, and on a metered account
    /// keeps spending.
    async fn abort_owned(&self, client: &OpenCodeClient) {
        for id in &self.owned {
            if let Err(err) = client.abort(id).await {
                tracing::warn!(%err, session = %id, "opencode abort failed");
            }
        }
    }

    fn agent(&self) -> Option<String> {
        self.params.agent.clone().or_else(|| self.params.cfg.default_agent.clone())
    }

    fn first_turn(&self) -> Option<String> {
        let prompt = self.params.prompt.clone().unwrap_or_default();
        if self.params.attachments.is_empty() {
            return (!prompt.trim().is_empty()).then_some(prompt);
        }
        let files = self.params.attachments.join("\n");
        Some(format!("{prompt}\n\nAttached files:\n{files}").trim().to_owned())
    }

    async fn await_health(&self, client: &OpenCodeClient) -> Result<()> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(self.params.cfg.startup_timeout_ms);
        let mut last: Option<String> = None;
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(HEALTH_PROBE_TIMEOUT, client.health()).await {
                Ok(Ok(h)) if h.healthy => return Ok(()),
                Ok(Ok(h)) => last = Some(format!("unhealthy (version {})", h.version)),
                Ok(Err(err)) => last = Some(err.to_string()),
                Err(_) => last = Some("health probe timed out".to_owned()),
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        anyhow::bail!(
            "`opencode serve` did not become healthy at {}: {}",
            client.base(),
            last.unwrap_or_else(|| "no response".to_owned())
        )
    }

    async fn register(
        &mut self,
        local_id: &str,
        parent_local_id: Option<String>,
        working_dir: Option<String>,
    ) {
        self.owned.insert(local_id.to_owned());
        // opencode mints `ses_…` only now, but the relay argv was baked with the
        // launch key, and the server resolves a parent by `sessions.id`.
        if let Some(agent_mcp) = &self.params.agent_mcp {
            crate::agenttool::bind_session_alias(agent_mcp.session_key(), local_id);
        }
        let meta = SessionMeta {
            working_dir,
            parent_local_id,
            extra: serde_json::json!({
                "harness": "opencode",
                "spawn_key": self.params.key,
            }),
        };
        self.live.lock().await.insert(
            local_id.to_owned(),
            LiveSession { commands: self.commands_tx.clone(), meta: meta.clone() },
        );
        let _ = self
            .events
            .send(AdapterEvent::SessionStarted { local_id: local_id.to_owned(), meta })
            .await;
    }

    /// Returns the failure text when the request never reached the model.
    async fn prompt(
        &mut self,
        client: &OpenCodeClient,
        session_id: &str,
        text: &str,
        model: Option<&ModelRef>,
    ) -> Option<String> {
        let body = PromptRequest {
            model: model.map(|m| PromptModelRef {
                provider_id: m.provider_id.clone(),
                model_id: m.model_id.clone(),
            }),
            agent: self.agent(),
            parts: vec![PartInput::Text { text: text.to_owned() }],
        };
        match client.prompt_async(session_id, &body).await {
            Ok(()) => {
                self.in_flight = true;
                None
            }
            Err(err) => {
                tracing::error!(%err, %session_id, "opencode prompt failed");
                let detail = format!("prompt rejected by opencode serve: {err}");
                self.emit_error(session_id, &detail).await;
                let _ = self
                    .events
                    .send(status(session_id, Some("error".to_owned()), Some(detail.clone()), None))
                    .await;
                Some(detail)
            }
        }
    }

    /// Deliver a prompt and, for a one-shot child, end the session as crashed
    /// when it was rejected outright. Returns `false` when the driver should stop.
    async fn prompt_or_crash(
        &mut self,
        client: &OpenCodeClient,
        session_id: &str,
        text: &str,
        model: Option<&ModelRef>,
        command_id: Option<Uuid>,
    ) -> bool {
        let failure = self.prompt(client, session_id, text, model).await;
        if let Some(command_id) = command_id {
            let _ = self
                .events
                .send(AdapterEvent::CommandResult {
                    command_id,
                    ok: failure.is_none(),
                    error: failure.clone(),
                })
                .await;
        }
        let Some(detail) = failure else {
            return true;
        };
        if !self.oneshot {
            return true;
        }
        self.end_crashed(session_id.to_owned(), detail).await
    }

    /// One bounded retry — reattaching to the bus recovers a dropped stream,
    /// while re-sending the prompt would double-bill a turn that may still run.
    async fn on_stall(&mut self, client: &OpenCodeClient, reattached: bool) -> Stall {
        if reattached {
            tracing::error!("opencode event stream still silent after reattaching — crashing");
            self.abort_owned(client).await;
            self.crash_all(&stalled_detail()).await;
            self.in_flight = false;
            return Stall::Crashed;
        }
        tracing::warn!("no opencode events under an in-flight turn — reattaching");
        Stall::Reattach
    }

    /// Report every still-owned session as crashed.
    async fn crash_all(&mut self, detail: &str) {
        for id in std::mem::take(&mut self.owned) {
            self.emit_error(&id, detail).await;
            self.live.lock().await.remove(&id);
            let _ = self
                .events
                .send(AdapterEvent::SessionEnded {
                    local_id: id,
                    reason: EndReason::Crashed { detail: detail.to_owned() },
                })
                .await;
        }
    }

    /// Returns `false` when the driver should stop.
    #[allow(clippy::cognitive_complexity)]
    async fn on_command(
        &mut self,
        client: &OpenCodeClient,
        cmd: SessionCommand,
        model: Option<&ModelRef>,
    ) -> bool {
        match cmd {
            SessionCommand::Prompt { session_id, text, command_id } => {
                return self.prompt_or_crash(client, &session_id, &text, model, command_id).await;
            }
            SessionCommand::Kill { session_id } => {
                if let Err(err) = client.abort(&session_id).await {
                    tracing::warn!(%err, %session_id, "opencode abort failed");
                }
                self.in_flight = false;
                self.owned.remove(&session_id);
                self.live.lock().await.remove(&session_id);
                let _ = self
                    .events
                    .send(AdapterEvent::SessionEnded {
                        local_id: session_id,
                        reason: EndReason::Killed,
                    })
                    .await;
                if self.owned.is_empty() {
                    return false;
                }
            }
            SessionCommand::Fork { parent, prompt, name, command_id } => {
                self.on_fork(client, &parent, prompt, name, command_id, model).await;
            }
            SessionCommand::Permission { session_id, request_id, allow } => {
                let response = if allow { "once" } else { "reject" };
                if let Err(err) =
                    client.respond_permission(&session_id, &request_id, response).await
                {
                    tracing::warn!(%err, %session_id, "opencode permission response failed");
                }
                let _ = self
                    .events
                    .send(AdapterEvent::PermissionResolved { local_id: session_id, request_id })
                    .await;
            }
        }
        true
    }

    async fn on_fork(
        &mut self,
        client: &OpenCodeClient,
        parent: &str,
        prompt: Option<String>,
        name: Option<String>,
        command_id: Option<Uuid>,
        model: Option<&ModelRef>,
    ) {
        match client.fork(parent).await {
            Ok(child) => {
                self.register(&child.id, Some(parent.to_owned()), Some(self.params.cwd.clone()))
                    .await;
                if let Some(name) = name {
                    let _ = self.events.send(status(&child.id, None, None, Some(name))).await;
                }
                if let Some(command_id) = command_id {
                    let _ = self
                        .events
                        .send(AdapterEvent::CommandResult { command_id, ok: true, error: None })
                        .await;
                }
                if let Some(text) = prompt.filter(|p| !p.trim().is_empty()) {
                    self.prompt(client, &child.id, &text, model).await;
                }
            }
            Err(err) => {
                tracing::error!(%err, %parent, "opencode fork failed");
                if let Some(command_id) = command_id {
                    let _ = self
                        .events
                        .send(AdapterEvent::CommandResult {
                            command_id,
                            ok: false,
                            error: Some(err.to_string()),
                        })
                        .await;
                }
            }
        }
    }

    /// Returns `false` when the driver should stop.
    async fn on_event(&mut self, client: &OpenCodeClient, evt: OcEvent) -> bool {
        let Some(session_id) = evt.session_id().map(str::to_owned) else { return true };
        if !self.owned.contains(&session_id) {
            return true;
        }
        match evt {
            OcEvent::MessageUpdated { properties } => {
                let info = properties.info;
                if info.role == "assistant" {
                    self.saw_assistant = true;
                }
                self.roles.insert(info.id.clone(), info.role.clone());
                if let Some(usage) = normalize::token_usage(&session_id, &info)
                    && self.emitted_usage.insert(info.id.clone())
                {
                    let _ = self.events.send(usage).await;
                }
                if let Some(error) = info.error.as_ref() {
                    let text = normalize::error_text(error);
                    self.in_flight = false;
                    self.emit_error(&session_id, &text).await;
                    if self.oneshot {
                        return self.end_crashed(session_id, text).await;
                    }
                }
            }
            OcEvent::PartUpdated { properties } => {
                self.emit_part(&session_id, &properties.part).await;
            }
            OcEvent::SessionIdle { .. } => {
                self.in_flight = false;
                if self.oneshot && self.saw_assistant {
                    return false;
                }
                let _ = self
                    .events
                    .send(status(&session_id, Some("idle".to_owned()), None, None))
                    .await;
            }
            OcEvent::SessionStatus { properties } => {
                if let Some(kind) = status_kind(&properties.status) {
                    let (tempo, detail) = match kind {
                        StatusKind::Busy => ("working".to_owned(), None),
                        StatusKind::Retry { attempt, message } => (
                            "working".to_owned(),
                            Some(format!("provider retry {attempt}: {message}")),
                        ),
                        StatusKind::Other(other) => (other, None),
                    };
                    let _ = self.events.send(status(&session_id, Some(tempo), detail, None)).await;
                }
            }
            OcEvent::SessionError { properties } => {
                let text = properties
                    .error
                    .as_ref()
                    .map_or_else(|| "session error".to_owned(), normalize::error_text);
                self.in_flight = false;
                self.emit_error(&session_id, &text).await;
                if self.oneshot {
                    return self.end_crashed(session_id, text).await;
                }
            }
            OcEvent::SessionDeleted { .. } => {
                self.owned.remove(&session_id);
                self.live.lock().await.remove(&session_id);
                let _ = self
                    .events
                    .send(AdapterEvent::SessionEnded {
                        local_id: session_id,
                        reason: EndReason::Completed,
                    })
                    .await;
            }
            OcEvent::PermissionAsked { properties } => {
                let _ = self
                    .events
                    .send(AdapterEvent::PermissionRequest {
                        local_id: session_id,
                        request_id: properties.id.clone(),
                        tool: properties.permission.clone(),
                        input: properties.metadata.clone(),
                    })
                    .await;
                let _ = client;
            }
            OcEvent::PermissionReplied { properties } => {
                let _ = self
                    .events
                    .send(AdapterEvent::PermissionResolved {
                        local_id: session_id,
                        request_id: properties.id,
                    })
                    .await;
            }
            OcEvent::Other => {}
        }
        true
    }

    async fn end_crashed(&mut self, session_id: String, detail: String) -> bool {
        self.owned.remove(&session_id);
        self.live.lock().await.remove(&session_id);
        let _ = self
            .events
            .send(AdapterEvent::SessionEnded {
                local_id: session_id,
                reason: EndReason::Crashed { detail },
            })
            .await;
        false
    }

    async fn emit_part(&mut self, session_id: &str, part: &Part) {
        let role = part
            .message_id()
            .and_then(|id| self.roles.get(id))
            .map_or("assistant", String::as_str)
            .to_owned();
        if !normalize::is_final(part, &role) || !self.emitted_parts.insert(part.id().to_owned()) {
            return;
        }
        for (kind, payload) in normalize::part_payloads(part, &role) {
            let event = match kind {
                Kind::Message => AdapterEvent::Message {
                    local_id: session_id.to_owned(),
                    payload,
                    turn_id: None,
                },
                Kind::ToolUse => AdapterEvent::ToolUse { local_id: session_id.to_owned(), payload },
            };
            if self.events.send(event).await.is_err() {
                return;
            }
        }
    }

    async fn emit_error(&self, session_id: &str, text: &str) {
        let _ = self
            .events
            .send(AdapterEvent::Message {
                local_id: session_id.to_owned(),
                payload: serde_json::json!({
                    "type": "text",
                    "content": format!("· {text}"),
                    "role": "assistant",
                    "text": format!("· {text}"),
                }),
                turn_id: None,
            })
            .await;
    }
}

fn stalled_detail() -> String {
    format!(
        "opencode event stream went silent for {}s under an in-flight turn, and reattaching to \
         it recovered nothing",
        STREAM_INACTIVITY.as_secs()
    )
}

fn status(
    local_id: &str,
    tempo: Option<String>,
    detail: Option<String>,
    name: Option<String>,
) -> AdapterEvent {
    AdapterEvent::Status {
        local_id: local_id.to_owned(),
        state: tempo.clone(),
        tempo,
        detail,
        activity: None,
        name,
        intent: None,
        model: None,
        effort: None,
        permission_mode: None,
        children: Vec::new(),
    }
}

#[allow(clippy::cognitive_complexity)]
async fn pump_sse(
    client: Arc<OpenCodeClient>,
    out: mpsc::Sender<OcEvent>,
    shutdown: CancellationToken,
) {
    use futures_util::StreamExt;

    loop {
        if shutdown.is_cancelled() {
            return;
        }
        match client.events().await {
            Ok(resp) => {
                let mut decoder = SseDecoder::new();
                let mut stream = resp.bytes_stream();
                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        chunk = stream.next() => {
                            let Some(chunk) = chunk else { break };
                            let Ok(bytes) = chunk else { break };
                            for data in decoder.push(&String::from_utf8_lossy(&bytes)) {
                                match serde_json::from_str::<OcEvent>(&data) {
                                    Ok(evt) => {
                                        if out.send(evt).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(err) => {
                                        tracing::debug!(%err, "undecodable opencode event");
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(%err, "opencode event stream unavailable"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// How long `opencode serve` gets to exit on SIGTERM before the group is
/// `SIGKILL`ed.
const SERVE_TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Terminate the serve process *tree*: signal the group (see `process_group`
/// at spawn), then escalate. Returns once the direct child is reaped.
async fn shutdown_serve(child: &mut tokio::process::Child) {
    let pgid = child.id().and_then(|p| i32::try_from(p).ok());
    signal_group(child, rustix::process::Signal::TERM);
    if tokio::time::timeout(SERVE_TERM_GRACE, child.wait()).await.is_err() {
        signal_group(child, rustix::process::Signal::KILL);
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let Some(pgid) = pgid else { return };
    for _ in 0..GROUP_REAP_POLLS {
        if !group_alive(pgid) {
            return;
        }
        signal_group_pid(pgid, rustix::process::Signal::KILL);
        tokio::time::sleep(GROUP_REAP_POLL_EVERY).await;
    }
    if group_alive(pgid) {
        tracing::error!(pgid, "opencode serve process group survived SIGKILL");
    }
}

const GROUP_REAP_POLLS: u32 = 50;
const GROUP_REAP_POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

/// A fully reaped group yields `ESRCH`.
fn group_alive(pgid: i32) -> bool {
    rustix::process::Pid::from_raw(pgid)
        .is_some_and(|p| rustix::process::test_kill_process_group(p).is_ok())
}

fn signal_group(child: &tokio::process::Child, signal: rustix::process::Signal) {
    let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) else { return };
    signal_group_pid(pid, signal);
}

/// The child leads its own group, so its pid is the pgid. A reaped process
/// just yields ESRCH.
fn signal_group_pid(pgid: i32, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pgid) {
        let _ = rustix::process::kill_process_group(pid, signal);
    }
}

fn drain_child_logs(child: &mut tokio::process::Child) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "opencode_serve", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "opencode_serve_stderr", "{line}");
            }
        });
    }
}

fn free_port(hostname: &str) -> Result<u16> {
    let listener = std::net::TcpListener::bind((hostname, 0))
        .with_context(|| format!("bind {hostname}:0 for the opencode server"))?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_overrides() {
        let d = OpenCodeConfig::from_value(&serde_json::json!({}));
        assert_eq!(d.bin, "opencode");
        assert_eq!(d.hostname, "127.0.0.1");
        assert_eq!(d.startup_timeout_ms, 60_000);
        assert!(d.default_agent.is_none());

        let c = OpenCodeConfig::from_value(&serde_json::json!({
            "bin": "/usr/local/bin/opencode",
            "hostname": "127.0.0.2",
            "state_root": "/run/oc",
            "startup_timeout_ms": 5000,
            "agent": "cctui-reviewer",
            "model": "fireworks-ai/accounts/fireworks/models/kimi-k3",
        }));
        assert_eq!(c.bin, "/usr/local/bin/opencode");
        assert_eq!(c.state_root, std::path::PathBuf::from("/run/oc"));
        assert_eq!(c.startup_timeout_ms, 5000);
        assert_eq!(c.default_agent.as_deref(), Some("cctui-reviewer"));
        assert_eq!(
            c.default_model.as_deref(),
            Some("fireworks-ai/accounts/fireworks/models/kimi-k3")
        );
    }

    #[test]
    fn blank_config_strings_fall_back_to_defaults() {
        let c = OpenCodeConfig::from_value(&serde_json::json!({ "bin": "  ", "agent": "" }));
        assert_eq!(c.bin, "opencode");
        assert!(c.default_agent.is_none());
    }

    #[test]
    fn free_port_is_bindable() {
        let port = free_port("127.0.0.1").unwrap();
        assert!(port > 0);
    }

    /// End-to-end against a real `opencode` binary: point `CCTUI_OPENCODE_BIN`
    /// at one and run with `--ignored`. Ignored by default — CI images have no
    /// opencode.
    #[tokio::test]
    #[ignore = "requires a real opencode binary (CCTUI_OPENCODE_BIN)"]
    async fn spawns_a_real_server_and_creates_a_session() {
        let Ok(bin) = std::env::var("CCTUI_OPENCODE_BIN") else { return };
        let repo = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let command_id = Uuid::new_v4();

        let params = SpawnParams {
            cfg: OpenCodeConfig {
                bin,
                state_root: state.path().to_path_buf(),
                startup_timeout_ms: 90_000,
                ..OpenCodeConfig::default()
            },
            key: command_id.to_string(),
            cwd: repo.path().display().to_string(),
            env: std::collections::BTreeMap::new(),
            prompt: None,
            name: Some("probe".to_owned()),
            model: Some("fireworks-ai/accounts/fireworks/models/kimi-k3".to_owned()),
            agent: Some(super::super::config::REVIEWER_AGENT.to_owned()),
            attachments: Vec::new(),
            command_id: Some(command_id),
            parent_local_id: None,
            agent_mcp: None,
        };
        let live = LiveRegistry::default();
        let handle =
            tokio::spawn(OpenCodeSession::new(params, tx, live.clone(), shutdown.clone()).run());

        let mut started = None;
        let mut acked = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_mins(2);
        while tokio::time::Instant::now() < deadline && !(acked && started.is_some()) {
            match tokio::time::timeout(std::time::Duration::from_mins(2), rx.recv()).await {
                Ok(Some(AdapterEvent::SessionStarted { local_id, .. })) => started = Some(local_id),
                Ok(Some(AdapterEvent::CommandResult { ok, error, .. })) => {
                    assert!(ok, "spawn failed: {error:?}");
                    acked = true;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        shutdown.cancel();
        let _ = handle.await;

        let local_id = started.expect("no SessionStarted");
        assert!(local_id.starts_with("ses"), "unexpected session id {local_id}");
        assert!(acked, "spawn was never acked");
    }

    #[cfg(unix)]
    fn is_alive(pid: i32) -> bool {
        rustix::process::Pid::from_raw(pid)
            .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_serve_takes_down_the_whole_process_tree() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 300 & echo $!; wait")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().unwrap();

        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let grandchild: i32 = lines.next_line().await.unwrap().unwrap().trim().parse().unwrap();
        assert!(is_alive(grandchild));

        shutdown_serve(&mut child).await;
        for _ in 0..50 {
            if !is_alive(grandchild) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("grandchild {grandchild} survived shutdown_serve");
    }

    fn test_session(
        parent_local_id: Option<String>,
    ) -> (OpenCodeSession, mpsc::Receiver<AdapterEvent>, Arc<OpenCodeClient>) {
        let (tx, rx) = mpsc::channel(64);
        let params = SpawnParams {
            cfg: OpenCodeConfig::default(),
            key: "key-1".to_owned(),
            cwd: ".".to_owned(),
            env: std::collections::BTreeMap::new(),
            prompt: Some("review the diff".to_owned()),
            name: None,
            model: None,
            agent: None,
            attachments: Vec::new(),
            command_id: None,
            parent_local_id,
            agent_mcp: None,
        };
        let mut session =
            OpenCodeSession::new(params, tx, LiveRegistry::default(), CancellationToken::new());
        session.owned.insert("ses_1".to_owned());
        let client =
            Arc::new(OpenCodeClient::new("http://127.0.0.1:1".to_owned(), "pw".to_owned()));
        (session, rx, client)
    }

    fn idle() -> OcEvent {
        OcEvent::SessionIdle {
            properties: super::super::events::SessionRef { session_id: "ses_1".to_owned() },
        }
    }

    fn assistant_message() -> OcEvent {
        OcEvent::MessageUpdated {
            properties: super::super::events::MessageUpdated {
                session_id: "ses_1".to_owned(),
                info: super::super::client::MessageInfo {
                    id: "msg_1".to_owned(),
                    role: "assistant".to_owned(),
                    ..Default::default()
                },
            },
        }
    }

    #[tokio::test]
    async fn oneshot_child_ends_on_idle_after_assistant_output() {
        let (mut session, _rx, client) = test_session(Some("parent-1".to_owned()));
        assert!(session.on_event(&client, idle()).await, "idle before output must not end");
        assert!(session.on_event(&client, assistant_message()).await);
        assert!(!session.on_event(&client, idle()).await, "idle after output must end");
    }

    #[tokio::test]
    async fn interactive_session_stays_up_across_idles() {
        let (mut session, mut rx, client) = test_session(None);
        assert!(session.on_event(&client, assistant_message()).await);
        assert!(session.on_event(&client, idle()).await);
        match rx.recv().await.unwrap() {
            AdapterEvent::Status { tempo, .. } => assert_eq!(tempo.as_deref(), Some("idle")),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oneshot_child_crashes_on_session_error() {
        let (mut session, mut rx, client) = test_session(Some("parent-1".to_owned()));
        let evt = OcEvent::SessionError {
            properties: super::super::events::SessionErrorProps {
                session_id: Some("ses_1".to_owned()),
                error: Some(serde_json::json!({
                    "name": "ProviderAuthError",
                    "data": { "message": "401 unauthorized" },
                })),
            },
        };
        assert!(!session.on_event(&client, evt).await);
        assert!(!session.owned.contains("ses_1"));
        let mut ended = None;
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::SessionEnded { local_id, reason } = evt {
                ended = Some((local_id, reason));
            }
        }
        let (local_id, reason) = ended.expect("no SessionEnded");
        assert_eq!(local_id, "ses_1");
        match reason {
            EndReason::Crashed { detail } => assert!(detail.contains("401")),
            other => panic!("expected Crashed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn interactive_session_survives_a_session_error() {
        let (mut session, _rx, client) = test_session(None);
        let evt = OcEvent::SessionError {
            properties: super::super::events::SessionErrorProps {
                session_id: Some("ses_1".to_owned()),
                error: None,
            },
        };
        assert!(session.on_event(&client, evt).await);
        assert!(session.owned.contains("ses_1"));
    }

    fn ended_events(rx: &mut mpsc::Receiver<AdapterEvent>) -> Vec<(String, EndReason)> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::SessionEnded { local_id, reason } = evt {
                out.push((local_id, reason));
            }
        }
        out
    }

    #[tokio::test]
    async fn kill_aborts_the_turn_and_ends_the_session_as_killed() {
        let (mut session, mut rx, client) = test_session(None);
        session.in_flight = true;
        let stop = session
            .on_command(&client, SessionCommand::Kill { session_id: "ses_1".to_owned() }, None)
            .await;
        assert!(!stop, "killing the last session must stop the driver so serve is reaped");
        assert!(!session.in_flight);
        assert!(session.owned.is_empty());
        let ended = ended_events(&mut rx);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0].0, "ses_1");
        assert_eq!(ended[0].1, EndReason::Killed);
    }

    #[tokio::test]
    async fn killing_one_of_several_sessions_keeps_the_driver_running() {
        let (mut session, mut rx, client) = test_session(None);
        session.owned.insert("ses_2".to_owned());
        let stop = session
            .on_command(&client, SessionCommand::Kill { session_id: "ses_1".to_owned() }, None)
            .await;
        assert!(stop, "other sessions still live on this serve");
        assert!(!session.owned.contains("ses_1"));
        assert!(session.owned.contains("ses_2"));
        assert_eq!(ended_events(&mut rx), vec![("ses_1".to_owned(), EndReason::Killed)]);
    }

    #[tokio::test]
    async fn a_rejected_prompt_crashes_a_oneshot_child() {
        let (mut session, mut rx, client) = test_session(Some("parent-1".to_owned()));
        let stop = session
            .on_command(
                &client,
                SessionCommand::Prompt {
                    session_id: "ses_1".to_owned(),
                    text: "go".to_owned(),
                    command_id: None,
                },
                None,
            )
            .await;
        assert!(!stop, "an unreachable serve must end the child, not hang it");
        assert!(!session.in_flight);
        let ended = ended_events(&mut rx);
        assert_eq!(ended.len(), 1, "{ended:?}");
        match &ended[0].1 {
            EndReason::Crashed { detail } => {
                assert!(detail.contains("prompt rejected"), "{detail}");
            }
            other => panic!("expected Crashed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rejected_prompt_reports_an_error_without_killing_an_interactive_session() {
        let (mut session, mut rx, client) = test_session(None);
        let stop = session
            .on_command(
                &client,
                SessionCommand::Prompt {
                    session_id: "ses_1".to_owned(),
                    text: "go".to_owned(),
                    command_id: None,
                },
                None,
            )
            .await;
        assert!(stop);
        assert!(session.owned.contains("ses_1"));
        assert!(ended_events(&mut rx).is_empty(), "an interactive session must survive");
    }

    #[tokio::test]
    async fn a_command_still_buffered_when_the_session_ends_is_acked_as_failed() {
        let (mut session, mut rx, _client) = test_session(None);
        let command_id = Uuid::new_v4();
        session
            .commands_tx
            .send(SessionCommand::Prompt {
                session_id: "ses_1".to_owned(),
                text: "go".to_owned(),
                command_id: Some(command_id),
            })
            .await
            .unwrap();
        session
            .commands_tx
            .send(SessionCommand::Kill { session_id: "ses_1".to_owned() })
            .await
            .unwrap();
        session.fail_buffered_commands().await;
        let mut acks = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::CommandResult { command_id: cid, ok, .. } = evt {
                acks.push((cid, ok));
            }
        }
        assert_eq!(acks, vec![(command_id, false)], "only correlated commands are answered");
    }

    #[test]
    fn command_id_is_read_from_every_correlated_variant() {
        let cid = Uuid::new_v4();
        assert_eq!(
            SessionCommand::Prompt {
                session_id: String::new(),
                text: String::new(),
                command_id: Some(cid),
            }
            .command_id(),
            Some(cid)
        );
        assert_eq!(
            SessionCommand::Fork {
                parent: String::new(),
                prompt: None,
                name: None,
                command_id: Some(cid),
            }
            .command_id(),
            Some(cid)
        );
        assert_eq!(SessionCommand::Kill { session_id: String::new() }.command_id(), None);
    }

    #[tokio::test]
    async fn a_rejected_prompt_acks_its_command_id_as_failed() {
        let (mut session, mut rx, client) = test_session(None);
        let command_id = Uuid::new_v4();
        session
            .on_command(
                &client,
                SessionCommand::Prompt {
                    session_id: "ses_1".to_owned(),
                    text: "go".to_owned(),
                    command_id: Some(command_id),
                },
                None,
            )
            .await;
        let mut ack = None;
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::CommandResult { command_id: cid, ok, error } = evt {
                assert_eq!(cid, command_id);
                ack = Some((ok, error));
            }
        }
        let (ok, error) = ack.expect("a send must report an outcome, not stay unconfirmed");
        assert!(!ok);
        assert!(error.is_some_and(|e| e.contains("rejected")), "expected the rejection detail");
    }

    #[tokio::test]
    async fn a_rejected_prompt_surfaces_an_error_message_and_status() {
        let (mut session, mut rx, client) = test_session(None);
        assert!(session.prompt(&client, "ses_1", "go", None).await.is_some());
        let mut saw_message = false;
        let mut saw_status = false;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AdapterEvent::Message { payload, .. } => {
                    saw_message |=
                        payload["text"].as_str().unwrap_or_default().contains("rejected");
                }
                AdapterEvent::Status { tempo, .. } => {
                    saw_status |= tempo.as_deref() == Some("error");
                }
                _ => {}
            }
        }
        assert!(saw_message, "the failure must reach the transcript");
        assert!(saw_status, "the failure must flip the session status to error");
    }

    #[tokio::test]
    async fn a_stalled_stream_reattaches_once_then_crashes_every_session() {
        let (mut session, mut rx, client) = test_session(None);
        session.owned.insert("ses_2".to_owned());
        session.in_flight = true;

        assert_eq!(session.on_stall(&client, false).await, Stall::Reattach);
        assert!(ended_events(&mut rx).is_empty(), "the first stall only reattaches");

        assert_eq!(session.on_stall(&client, true).await, Stall::Crashed);
        assert!(!session.in_flight);
        assert!(session.owned.is_empty());
        let mut ended = ended_events(&mut rx);
        ended.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(ended.len(), 2, "{ended:?}");
        for (local_id, reason) in ended {
            match reason {
                EndReason::Crashed { detail } => {
                    assert!(detail.contains("went silent"), "{local_id}: {detail}");
                }
                other => panic!("{local_id}: expected Crashed, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_turn_is_only_in_flight_between_the_prompt_and_idle() {
        let (mut session, _rx, client) = test_session(None);
        assert!(!session.in_flight);
        session.in_flight = true;
        assert!(session.on_event(&client, assistant_message()).await);
        assert!(session.in_flight, "assistant output does not end the turn");
        assert!(session.on_event(&client, idle()).await);
        assert!(!session.in_flight, "idle ends the turn — the watchdog must stand down");
    }

    #[tokio::test]
    async fn a_driver_that_fails_after_registering_reports_crashed_not_completed() {
        let (tx, mut rx) = mpsc::channel(64);
        let mut params = SpawnParams {
            cfg: OpenCodeConfig::default(),
            key: "key-1".to_owned(),
            cwd: "/definitely/not/a/directory".to_owned(),
            env: std::collections::BTreeMap::new(),
            prompt: None,
            name: None,
            model: None,
            agent: None,
            attachments: Vec::new(),
            command_id: None,
            parent_local_id: Some("parent-1".to_owned()),
            agent_mcp: None,
        };
        params.cfg.bin = "/definitely/not/a/binary".to_owned();
        let mut session =
            OpenCodeSession::new(params, tx, LiveRegistry::default(), CancellationToken::new());
        session.owned.insert("ses_1".to_owned());
        session.run().await;
        let ended = ended_events(&mut rx);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert!(matches!(ended[0].1, EndReason::Crashed { .. }), "{:?}", ended[0].1);
    }

    #[test]
    fn the_stall_detail_names_the_inactivity_window() {
        let detail = stalled_detail();
        assert!(detail.contains(&STREAM_INACTIVITY.as_secs().to_string()), "{detail}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_serve_leaves_no_live_process_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 300 & sleep 300 & echo $$; wait")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pgid = i32::try_from(child.id().unwrap()).unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let _ = lines.next_line().await.unwrap();
        assert!(group_alive(pgid));

        shutdown_serve(&mut child).await;
        assert!(!group_alive(pgid), "process group {pgid} outlived shutdown_serve");
    }

    #[test]
    fn status_mirrors_tempo_into_state() {
        match status("ses_1", Some("idle".to_owned()), None, None) {
            AdapterEvent::Status { local_id, tempo, state, name, .. } => {
                assert_eq!(local_id, "ses_1");
                assert_eq!(tempo.as_deref(), Some("idle"));
                assert_eq!(state.as_deref(), Some("idle"));
                assert!(name.is_none());
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }
}
