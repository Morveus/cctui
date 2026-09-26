//! Adapter contract surface.
//!
//! Wire types shared between the daemon and the server (and inspected by
//! clients via the WS frames in [`crate::ws`]). The runtime `Adapter` trait
//! itself lives in `cctui-daemon` so that this crate stays free of async
//! runtime dependencies; consumers that only need to read or transport
//! `AdapterEvent`/`AdapterCommand` values do not pay for tokio.

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// Stable identifier for an adapter implementation (e.g. `"claude-code"`,
/// `"codex"`).
///
/// Adapters compiled into the daemon return their id from `Adapter::id()`;
/// the server uses this string in `sessions.adapter_id` and
/// `adapters_enabled.adapter_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(transparent)]
pub struct AdapterId(pub String);

/// Every adapter compiled into the daemon.
///
/// The server's Reconcile runs all of them on every machine; `adapters_enabled`
/// rows only override (per-machine config, explicit disable). A missing binary
/// surfaces as a failed spawn, never as a silently absent adapter.
pub const KNOWN_ADAPTERS: &[&str] = &["claude-code", "codex", "opencode"];

impl AdapterId {
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AdapterId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for AdapterId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for AdapterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Adapter-specific session metadata.
///
/// Payload shape is left to the adapter (e.g. `claude-code` may include
/// `claude_session_id`, `working_dir`, project dir; `codex` may include the
/// log path). The server stores this as JSONB alongside the normalised
/// session row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_local_id: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

impl Default for SessionMeta {
    fn default() -> Self {
        Self { working_dir: None, parent_local_id: None, extra: serde_json::Value::Null }
    }
}

/// 8-hex worker shortcode used by the `claude daemon` control socket.
///
/// Matches `^[0-9a-f]{8}$`. Surfaced opaquely as the adapter's `local_id`
/// for claude-code sessions; this newtype is offered for callers that
/// want validation at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobShort(String);

impl JobShort {
    /// Construct from an arbitrary string after lowercase + length + hex-class
    /// validation. Returns `None` if the input is not exactly 8 ASCII
    /// hex-digits.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            Some(Self(s.to_string()))
        } else {
            None
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a session ended. Free-form `Other(String)` reserved for adapter-specific
/// reasons we do not want to enumerate centrally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EndReason {
    Completed,
    Killed,
    Crashed {
        detail: String,
    },
    /// The adapter could not revive an existing session (`thread/resume`).
    ResumeFailed {
        detail: String,
    },
    /// A fresh spawn never reached a running session.
    SpawnFailed {
        detail: String,
    },
    Other {
        detail: String,
    },
}

impl EndReason {
    #[must_use]
    pub const fn kind(&self) -> crate::models::SessionEndReason {
        match self {
            Self::Completed => crate::models::SessionEndReason::Completed,
            Self::Killed => crate::models::SessionEndReason::Killed,
            Self::Crashed { .. } => crate::models::SessionEndReason::Crashed,
            Self::ResumeFailed { .. } => crate::models::SessionEndReason::ResumeFailed,
            Self::SpawnFailed { .. } => crate::models::SessionEndReason::SpawnFailed,
            Self::Other { .. } => crate::models::SessionEndReason::Other,
        }
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Completed | Self::Killed => None,
            Self::Crashed { detail }
            | Self::ResumeFailed { detail }
            | Self::SpawnFailed { detail }
            | Self::Other { detail } => Some(detail),
        }
    }
}

/// Events emitted by an adapter and forwarded by the daemon to the server.
///
/// `local_id` is the adapter's own session identifier (claude session id,
/// codex log basename, …). The server resolves it to a stable
/// `server_session_id` via the unique `(machine_id, adapter_id, local_id)`
/// index on `sessions`.
///
/// Payloads are deliberately opaque `serde_json::Value` so the wire stays
/// stable while each adapter keeps its native shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdapterEvent {
    SessionStarted {
        local_id: String,
        meta: SessionMeta,
    },
    Message {
        local_id: String,
        payload: serde_json::Value,
        /// Identity of the human turn this event derives from, minted by the
        /// client and carried through [`AdapterCommand::Reply`]. `None` for
        /// turns cctui did not originate (typed into the agent's own TUI) and
        /// for daemons predating the field — those still need content matching.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<Uuid>,
    },
    ToolUse {
        local_id: String,
        payload: serde_json::Value,
    },
    SessionEnded {
        local_id: String,
        reason: EndReason,
    },
    /// A snapshot of the session's runtime status. Mirrors the
    /// `LiveSnapshot` shape from the `claude daemon` `list` op plus the
    /// identity fields read from `~/.claude/jobs/<short>/state.json`.
    /// Adapters that don't have this signal can omit it. All fields except
    /// `local_id` are optional so partial updates round-trip cleanly.
    Status {
        local_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tempo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activity: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intent: Option<String>,
        /// Model the session runs on (e.g. `"opus[1m]"`, `"claude-opus-4-8"`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Reasoning/effort level (e.g. `"low"`, `"high"`), when set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        /// Permission posture the session is actually running under, as
        /// observed (claude reports it in the transcript, not in `state.json`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<PermissionMode>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<SessionChild>,
    },
    /// Linked-PR children from a transcript `pr-link` line: a fallback source
    /// of [`SessionChild`]. The server fills the session row only when it has
    /// no children, so an authoritative `Status` snapshot always wins.
    PrLink {
        local_id: String,
        children: Vec<SessionChild>,
    },
    /// Per-assistant-message token usage extracted from the transcript's
    /// `message.usage` block. Idempotent on the server side via
    /// `UNIQUE (session_id, message_id)`. Cache fields are `0` when the
    /// underlying adapter doesn't report prefix-cache stats.
    TokenUsage {
        local_id: String,
        message_id: String,
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cache_read_tokens: u64,
        #[serde(default)]
        cache_creation_tokens: u64,
    },
    /// The model id observed on an assistant message in the transcript (e.g.
    /// `"claude-opus-4-8"`). This is the ground truth of what actually ran;
    /// it fills `sessions.model` for sessions launched without an explicit
    /// `--model` flag (where the flag-derived model is absent). The server
    /// writes it only when the model is still unset, so an explicit
    /// `--model` alias keeps priority.
    SessionModel {
        local_id: String,
        model: String,
    },
    /// The agent is blocked awaiting a tool-permission decision. The
    /// `request_id` is what the client must echo back via
    /// `AdapterCommand::PermissionResponse`. Carrier is adapter-specific
    /// (see the v1 spec for the claude-code investigation).
    PermissionRequest {
        local_id: String,
        request_id: String,
        tool: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        input: serde_json::Value,
    },
    /// A previously-emitted [`AdapterEvent::PermissionRequest`] is no longer
    /// pending — the underlying agent's tool-permission prompt was answered
    /// (from any surface) or dismissed. Clients drop the inline prompt. The
    /// claude-code adapter emits this when the control-socket record's
    /// `tempo:"blocked"`/`needs` signal clears, so a prompt answered in the
    /// native TUI (or timed out) doesn't leave a stale card in the webui.
    PermissionResolved {
        local_id: String,
        request_id: String,
    },
    /// The agent is awaiting a free-form answer to an `AskUserQuestion` tool
    /// call. Unlike [`AdapterEvent::PermissionRequest`], the structured option
    /// set is NOT available from the transcript live: claude-code flushes the
    /// `tool_use` block only once the turn advances (i.e. after the question is
    /// answered), and the control socket reports `state:"done"` while it's
    /// pending — so it would otherwise appear only retroactively. The daemon's
    /// `AskUserQuestion` `PreToolUse` hook delivers the question text the
    /// instant the form renders. Answered via a normal
    /// [`AdapterCommand::Reply`].
    ///
    /// `questions` carries the raw `tool_input.questions` array (header,
    /// question, options, multiSelect) the hook also has, so clients can render
    /// the interactive option-card form live instead of only the flattened
    /// `question` text. `None` for deliveries.
    AskQuestion {
        local_id: String,
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        questions: Option<serde_json::Value>,
        /// The assistant prose that preceded the `AskUserQuestion` tool call in
        /// the same turn (the research summary / recommendation the question
        /// depends on). Read from the transcript by the `ask-hook` subcommand,
        /// which already gets `transcript_path` on stdin, so the live question
        /// card can show its context instead of being answered blind.
        /// `None` when the model called the tool with no preceding text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preamble: Option<String>,
    },
    /// A previously-emitted [`AdapterEvent::AskQuestion`] is no longer pending
    /// (the `AskUserQuestion` `PostToolUse` hook fired). Clients dismiss the
    /// live prompt.
    AskResolved {
        local_id: String,
    },
    /// The agent is in plan mode and has presented a plan via the
    /// `ExitPlanMode` tool, rendering a single-select PTY approval prompt
    /// (1 = accept + bypass, 2 = accept + manual approval, 3 = keep planning,
    /// 4 = tell Claude what to change). Like [`AdapterEvent::AskQuestion`],
    /// this is invisible to the control socket (it reports `state:"done"`
    /// while pending) and the `tool_use` block only flushes to the transcript
    /// after the turn advances — so the daemon's `ExitPlanMode` `PreToolUse`
    /// hook delivers the plan the instant the prompt renders.
    ///
    /// Answered via a normal [`AdapterCommand::Reply`]: a digit pick (1-3)
    /// drives the form natively (mirrors `AskUserQuestion`), and option 4
    /// ("Tell Claude what to change") is free-text answered via the existing
    /// dismiss-then-reply path.
    PlanRequest {
        local_id: String,
        /// The plan markdown (`tool_input.plan`).
        plan: String,
        /// Assistant prose preceding the `ExitPlanMode` call in the same turn,
        /// read from the transcript by the `ask-hook` subcommand. `None` when
        /// the model called the tool with no preceding text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preamble: Option<String>,
    },
    /// A previously-emitted [`AdapterEvent::PlanRequest`] is no longer pending
    /// (the `ExitPlanMode` `PostToolUse` hook fired, or the prompt was
    /// answered / dismissed). Clients drop the live Plan card.
    PlanResolved {
        local_id: String,
    },
    /// Outcome of a server-initiated command (currently `Spawn`). Not tied to
    /// a session — `command_id` correlates it with the HTTP spawn response so
    /// the server can rebroadcast it to the originating client.
    CommandResult {
        command_id: Uuid,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Reply to an [`AdapterCommand::Diagnose`]: everything the
    /// adapter knows about the session, dated. `request_id` correlates with
    /// the round-trip the server parked for the originating
    /// `GET /sessions/{id}/diagnose`. Boxed: the report is by far the largest
    /// payload on this enum.
    Diagnose {
        local_id: String,
        request_id: Uuid,
        report: Box<crate::diagnose::SessionDiagnose>,
    },
    /// Machine/account-scoped codex model catalog from `model/list`.
    /// Keyed by `machine_id`, not a session — the server keeps the latest per
    /// machine so the picker offers the account's real models, static fallback.
    CodexModels {
        catalog: crate::codex_catalog::CodexModelCatalog,
    },
    /// A coalesced slice of a watched session's live PTY byte stream.
    /// Emitted only while a viewer is attached (server-gated via
    /// [`AdapterCommand::WatchPty`]); `data` is the standard-base64 of the raw
    /// terminal bytes. Never persisted — the server fans it straight out to the
    /// browsers watching the session's terminal, nothing is stored.
    PtyChunk {
        local_id: String,
        data: String,
    },
    /// The daemon's transcript byte-offset high-water mark for a session. The
    /// server keeps `max(offset)` per session and returns it as a resume mark on
    /// the daemon's next connect, so the tail cursor clamps forward rather than
    /// replaying the transcript from zero.
    TranscriptMark {
        local_id: String,
        offset: u64,
    },
    /// Account rate-limit windows the agent itself reported (codex sends them
    /// with every token count), recorded as usage samples of the credential
    /// the session is bound to.
    RateLimits {
        local_id: String,
        windows: Vec<RateLimitWindow>,
        /// Unix seconds the agent observed them at; `None` means just now.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_at: Option<i64>,
    },
}

/// One rate-limit window as reported by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    /// Window length; `None` when the agent did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// `primary` or `secondary`, the agent's own naming.
    pub slot: String,
}

/// Child reference attached to a session — typically a linked PR. Drives the
/// TUI's "Ready for review" classifier bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionChild {
    pub id: String,
    pub href: String,
    pub kind: String,
}

/// Commands sent from the daemon to an adapter (and ultimately to the
/// underlying agent process). v0 defines the shapes; the write path is
/// implemented incrementally per adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdapterCommand {
    /// Per-session transcript resume marks the server pushes on connect.
    /// `marks` maps `local_id` → the server's stored transcript byte
    /// offset; the adapter clamps each known session's tail cursor forward and
    /// heals any divergence with one bounded re-send.
    ResumeMarks {
        marks: Vec<(String, u64)>,
    },
    SendMessage {
        local_id: String,
        text: String,
    },
    Kill {
        local_id: String,
        /// POSIX signal number. `None` means default (SIGTERM for the
        /// `claude daemon` `kill` op).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
    Spawn {
        spec: SessionSpec,
        /// Correlation id minted by the server's spawn route. Echoed back in
        /// an [`AdapterEvent::CommandResult`] so the originating client can be
        /// told whether the spawn actually succeeded. `None` for
        /// commands not initiated via the HTTP spawn route.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<Uuid>,
        /// Session id pre-minted by the server, mirroring
        /// [`AdapterCommand::Fork`]'s `session_id`. When `Some`, the claude
        /// adapter launches the worker with this as `--session-id` instead of
        /// minting its own, so the id the server bound the gateway session
        /// token to (`session_tokens.session_id`) matches the id the worker
        /// registers as — without it `account_name` never resolves. Adapters
        /// that mint their own id (codex) ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<Uuid>,
    },
    /// Fork an existing conversation into a brand-new session, optionally
    /// changing the model/effort at fork time. Each adapter maps it
    /// to its native primitive: claude `--resume <parent-session-id>
    /// --fork-session [--model …] [--effort …]` (minting a fresh short /
    /// session id, forking from the parent's on-disk `resumeSessionId` when
    /// present); codex app-server `thread/fork { threadId, … }`.
    /// The new session links back to the parent via
    /// [`SessionMeta::parent_local_id`] on its `SessionStarted`, so the fork is
    /// discoverable; the parent is left intact (NOT archived/flipped). The
    /// supported substitute for claude's missing in-place model switch
    /// routes here. `spec.working_dir`/`model`/`effort`/`name` carry
    /// the (optionally overridden) launch parameters; `spec.prompt` is an
    /// optional first turn on the forked branch.
    Fork {
        /// The parent session's `local_id` — for claude this is the parent's
        /// session id (== its DB row id, from which the short is derived); for
        /// codex it is the parent thread id.
        parent_local_id: String,
        spec: SessionSpec,
        /// Correlation id minted by the server's fork route, echoed back in an
        /// [`AdapterEvent::CommandResult`]. `None` for non-HTTP
        /// callers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<Uuid>,
        /// Child session id pre-minted by the server so the fork route can
        /// return it and the webui can navigate to the new conversation
        /// immediately. When `Some`, the claude adapter uses it as the
        /// new `--session-id` instead of minting its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Conversation-extract selector. `None` → fork the parent's
        /// full history via the native primitive. `Some` → fork only a
        /// subset: the claude adapter materializes a sliced copy of the parent's
        /// on-disk transcript as the child's own `<child>.jsonl` and resumes
        /// that standalone file (no `--fork-session`). Only supported by the
        /// claude adapter; codex has no partial-fork primitive, so the server
        /// rejects subset forks for codex before they reach the daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extract: Option<ForkExtract>,
    },
    /// Claude-code-specific: inject `text` directly into the worker PTY
    /// via the `reply` op. Distinct from `SendMessage` (which v0 routed
    /// through MCP notifications) so that adapters with both paths can
    /// disambiguate.
    Reply {
        local_id: String,
        text: String,
        /// Structured ask answer: per-question 0-based option picks
        /// for a pending `AskUserQuestion`. When present and a form is up, the
        /// adapter answers natively (PTY keystrokes on the real form) so claude
        /// records a genuine `tool_result`; `text` remains the fallback carrier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask_picks: Option<Vec<Vec<usize>>>,
        /// Gateway env re-minted by the server for the session's bound OAuth
        /// account, carried on every reply so that if the daemon has to
        /// cold-resume a hibernated worker to deliver it, the revived worker
        /// gets a fresh valid `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_BASE_URL` (or
        /// `OpenAI` pair) instead of launching with empty env and 401ing.
        /// Ignored when the worker is already alive; empty for
        /// sessions with no account binding.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
        /// Correlation id minted by the server's send-message routes, echoed
        /// back in an [`AdapterEvent::CommandResult`] so the originating client
        /// learns whether the adapter actually delivered the reply.
        /// `None` for non-client callers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<Uuid>,
        /// Client-minted identity of this human turn (`UUIDv7`). The adapter
        /// stamps it onto every [`AdapterEvent::Message`] the injected turn
        /// produces, so the several encodings Claude stores one turn in share
        /// one key. `None` for callers that mint none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<Uuid>,
    },
    /// Interrupt the in-flight turn WITHOUT tearing the session down — the
    /// keep-alive equivalent of pressing Esc in the TUI. Distinct
    /// from `Kill`: the worker, session, and transcript stay live and
    /// resumable. The claude-code adapter has no control-socket turn-interrupt
    /// op, so it attaches to the worker PTY and injects a bare ESC keystroke;
    /// the codex adapter sends `turn/interrupt` without terminating the
    /// app-server.
    Interrupt {
        local_id: String,
        /// Correlation id minted by the server's interrupt route, echoed back
        /// in an [`AdapterEvent::CommandResult`] so the originating client can
        /// see whether the agent actually accepted the interrupt.
        /// `None` for non-HTTP callers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<Uuid>,
    },
    /// Revive an exited-but-resumable conversation without sending a reply.
    /// Claude-code maps this to the same `dispatch --resume <sessionId>`
    /// primitive used by resume-on-reply; adapters that do not support durable
    /// transcripts may reject it.
    ///
    /// `working_dir` lets the daemon resume even when the on-disk job
    /// `state.json` is gone (archiving runs `claude rm`, which deletes it while
    /// preserving the conversation transcript) — the daemon then falls back to
    /// `local_id` as the conversation id and this cwd.
    Resume {
        local_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// Gateway env (`ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN` or the
        /// `OpenAI` pair) re-minted by the server for the session's bound OAuth
        /// account, re-injected into the revived worker so it keeps routing
        /// through the gateway after a hibernation/restart instead of hitting
        /// the default upstream with no credential and 401ing. Empty
        /// for sessions with no account binding.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
    },
    /// Answer a previously-emitted `AdapterEvent::PermissionRequest`.
    PermissionResponse {
        local_id: String,
        request_id: String,
        allow: bool,
    },
    /// Rename a session after creation. The adapter is expected to persist
    /// the name to its source of truth (claude-code: `state.json.name`) so it
    /// survives the next status poll, rather than being clobbered by the
    /// on-disk value. Propagates from `PATCH /api/v1/sessions/{id}`.
    Rename {
        local_id: String,
        name: String,
    },
    /// Remove a session entirely — the equivalent of Claude Code's agent-view
    /// Ctrl+X (`claude rm <id>`): stop the worker if it is still live, then
    /// delete its on-disk job metadata (and any Claude-created worktree) so it
    /// disappears from Claude Code's native `claude agents` view as well as
    /// cctui's discovery. The conversation transcript is preserved and stays
    /// resumable. There is no control-socket op for this, so the
    /// claude-code adapter shells out to `claude rm`; adapters without an
    /// external agent view (codex) treat this as a plain kill. Propagates from
    /// the archive route.
    Remove {
        local_id: String,
        /// Correlation id minted by the archive route, echoed back in an
        /// [`AdapterEvent::CommandResult`] so the caller learns whether the
        /// job was actually removed. `None` for the reconcile path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<Uuid>,
        /// Who asked. A claude job cctui did not start is removed only on
        /// [`RemoveInitiator::User`]; an automatic sweep leaves it alone.
        #[serde(default)]
        initiator: RemoveInitiator,
    },
    /// Change the model and/or reasoning effort of an already-running session
    /// **in place**, without spawning a new conversation. Applies to
    /// subsequent turns on the same thread. Agent-asymmetric: the codex adapter
    /// carries the override on the next `turn/start` (a stable per-turn override
    /// codex promotes to the later default) and echoes the resolved
    /// values via [`AdapterEvent::Status`]; the claude-code adapter has no
    /// non-interactive set-model lever, so it errors "fork to change model"
    /// (fork-with-`--model`). At least one of `model`/`effort` is
    /// expected. `command_id` correlates the outcome as an
    /// [`AdapterEvent::CommandResult`] the webui awaits before confirming.
    SetModel {
        local_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<Uuid>,
    },
    /// Snapshot everything the adapter knows about a session. The
    /// adapter answers with an [`AdapterEvent::Diagnose`] echoing
    /// `request_id`, which the server correlates back to the waiting
    /// `GET /sessions/{id}/diagnose`. Read-only: never touches the worker.
    Diagnose {
        local_id: String,
        request_id: Uuid,
    },
    /// Start (`watch: true`) or stop (`watch: false`) relaying the session's
    /// live PTY byte stream to the server as [`AdapterEvent::PtyChunk`].
    /// The server sends `watch: true` when the first browser opens
    /// the read-only terminal view and `watch: false` when the last one closes,
    /// so the daemon only opens the extra viewer attach while someone is
    /// actually watching. Read-only: the adapter opens a fresh attach purely to
    /// forward bytes (a fresh attach makes the worker repaint the current
    /// screen), never injecting keystrokes. Adapters without a PTY (codex)
    /// ignore it.
    WatchPty {
        local_id: String,
        watch: bool,
    },
}

impl AdapterCommand {
    /// The session this command targets. `None` for the adapter-wide commands
    /// (`Spawn`, `ResumeMarks`), which no single session owns.
    #[must_use]
    pub fn local_id(&self) -> Option<&str> {
        match self {
            Self::SendMessage { local_id, .. }
            | Self::Kill { local_id, .. }
            | Self::Reply { local_id, .. }
            | Self::Interrupt { local_id, .. }
            | Self::Resume { local_id, .. }
            | Self::PermissionResponse { local_id, .. }
            | Self::Rename { local_id, .. }
            | Self::Remove { local_id, .. }
            | Self::SetModel { local_id, .. }
            | Self::Diagnose { local_id, .. }
            | Self::WatchPty { local_id, .. } => Some(local_id),
            Self::Fork { parent_local_id, .. } => Some(parent_local_id),
            Self::Spawn { spec, .. } => spec.parent_local_id.as_deref(),
            Self::ResumeMarks { .. } => None,
        }
    }

    #[must_use]
    pub const fn command_id(&self) -> Option<Uuid> {
        match self {
            Self::Spawn { command_id, .. }
            | Self::Fork { command_id, .. }
            | Self::Reply { command_id, .. }
            | Self::Interrupt { command_id, .. }
            | Self::Remove { command_id, .. }
            | Self::SetModel { command_id, .. } => *command_id,
            _ => None,
        }
    }
}

/// Who asked for an [`AdapterCommand::Remove`].
///
/// Defaults to [`Self::Automatic`] so a peer that predates the field, or any
/// path that forgets to say, gets the conservative reading.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RemoveInitiator {
    User,
    /// A TTL sweep, a spawn-intent auto-archive, or the reconcile purge.
    #[default]
    Automatic,
}

impl RemoveInitiator {
    /// The persisted `sessions.archived_by` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Automatic => "automatic",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "automatic" => Some(Self::Automatic),
            _ => None,
        }
    }
}

/// Which slice of a parent conversation a subset fork keeps.
///
/// All modes anchor on an assistant `message_id` (`msg_…`) — the only
/// per-message identity present in both the webui line and the on-disk
/// transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ForkMode {
    /// Everything up to and including the anchor message; drop what follows.
    UpTo,
    /// Everything after the anchor message; drop the anchor and all before it.
    After,
    /// Only the turns containing the selected messages.
    Selected,
}

/// Conversation-extract selector for a subset fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ForkExtract {
    pub mode: ForkMode,
    /// Anchor assistant `message_id` for `up_to`/`after`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_message_id: Option<String>,
    /// Selected assistant `message_id`s for `selected`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_message_ids: Vec<String>,
}

/// Per-spawn permission posture (supersedes the
/// `full_access: bool`). Adapters map it to their own vocabulary; `None`
/// on a [`SessionSpec`] defers to the daemon's per-host default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Skip every prompt and the sandbox. claude `--permission-mode
    /// bypassPermissions`; codex `sandbox_mode=danger-full-access` +
    /// `approval_policy=never`.
    Yolo,
    /// Auto-apply edits/commands but keep the workspace sandbox — no
    /// prompts. claude `--permission-mode acceptEdits`; codex
    /// `sandbox_mode=workspace-write` + `approval_policy=never`.
    Auto,
    /// Prompt on every action. claude `--permission-mode default`; codex
    /// `sandbox_mode=workspace-write` + `approval_policy=on-request`.
    Ask,
    /// "Whip" (🐎) — yolo on steroids. Same permission posture as
    /// [`Self::Yolo`] (no prompts, no sandbox), plus two enforcement hooks
    /// injected into the claude worker: `AskUserQuestion` is **banned** (a
    /// `PreToolUse` deny), and a `Stop` hook blocks stalling / hand-back
    /// language so the worker keeps going until the work is genuinely done or
    /// it is genuinely blocked. Codex (no hook surface) maps it like yolo.
    Whip,
}

impl PermissionMode {
    /// The claude `--permission-mode` value for this posture.
    #[must_use]
    pub const fn claude_flag(self) -> &'static str {
        match self {
            Self::Yolo | Self::Whip => "bypassPermissions",
            Self::Auto => "acceptEdits",
            Self::Ask => "default",
        }
    }

    /// The codex `(sandbox_mode, approval_policy)` pair for this posture.
    #[must_use]
    pub const fn codex_sandbox_approval(self) -> (&'static str, &'static str) {
        match self {
            Self::Yolo | Self::Whip => ("danger-full-access", "never"),
            Self::Auto => ("workspace-write", "never"),
            Self::Ask => ("workspace-write", "on-request"),
        }
    }

    /// Whip (🐎) mode: ban `AskUserQuestion` and install the no-stall `Stop`
    /// hook on top of the yolo posture.
    #[must_use]
    pub const fn is_whip(self) -> bool {
        matches!(self, Self::Whip)
    }

    /// Coarse `default` / `auto` / `yolo` label surfaced to the agent in the
    /// spawn-time `<session-context>` block. `Ask` (prompt on every
    /// action, claude `default`) reads as `default`; `Auto` (`acceptEdits`) as
    /// `auto`; `Yolo`/`Whip` (`bypassPermissions`) as `yolo`. The whip-specific
    /// enforcement hooks aren't part of this coarse posture label.
    #[must_use]
    pub const fn normalized_label(self) -> &'static str {
        match self {
            Self::Ask => "default",
            Self::Auto => "auto",
            Self::Yolo | Self::Whip => "yolo",
        }
    }
}

/// Parameters for spawning a brand-new session. Used by `AdapterCommand::Spawn`
/// (post-v0; the spawn route currently returns 501).
///
/// `Debug` is implemented by hand to redact `env` (secret values) and
/// `bootstrap` (base64 file bytes) so neither leaks into logs if a
/// `DaemonFrameDown` carrying this spec is ever debug-printed.
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub adapter_id: AdapterId,
    pub working_dir: Option<String>,
    pub prompt: Option<String>,
    /// Optional display name to launch the session with (claude `--name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Per-spawn permission posture. `None` defers to the
    /// daemon's per-host default. Adapters map it to their own vocabulary
    /// (codex `sandbox_mode` + `approval_policy`, claude `--permission-mode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Reasoning/effort level (claude `--effort`, codex
    /// `model_reasoning_effort`). `None` defers to the adapter default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Model family to launch under. claude `--model`, codex
    /// `-c model="…"`. `None` defers to the adapter default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Codex service tier: `"default"` (standard) or `"fast"` (1.5x speed,
    /// increased usage — same model, same quality). Resolved server-side, so a
    /// codex spawn always carries a concrete value rather than inheriting
    /// codex's own `priority` default. `None` on every other adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Environment secrets injected into the worker process env at spawn time.
    /// Merged on top of the pre-forked spare's baseline env and
    /// mirrored into `reattachEnv` so they survive a respawn/reattach. NEVER
    /// persisted (DB / `state.json` / `seed`) and NEVER logged.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Opaque bootstrap payload carried to the daemon at spawn. When
    /// present it deserializes into [`BootstrapUploads`]: small files the
    /// daemon stages under `/tmp/cctui-uploads/<session-id>/` and references in
    /// the prompt so the worker can read them.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub bootstrap: serde_json::Value,
    /// Parent session, echoed by the adapter into the child's
    /// [`SessionMeta::parent_local_id`]. `None` for a top-level spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_local_id: Option<String>,
}

impl std::fmt::Debug for SessionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSpec")
            .field("adapter_id", &self.adapter_id)
            .field("working_dir", &self.working_dir)
            .field("prompt", &self.prompt)
            .field("name", &self.name)
            .field("permission_mode", &self.permission_mode)
            .field("effort", &self.effort)
            .field("model", &self.model)
            .field("service_tier", &self.service_tier)
            // Redacted: secret values / file bytes must never reach a log.
            .field("env", &format_args!("<{} secret(s) redacted>", self.env.len()))
            .field("bootstrap", &format_args!("<redacted>"))
            .field("parent_local_id", &self.parent_local_id)
            .finish()
    }
}

/// Bootstrap payload shape for file uploads. Carried in
/// [`SessionSpec::bootstrap`] as JSON; the server base64-encodes the bytes and
/// the daemon decodes + stages them before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapUploads {
    pub uploads: Vec<BootstrapFile>,
}

/// One uploaded file. `name` is a bare filename (no path separators — the
/// server strips/rejects traversal); `content_b64` is the standard-base64 file
/// content. `Debug` redacts the bytes.
#[derive(Clone, Serialize, Deserialize)]
pub struct BootstrapFile {
    pub name: String,
    pub content_b64: String,
}

impl std::fmt::Debug for BootstrapFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapFile")
            .field("name", &self.name)
            .field("content_b64", &format_args!("<{} b64 bytes>", self.content_b64.len()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_mode_maps_to_an_approval_policy_codex_accepts() {
        for mode in
            [PermissionMode::Yolo, PermissionMode::Whip, PermissionMode::Auto, PermissionMode::Ask]
        {
            let (_, approval) = mode.codex_sandbox_approval();
            assert!(matches!(approval, "on-request" | "never"), "{mode:?} → {approval}");
        }
    }

    #[test]
    fn remove_carries_its_correlation_id() {
        let id = Uuid::new_v4();
        let cmd = AdapterCommand::Remove {
            local_id: "sess-1".into(),
            command_id: Some(id),
            initiator: RemoveInitiator::User,
        };
        assert_eq!(cmd.command_id(), Some(id));
        assert_eq!(cmd.local_id(), Some("sess-1"));
        let back: AdapterCommand =
            serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(back.command_id(), Some(id));
        assert!(matches!(back, AdapterCommand::Remove { initiator: RemoveInitiator::User, .. }));
    }

    #[test]
    fn remove_without_an_initiator_deserializes_as_automatic() {
        let cmd: AdapterCommand =
            serde_json::from_str(r#"{"kind":"remove","local_id":"sess-1"}"#).unwrap();
        assert!(matches!(
            cmd,
            AdapterCommand::Remove { initiator: RemoveInitiator::Automatic, .. }
        ));
    }

    fn spec(parent: Option<&str>) -> SessionSpec {
        SessionSpec {
            adapter_id: AdapterId::new("codex"),
            working_dir: Some("/workspace".into()),
            prompt: Some("review".into()),
            name: None,
            permission_mode: None,
            effort: None,
            model: None,
            service_tier: None,
            env: std::collections::BTreeMap::new(),
            bootstrap: serde_json::Value::Null,
            parent_local_id: parent.map(str::to_owned),
        }
    }

    #[test]
    fn a_child_spawn_belongs_to_its_parent_session() {
        let cmd = AdapterCommand::Spawn {
            spec: spec(Some("parent-1")),
            command_id: None,
            session_id: Some(Uuid::new_v4()),
        };
        assert_eq!(cmd.local_id(), Some("parent-1"));
    }

    #[test]
    fn a_top_level_spawn_has_no_session() {
        let cmd = AdapterCommand::Spawn { spec: spec(None), command_id: None, session_id: None };
        assert_eq!(cmd.local_id(), None);
    }

    #[test]
    fn remove_from_older_peer_has_no_correlation_id() {
        let json = r#"{"kind":"remove","local_id":"sess-1"}"#;
        let cmd: AdapterCommand = serde_json::from_str(json).unwrap();
        assert_eq!(cmd.command_id(), None);
    }

    #[test]
    fn adapter_id_roundtrips() {
        let id = AdapterId::new("claude-code");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""claude-code""#);
        let back: AdapterId = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), "claude-code");
    }

    #[test]
    fn adapter_event_codex_models_roundtrips() {
        let evt = AdapterEvent::CodexModels {
            catalog: crate::codex_catalog::CodexModelCatalog {
                models: vec![crate::codex_catalog::CodexModel {
                    id: "gpt-5.6-sol".into(),
                    model: "gpt-5.6-sol".into(),
                    display_name: "GPT-5.6 Sol".into(),
                    description: String::new(),
                    hidden: false,
                    is_default: true,
                    supported_efforts: vec!["low".into(), "high".into()],
                    default_effort: "medium".into(),
                    input_modalities: vec!["text".into()],
                    upgrade: None,
                    minimal_client_version: None,
                }],
                client_version: None,
            },
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""kind":"codex_models""#));
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        let AdapterEvent::CodexModels { catalog } = back else { panic!("wrong variant") };
        assert_eq!(catalog.models[0].id, "gpt-5.6-sol");
        assert_eq!(catalog.models[0].supported_efforts, ["low", "high"]);
    }

    #[test]
    fn adapter_event_session_started_roundtrips() {
        let evt = AdapterEvent::SessionStarted {
            local_id: "abc".into(),
            meta: SessionMeta { working_dir: Some("/tmp".into()), ..SessionMeta::default() },
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""kind":"session_started""#));
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AdapterEvent::SessionStarted { .. }));
    }

    #[test]
    fn adapter_event_all_variants_roundtrip() {
        let cases = vec![
            AdapterEvent::SessionStarted { local_id: "s1".into(), meta: SessionMeta::default() },
            AdapterEvent::Message {
                local_id: "s1".into(),
                payload: serde_json::json!({"role": "assistant", "text": "hi"}),
                turn_id: None,
            },
            AdapterEvent::ToolUse {
                local_id: "s1".into(),
                payload: serde_json::json!({"tool": "Bash", "input": {}}),
            },
            AdapterEvent::SessionEnded { local_id: "s1".into(), reason: EndReason::Completed },
        ];
        for evt in cases {
            let json = serde_json::to_string(&evt).unwrap();
            let _back: AdapterEvent = serde_json::from_str(&json).expect(&json);
        }
    }

    #[test]
    fn message_turn_id_roundtrips_and_is_omitted_when_absent() {
        let bare = AdapterEvent::Message {
            local_id: "s1".into(),
            payload: serde_json::json!({"role": "user", "text": "hi"}),
            turn_id: None,
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert!(!json.contains("turn_id"), "{json}");
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AdapterEvent::Message { turn_id: None, .. }));

        let id = Uuid::new_v4();
        let stamped = AdapterEvent::Message {
            local_id: "s1".into(),
            payload: serde_json::json!({"role": "user", "text": "hi"}),
            turn_id: Some(id),
        };
        let json = serde_json::to_string(&stamped).unwrap();
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        let AdapterEvent::Message { turn_id, .. } = back else { panic!("wrong variant") };
        assert_eq!(turn_id, Some(id));
    }

    #[test]
    fn message_from_daemon_predating_the_field_decodes() {
        let legacy = r#"{"kind":"message","local_id":"s1","payload":{"role":"user"}}"#;
        let back: AdapterEvent = serde_json::from_str(legacy).unwrap();
        assert!(matches!(back, AdapterEvent::Message { turn_id: None, .. }));
    }

    #[test]
    fn reply_command_carries_turn_id() {
        let id = Uuid::new_v4();
        let cmd = AdapterCommand::Reply {
            local_id: "s1".into(),
            text: "go on".into(),
            ask_picks: None,
            env: std::collections::BTreeMap::default(),
            command_id: None,
            turn_id: Some(id),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: AdapterCommand = serde_json::from_str(&json).unwrap();
        let AdapterCommand::Reply { turn_id, .. } = back else { panic!("wrong variant") };
        assert_eq!(turn_id, Some(id));

        let legacy = r#"{"kind":"reply","local_id":"s1","text":"go on"}"#;
        let back: AdapterCommand = serde_json::from_str(legacy).unwrap();
        assert!(matches!(back, AdapterCommand::Reply { turn_id: None, .. }));
    }

    #[test]
    fn adapter_command_roundtrips() {
        let cmd = AdapterCommand::SendMessage { local_id: "s1".into(), text: "hello".into() };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""kind":"send_message""#));
        let back: AdapterCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AdapterCommand::SendMessage { .. }));
    }

    #[test]
    fn end_reason_crashed_roundtrips() {
        let r = EndReason::Crashed { detail: "oom".into() };
        let json = serde_json::to_string(&r).unwrap();
        let back: EndReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn job_short_parses_and_rejects() {
        assert!(JobShort::parse("6e189420").is_some());
        assert!(JobShort::parse("6E189420").is_none(), "must be lowercase");
        assert!(JobShort::parse("6e18942").is_none(), "must be 8 chars");
        assert!(JobShort::parse("6e189420x").is_none(), "non-hex rejected");
        let s = JobShort::parse("6e189420").unwrap();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""6e189420""#);
        let back: JobShort = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn adapter_event_status_roundtrips() {
        let evt = AdapterEvent::Status {
            local_id: "s1".into(),
            tempo: Some("active".into()),
            state: Some("working".into()),
            detail: Some("running tests".into()),
            activity: None,
            name: Some("DEFI-1317".into()),
            intent: None,
            model: Some("opus[1m]".into()),
            effort: Some("low".into()),
            permission_mode: None,
            children: vec![SessionChild {
                id: "1972".into(),
                href: "https://github.com/o/r/pull/1972".into(),
                kind: "pr".into(),
            }],
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""kind":"status""#));
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AdapterEvent::Status { .. }));
    }

    #[test]
    fn adapter_event_status_minimal_roundtrips() {
        let evt = AdapterEvent::Status {
            local_id: "s1".into(),
            tempo: None,
            state: None,
            detail: None,
            activity: None,
            name: None,
            intent: None,
            model: None,
            effort: None,
            permission_mode: None,
            children: vec![],
        };
        let json = serde_json::to_string(&evt).unwrap();
        // Optional fields with `skip_serializing_if` drop out cleanly.
        assert!(!json.contains("tempo"));
        assert!(!json.contains("children"));
        let _back: AdapterEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn adapter_event_permission_request_roundtrips() {
        let evt = AdapterEvent::PermissionRequest {
            local_id: "s1".into(),
            request_id: "req-123".into(),
            tool: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""kind":"permission_request""#));
        let _back: AdapterEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn adapter_command_reply_kill_perm_roundtrip() {
        let cases = vec![
            AdapterCommand::Reply {
                local_id: "s1".into(),
                text: "go on".into(),
                ask_picks: None,
                env: std::collections::BTreeMap::default(),
                command_id: None,
                turn_id: None,
            },
            AdapterCommand::Kill { local_id: "s1".into(), signal: Some(15) },
            AdapterCommand::Kill { local_id: "s1".into(), signal: None },
            AdapterCommand::PermissionResponse {
                local_id: "s1".into(),
                request_id: "req-123".into(),
                allow: true,
            },
        ];
        for cmd in cases {
            let json = serde_json::to_string(&cmd).unwrap();
            let _back: AdapterCommand = serde_json::from_str(&json).expect(&json);
        }
    }

    #[test]
    fn adapter_command_kill_signal_omitted_by_default() {
        let cmd = AdapterCommand::Kill { local_id: "s1".into(), signal: None };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains("signal"), "signal:None must not serialize");
        // And deserialising without the field still works (back-compat).
        let back: AdapterCommand =
            serde_json::from_str(r#"{"kind":"kill","local_id":"s1"}"#).unwrap();
        assert!(matches!(back, AdapterCommand::Kill { signal: None, .. }));
    }

    #[test]
    fn adapter_diagnose_command_and_event_roundtrip() {
        // the diagnose round-trip rides the generic Command/Event
        // path, so its serde shape must stay wire-stable.
        let req_id = Uuid::new_v4();
        let cmd = AdapterCommand::Diagnose { local_id: "s1".into(), request_id: req_id };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""kind":"diagnose""#));
        let back: AdapterCommand = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, AdapterCommand::Diagnose { request_id, .. } if request_id == req_id)
        );

        let report = crate::diagnose::SessionDiagnose {
            local_id: "s1".into(),
            short: None,
            generated_at_ms: 42,
            adapter: "claude-code".into(),
            effective_state: crate::diagnose::DiagnoseFact::missing("activity", "no status"),
            last_hook_event: crate::diagnose::DiagnoseFact::missing("hook", "none"),
            attach: crate::diagnose::DiagnoseFact::missing("attach", "none"),
            pty_output: crate::diagnose::DiagnoseFact::missing("pty", "CCT-546"),
            claude_socket: crate::diagnose::DiagnoseFact::missing("discovery", "none"),
            transcript: crate::diagnose::DiagnoseFact::missing("filesystem", "none"),
            prompts: crate::diagnose::DiagnoseFact::missing("hook", "none"),
            permission_mode: crate::diagnose::DiagnoseFact::missing("spawn", "none"),
            dispatch: crate::diagnose::DiagnoseFact::missing("dispatch", "none"),
            gateway: crate::diagnose::DiagnoseFact::missing("daemon-config", "none"),
            codex: None,
        };
        let evt = AdapterEvent::Diagnose {
            local_id: "s1".into(),
            request_id: req_id,
            report: Box::new(report.clone()),
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""kind":"diagnose""#));
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        match back {
            AdapterEvent::Diagnose { request_id, report: r, .. } => {
                assert_eq!(request_id, req_id);
                assert_eq!(*r, report);
            }
            other => panic!("expected Diagnose, got {other:?}"),
        }
    }

    #[test]
    fn remove_initiator_persists_as_its_wire_name() {
        for i in [RemoveInitiator::User, RemoveInitiator::Automatic] {
            assert_eq!(serde_json::to_value(i).unwrap(), serde_json::json!(i.as_str()));
            assert_eq!(RemoveInitiator::parse(i.as_str()), Some(i));
        }
        assert_eq!(RemoveInitiator::parse("reaper"), None);
    }

    #[test]
    fn watch_pty_command_and_pty_chunk_event_roundtrip() {
        let cmd = AdapterCommand::WatchPty { local_id: "s1".into(), watch: true };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""kind":"watch_pty""#));
        assert!(json.contains(r#""watch":true"#));
        let back: AdapterCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AdapterCommand::WatchPty { watch: true, .. }));

        let evt = AdapterEvent::PtyChunk { local_id: "s1".into(), data: "aGk=".into() };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains(r#""kind":"pty_chunk""#));
        let back: AdapterEvent = serde_json::from_str(&json).unwrap();
        match back {
            AdapterEvent::PtyChunk { local_id, data } => {
                assert_eq!(local_id, "s1");
                assert_eq!(data, "aGk=");
            }
            other => panic!("expected PtyChunk, got {other:?}"),
        }
    }

    #[test]
    fn session_spec_minimal_roundtrips() {
        let spec = SessionSpec {
            adapter_id: AdapterId::new("claude-code"),
            working_dir: None,
            prompt: None,
            name: None,
            permission_mode: None,
            effort: None,
            model: None,
            service_tier: None,
            env: std::collections::BTreeMap::new(),
            bootstrap: serde_json::Value::Null,
            parent_local_id: None,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let _back: SessionSpec = serde_json::from_str(&json).unwrap();
    }
}
