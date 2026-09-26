use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::adapter::{AdapterCommand, AdapterEvent, BootstrapFile};
use crate::api::DaemonAdapterConfig;

// --- Daemon → Server ---

/// Successful [`DaemonFrameUp::ReadFileResult`] payload.
///
/// Exactly one of `data` (standard base64, files up to
/// [`READ_FILE_INLINE_BYTES`]) or `blob_hash` (sha256 hex of the bytes the
/// daemon PUT to the blob store) is set. `sha256` is always the content hash
/// (the server's `ETag`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileOk {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_hash: Option<String>,
}

/// Why a [`DaemonFrameDown::ReadFile`] was refused; the server maps these
/// onto HTTP statuses (403 / 413 / 404 / 500).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFileErrorKind {
    Denied,
    TooLarge,
    NotFound,
    Io,
}

/// Files up to this size ride inline in the `ReadFileResult`; larger ones go
/// through the blob store.
pub const READ_FILE_INLINE_BYTES: u64 = 1024 * 1024;

/// Hard cap on a `ReadFile` (the blob store's own limit).
pub const READ_FILE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Frames sent by a daemon to the server over `/api/v1/daemon/ws`.
///
/// `Event` is inherently the largest variant (it carries an [`AdapterEvent`]
/// with JSON payloads / many optional fields); boxing it would ripple
/// through every construct/match site for no real benefit on this
/// non-hot-path wire enum.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
#[allow(clippy::large_enum_variant)]
pub enum DaemonFrameUp {
    /// An adapter produced an event. The server maps `(machine_id, adapter_id,
    /// local_id)` to a stable `server_session_id`, persisting a new row on
    /// `SessionStarted` if one does not exist yet.
    Event { adapter_id: String, event: AdapterEvent },
    /// Optional explicit registration hint when the adapter cannot supply a
    /// full `SessionStarted` yet (e.g. resumed session). Mostly redundant.
    SessionRegistered { adapter_id: String, local_id: String },
    /// Liveness ping. `bandwidth` carries the daemon's per-subsystem
    /// byte counters so the server can persist per-machine bandwidth and detect
    /// an upload/insert divergence. Optional so older daemons still parse and the
    /// server tolerates its absence.
    Heartbeat {
        sent_at: chrono::DateTime<chrono::Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bandwidth: Option<crate::bandwidth::BandwidthSummary>,
        /// Whether this machine has a deterministic update hook configured
        /// (`CCTUI_UPDATE_COMMAND`), so the server can offer the hook instead
        /// of a YOLO agent. Optional: a daemon too old to know the field
        /// omits it, and the server then leaves the stored flag alone rather
        /// than reading the absence as "no hook".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update_hook: Option<bool>,
        /// Host CPU / memory / disk snapshot for the header resource gauge.
        /// Optional: a daemon too old (or on a platform without `/proc`)
        /// omits it and the server keeps the last stored snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resources: Option<crate::resources::MachineResources>,
        /// Shorts of the claude jobs present under the daemon's jobs root.
        /// The server answers with [`DaemonFrameDown::ArchivedJobs`] for the
        /// ones whose session it has archived. Optional: a daemon that omits
        /// it cannot parse that reply and must never receive one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        claude_jobs: Option<Vec<String>>,
        /// Harness versions and auto-update outcomes. Optional: the server
        /// sends [`DaemonFrameDown::HarnessUpdatePolicy`] only to a daemon that
        /// reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        harness: Option<crate::harness::HarnessReport>,
    },
    /// Reply to a [`DaemonFrameDown::StageFiles`] request (mid-chat
    /// attachments). `request_id` correlates with the originating
    /// `POST /api/v1/sessions/{id}/files` so the server can return the staged
    /// absolute paths (or the error) to the waiting HTTP client.
    StageFilesResult {
        request_id: uuid::Uuid,
        ok: bool,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Reply to a [`DaemonFrameDown::ListDirs`] request (working-directory
    /// autocomplete in the spawn dialog). `request_id` correlates with the
    /// originating `GET /api/v1/machines/{id}/fs/dirs` so the server can
    /// return the directory names (or the error) to the waiting HTTP client.
    ListDirsResult {
        request_id: uuid::Uuid,
        ok: bool,
        #[serde(default)]
        dirs: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Reply to a [`DaemonFrameDown::GitInfo`] request; `request_id`
    /// correlates with the originating `GET /api/v1/machines/{id}/fs/gitinfo`.
    GitInfoResult {
        request_id: uuid::Uuid,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        info: Option<crate::git::GitInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Reply to a [`DaemonFrameDown::ReadFile`] request; `request_id`
    /// correlates with the originating `GET /api/v1/machines/{id}/fs/file`.
    ReadFileResult {
        request_id: uuid::Uuid,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<ReadFileOk>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<ReadFileErrorKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// One chunk of a serialized up-frame split by [`crate::chunk`].
    /// `transfer_id` is the content hash of the full payload (idempotent
    /// retransmission); `data` is standard-base64 of the raw chunk bytes. The
    /// server reassembles by `transfer_id`, parses the joined payload as a
    /// `DaemonFrameUp`, and processes it as usual.
    ///
    /// `codec` tags the reassembled bytes: `Some("zstd")` means the
    /// server must [`crate::compress::decompress_codec`] the joined payload
    /// before parsing it. Omitted (`None`) for legacy daemons that
    /// chunk uncompressed JSON.
    Chunk {
        transfer_id: String,
        chunk_index: u32,
        total_chunks: u32,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codec: Option<String>,
    },
    /// A single up-frame whose serialized body was compressed but is
    /// small enough to skip chunking. `data` is standard-base64 of the codec
    /// output; the server [`crate::compress::decode_compressed`]s it back to a
    /// serialized inner `DaemonFrameUp` and processes that.
    Compressed { codec: String, data: String },
    /// Several up-frames coalesced over the daemon's micro-batch window
    /// so cross-event redundancy compresses far better than one frame
    /// at a time. The server processes `frames` in order, preserving per-event
    /// semantics. Rides inside a `Compressed`/`Chunk` envelope when large.
    Batch { frames: Vec<Self> },
}

/// Frames sent by the server to a daemon over `/api/v1/daemon/ws`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DaemonFrameDown {
    /// Initial declarative state — sent on connect and again whenever the
    /// server mutates `adapters_enabled` (or the owner's secret-scrub settings)
    /// for this machine. `secret_scrub` is defaulted so older daemons/tests that
    /// omit it keep parsing.
    Reconcile {
        adapters: Vec<DaemonAdapterConfig>,
        #[serde(default)]
        secret_scrub: SecretScrubConfig,
    },
    /// A command for a specific adapter (and ultimately a specific session).
    /// `command` is boxed so this large variant (a `Spawn` carries a full
    /// `SessionSpec` with env + bootstrap) doesn't bloat every `DaemonFrameDown`.
    Command { adapter_id: String, command: Box<AdapterCommand> },
    /// Acknowledge that an event with the given monotonic `seq` has been
    /// durably stored. Lets the daemon trim its on-disk spool.
    Ack { seq: u64 },
    /// Stage mid-chat file attachments for a running session. The
    /// daemon decodes + writes the files into the same per-session staging dir
    /// used for spawn-time uploads, then replies with a
    /// [`DaemonFrameUp::StageFilesResult`] carrying the staged absolute paths.
    /// `local_id` is the adapter-local session id (the daemon's staging key);
    /// `request_id` correlates the reply with the waiting HTTP request.
    StageFiles {
        request_id: uuid::Uuid,
        adapter_id: String,
        local_id: String,
        uploads: Vec<BootstrapFile>,
    },
    /// List the sub-directories of `path` on the daemon's machine (working-
    /// directory autocomplete in the spawn dialog). The daemon expands a
    /// leading `~`, reads one directory level, and replies with a
    /// [`DaemonFrameUp::ListDirsResult`] carrying the sorted entry names.
    ListDirs { request_id: uuid::Uuid, path: String },
    /// Git facts for `path` (spawn dialog branch badge). The daemon expands
    /// `~`, refuses paths outside its allowed roots, and replies with a
    /// [`DaemonFrameUp::GitInfoResult`]. `include_dirty` opts into a
    /// `git status` subprocess.
    GitInfo {
        request_id: uuid::Uuid,
        path: String,
        #[serde(default)]
        include_dirty: bool,
    },
    /// Read one file on the daemon's machine for the webui (a path an agent
    /// linked in a message). The daemon expands `~`, canonicalises, refuses
    /// anything outside its allow-list (temp dirs, `$HOME`, plus `cwd` — the
    /// session's working directory), and replies with a
    /// [`DaemonFrameUp::ReadFileResult`]: bytes inline when small, otherwise
    /// the sha256 of the blob it PUT to the store. `max_bytes` is the server's
    /// hard cap; larger files are refused with [`ReadFileErrorKind::TooLarge`].
    ReadFile {
        request_id: uuid::Uuid,
        path: String,
        max_bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Acknowledge chunked-transfer progress: the highest contiguous
    /// chunk index the server has reassembled for `transfer_id`, or `None` when
    /// it holds no usable prefix (unknown/evicted transfer) so the daemon
    /// restarts from chunk 0. The daemon resumes from the chunk after the
    /// acked one on the next connection.
    ChunkAck {
        transfer_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        highest_contiguous_chunk: Option<u32>,
    },
    /// Per-session transcript high-water marks, sent right after
    /// Reconcile on connect. `session_marks` maps each session's `local_id` to
    /// the server's stored transcript byte offset, so the daemon clamps its tail
    /// cursor forward and resumes instead of replaying the transcript from zero.
    ResumeMarks {
        session_marks: Vec<(String, u64)>,
        /// Sessions the server has archived on this machine; the daemon
        /// removes any claude job still on disk for one of them. Superseded by
        /// [`Self::ArchivedJobs`]; kept so frames from older servers parse.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        archived: Vec<String>,
    },
    /// Sessions the server has archived among the `claude_jobs` a
    /// [`DaemonFrameUp::Heartbeat`] reported. The daemon removes their jobs so
    /// `claude agents` converges on the archive state without a reconnect.
    /// Only ever sent to a daemon that reported `claude_jobs`.
    ArchivedJobs { session_ids: Vec<String> },
    /// Re-run codex `model/list` over a one-shot app-server (no session
    /// spawned) and ship the result as an
    /// [`AdapterEvent::CodexModels`](crate::adapter::AdapterEvent::CodexModels).
    /// Fire-and-forget: the refreshed catalog arrives on the event stream like
    /// a session-start refresh does.
    RefreshCodexModels {},
    /// Run this machine's deterministic update hook to deploy cctui `version`.
    ///
    /// Fire-and-forget on purpose: the hook restarts the very server that sent
    /// this frame, so there is no reply to wait for. Progress comes back out
    /// of band over HTTP (`POST /api/v1/daemon/update-hook/{run_id}`), where it
    /// reaches whichever server process is alive by then, and the run row in
    /// Postgres outlives them both. A daemon with no hook configured reports
    /// `failed` immediately rather than silently dropping the frame.
    RunUpdateHook { run_id: uuid::Uuid, version: String, release_url: String },
    /// Effective harness auto-update policy for this machine. Only ever sent
    /// to a daemon whose heartbeat carried a `harness` report.
    HarnessUpdatePolicy { policy: crate::harness::HarnessUpdatePolicy },
}

/// Effective secret-scrub config synced to the daemon.
///
/// The enable
/// flag plus the owner's enabled user patterns. The compiled defaults live in
/// `cctui-crypto` on both sides; the daemon combines them at compile time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretScrubConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub patterns: Vec<ScrubPattern>,
}

/// A single user-supplied scrub pattern (name + regex source). Validated
/// server-side before it ever reaches the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrubPattern {
    pub name: String,
    pub regex: String,
}

// --- Dispatcher ↔ Server (247/248) ---

/// A dispatch intent relayed from the server to an enrolled dispatcher over the
/// wire.
///
/// The dispatcher turns this into a worker container/pod on its
/// host, injecting the dispatch info into the worker env. `payload` is opaque —
/// the dispatcher forwards it verbatim (lifting `cctui_machine_key` /`name` out
/// for env injection) without otherwise inspecting it.
///
/// This is the wire mirror of the server-internal `DispatchSpec`; the borrowed
/// in-process form stays on the server, this owned form crosses the WS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireDispatchSpec {
    /// Pre-minted session id (also the runtime correlation id).
    pub session_id: String,
    /// Per-flow timeout in minutes, if the caller set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
    /// Caller resume URL — a bearer capability; do not log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_url: Option<String>,
    /// Idempotency / dedup key: the caller's logical request id (e.g.
    /// an automation dedup key like `triage-PROJ-…`). The dispatcher derives the worker
    /// Job name from THIS, not `session_id` — which is now a fresh UUID per
    /// dispatch so isolated short-lived pods never get their logs chained into
    /// one growing conversation. A duplicate webhook within a Job's lifetime
    /// still coalesces (same key ⇒ same Job name); a genuinely new round gets its
    /// own pod AND its own session. `None` ⇒ the dispatcher falls back to
    /// `session_id` (each dispatch unique, no dedup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
    /// `WorkerProfile` to instantiate, selected by name only. A dispatch may
    /// only *pick* an operator-authored profile; it can never supply raw
    /// pod-spec fields. `None` ⇒ the dispatcher falls back to a `profile` key in
    /// `payload`, then to its configured `default_profile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Free-form blob, forwarded verbatim to the worker.
    pub payload: serde_json::Value,
}

/// Frames sent by the server to an enrolled dispatcher over
/// `/api/v1/dispatcher/ws`. Peer of [`DaemonFrameDown`]; the verb is
/// Dispatch (spawn a container/pod) rather than a per-adapter command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DispatcherFrameDown {
    /// Spawn a worker for this session. The dispatcher replies with a
    /// [`DispatcherFrameUp::DispatchResult`] carrying the opaque handle and the
    /// idempotency outcome (`dispatched`/`deduplicated`/`redispatched`).
    Dispatch { request_id: uuid::Uuid, spec: WireDispatchSpec },
    /// Inspect a previously returned handle. Replies with
    /// [`DispatcherFrameUp::StatusResult`].
    Status { request_id: uuid::Uuid, handle: String },
    /// Cancel/delete a previously returned handle. Replies with
    /// [`DispatcherFrameUp::CancelResult`].
    Cancel { request_id: uuid::Uuid, handle: String },
}

/// Frames sent by an enrolled dispatcher to the server over
/// `/api/v1/dispatcher/ws`. Peer of [`DaemonFrameUp`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DispatcherFrameUp {
    /// Sent once on connect: identifies the dispatcher kind + running version.
    Hello { kind: String, version: String },
    /// Liveness ping; drives the server's last-seen/online-stale-offline tier
    /// (mirrors the daemon heartbeat).
    Heartbeat { sent_at: chrono::DateTime<chrono::Utc> },
    /// Outcome of a [`DispatcherFrameDown::Dispatch`]. `status` is the
    /// idempotency outcome surfaced verbatim to the caller; `handle` is the
    /// opaque per-dispatcher reference (e.g. `container/cctui-worker-…`).
    DispatchResult {
        request_id: uuid::Uuid,
        session_id: String,
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Outcome of a [`DispatcherFrameDown::Status`]: the lifecycle state of a
    /// handle (`running`/`complete`/`failed`/`gone`).
    StatusResult {
        request_id: uuid::Uuid,
        handle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Outcome of a [`DispatcherFrameDown::Cancel`].
    CancelResult {
        request_id: uuid::Uuid,
        handle: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

// --- Agent → Server (stream events) ---

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Free-form text. `meta` marks a message that was injected *to* the agent
    /// rather than typed by the human (harness wake-ups, `<task-notification>`,
    /// `<system-reminder>`, slash-command expansions). Set authoritatively at
    /// the adapter layer (Claude's `isMeta` + known harness tags) so clients
    /// can render it distinctly without re-sniffing strings. `#[serde(default)]`
    /// keeps older stored payloads (no field) decoding as non-meta.
    ///
    /// `seq` is a monotonic per-session insert sequence
    /// (`stream_events.id`) so clients order events causally rather than by
    /// receive-time `ts`, which can tie or invert (a late-flushed
    /// `AskUserQuestion` carries a `ts` after the user's answer). Optional so
    /// payloads still decode.
    Text {
        content: String,
        #[serde(default)]
        meta: bool,
        /// `thinking` | `redacted_thinking` | `attachment` | `system_marker` |
        /// `turn_annotation` | `queue_op`;
        /// `None` is ordinary visible prose. Free string so an unknown adapter
        /// kind still decodes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Queue verb for a `queue_op`: `queued` | `dequeued` | `removed` |
        /// `cleared`. `None` for every other kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation: Option<String>,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<crate::models::TokenUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
        /// Identity of the human turn this text belongs to. `None` for
        /// assistant text, for turns cctui did not originate, and for rows
        /// stored before the column existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<uuid::Uuid>,
    },
    ToolCall {
        tool: String,
        input: serde_json::Value,
        /// `server_tool_use` marks a provider-executed tool (web search, code
        /// execution); `None` is an ordinary client-side tool call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    ToolResult {
        tool: String,
        output_summary: String,
        /// `server_tool_result` marks the output of a provider-executed tool;
        /// `None` is an ordinary client-side tool result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default)]
        error: bool,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    Heartbeat {
        tokens_in: u64,
        tokens_out: u64,
        cost_usd: f64,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    Reply {
        content: String,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<uuid::Uuid>,
    },
    /// A context reset boundary (`/clear` or `/compact`). The session id rotates
    /// in place under the same worker; rather than splitting into a second
    /// session (archive is worker-scoped, so one `claude rm` would wipe both),
    /// we keep one session and emit this marker so clients can render the cut
    /// distinctly.
    ContextReset {
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    /// A `/compact` boundary. Unlike `/clear`, `/compact` does NOT rotate the
    /// session id — it appends an `isCompactSummary` line to the same
    /// transcript — so it surfaces as its own event carrying the summary text,
    /// rendered as a distinct "context compacted" block rather than a user
    /// message.
    CompactSummary {
        content: String,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    /// A post-turn summary. Renders as subdued footer subtext on the turn's
    /// last assistant message, not as its own bubble.
    TurnSummary {
        detail: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_category: Option<String>,
        #[serde(default)]
        needs_action: bool,
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
    TurnEnd {
        ts: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<i64>,
    },
}

impl AgentEvent {
    /// The monotonic per-session insert sequence, when the server has
    /// stamped one. `None` for freshly-normalized events before persistence and
    /// for legacy payloads persisted before the field existed.
    #[must_use]
    pub const fn seq(&self) -> Option<i64> {
        match self {
            Self::Text { seq, .. }
            | Self::ToolCall { seq, .. }
            | Self::ToolResult { seq, .. }
            | Self::Heartbeat { seq, .. }
            | Self::Reply { seq, .. }
            | Self::ContextReset { seq, .. }
            | Self::CompactSummary { seq, .. }
            | Self::TurnSummary { seq, .. }
            | Self::TurnEnd { seq, .. } => *seq,
        }
    }

    /// Stamp the causal insert sequence. Called by the server right
    /// after a successful `stream_events` insert so the live broadcast carries
    /// the same ordering key the reload path derives from `stream_events.id`.
    pub const fn set_seq(&mut self, value: i64) {
        let slot = match self {
            Self::Text { seq, .. }
            | Self::ToolCall { seq, .. }
            | Self::ToolResult { seq, .. }
            | Self::Heartbeat { seq, .. }
            | Self::Reply { seq, .. }
            | Self::ContextReset { seq, .. }
            | Self::CompactSummary { seq, .. }
            | Self::TurnSummary { seq, .. }
            | Self::TurnEnd { seq, .. } => seq,
        };
        *slot = Some(value);
    }

    /// Stamp the originating human turn's identity. Only the variants that can
    /// carry a user turn (`Text`, `Reply`) have a slot; the rest ignore it.
    pub const fn set_turn_id(&mut self, value: uuid::Uuid) {
        match self {
            Self::Text { turn_id, .. } | Self::Reply { turn_id, .. } => *turn_id = Some(value),
            _ => {}
        }
    }
}

// --- TUI → Server ---

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TuiCommand {
    Subscribe {
        session_id: String,
    },
    Unsubscribe {
        session_id: String,
    },
    /// Start (`watch: true`) or stop (`watch: false`) the read-only live
    /// terminal view for a session. The server ref-counts watchers
    /// per session and only tells the daemon to open/close its viewer PTY
    /// attach on the 0↔1 transition, so idle sessions carry no extra stream.
    WatchTerminal {
        session_id: String,
        watch: bool,
    },
    /// A typed reply from a client. `client_msg_id` (when present) lets the
    /// server ack the send back to the originating socket via
    /// [`ServerEvent::MessageAck`], so the client can render a precise
    /// per-message delivery state (sending → delivered / failed) instead of
    /// optimistically assuming a frame that left the socket was delivered.
    /// `#[serde(default)]` keeps older clients (no field) working —
    /// they simply receive no ack.
    Message {
        session_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_msg_id: Option<String>,
        /// Structured `AskUserQuestion` answer: per-question 0-based option
        /// indices, in question order. Present only when the client
        /// is answering a live ask with pure option picks (no free text) —
        /// lets the daemon drive the actual form via PTY keystrokes so claude
        /// records a genuine `tool_result` instead of "User declined to answer
        /// questions" (the ESC-dismiss fallback). `content` still carries the
        /// flattened text so older daemons (and the fallback path) work.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask_picks: Option<Vec<Vec<usize>>>,
        /// `UUIDv7` minted by the client when the human hit send, carried through
        /// the daemon onto every event this turn produces so clients dedup by
        /// identity. Absent from older clients, which fall back to content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<uuid::Uuid>,
    },
    PermissionResponse {
        session_id: String,
        request_id: String,
        behavior: String,
    },
}

// --- Server → TUI ---

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Stream {
        session_id: String,
        data: AgentEvent,
    },
    Status {
        session_id: String,
        status: crate::models::SessionStatus,
    },
    SessionRegistered {
        session: crate::models::Session,
    },
    SessionDeregistered {
        session_id: String,
    },
    PermissionRequest {
        session_id: String,
        request_id: String,
        tool_name: String,
        description: String,
        input_preview: String,
    },
    /// A previously-broadcast permission request has been resolved (by TUI
    /// or a web client). Clients should dismiss any inline prompt UI.
    PermissionResolved {
        session_id: String,
        request_id: String,
    },
    /// The agent is blocked on an `AskUserQuestion`; carries the question text
    /// so clients render a live prompt before the transcript flushes the full
    /// tool call. `questions` carries the raw `tool_input.questions`
    /// array so clients render the interactive option-card form live rather
    /// than just the flattened text.
    AskQuestion {
        session_id: String,
        question: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        questions: Option<serde_json::Value>,
        /// Assistant prose preceding the question in the same turn, so clients
        /// render the reasoning above the live prompt instead of leaving the
        /// user to answer blind. `None` when there was none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preamble: Option<String>,
    },
    /// A previously-broadcast `AskQuestion` is resolved; clients dismiss the
    /// live prompt.
    AskResolved {
        session_id: String,
    },
    /// The agent is blocked on an `ExitPlanMode` plan-approval prompt; carries
    /// the plan markdown so clients render a live Plan card with the
    /// continuation options before the transcript flushes the tool call.
    PlanRequest {
        session_id: String,
        plan: String,
        /// Assistant prose preceding the plan in the same turn. `None` when
        /// there was none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preamble: Option<String>,
    },
    /// A previously-broadcast `PlanRequest` is resolved; clients dismiss the
    /// live Plan card.
    PlanResolved {
        session_id: String,
    },
    /// Outcome of a client-initiated command (currently `POST /sessions/spawn`).
    /// `command_id` matches the value returned by the spawn route so the
    /// originating client can surface success/failure instead of silently
    /// polling.
    CommandResult {
        command_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Set when the command targeted a session the server knows (interrupt,
        /// set-model, a failed spawn persisted as a row); scopes delivery to
        /// that session's owner.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// A session reached its end of life: the `sessions` row now carries
    /// `end_reason` / `end_detail`. List-level peer of the `session_ended`
    /// stream event, so clients can toast a failure without subscribing.
    SessionEnded {
        session_id: String,
        reason: crate::models::SessionEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Outcome of a client-sent [`TuiCommand::Message`] carrying a
    /// `client_msg_id`. Sent only to the originating socket. `ok=false` means
    /// the server could not dispatch the reply to the session's daemon (e.g.
    /// the daemon was momentarily offline — `NoDaemon`/`Closed`), so the client
    /// should mark the message failed and offer a retry rather than leaving it
    /// stuck "sending…" until it silently vanishes on the next resubscribe.
    MessageAck {
        session_id: String,
        client_msg_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Correlation id of the dispatched `Reply`. An `ok` ack only says the
        /// frame was queued toward a daemon; the client awaits the adapter's
        /// [`ServerEvent::CommandResult`] under this id for actual delivery.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_id: Option<uuid::Uuid>,
    },
    /// A machine has just reported a fresh expected-files manifest.
    ArchiveManifest {
        machine_id: uuid::Uuid,
        count: i64,
    },
    /// A machine's liveness tier just changed. Derived from the age
    /// of `machines.last_seen_at`, which the server advances on every daemon
    /// `Heartbeat`. Broadcast on transition so webui/TUI can flip a machine to
    /// offline within one liveness window without waiting for a failed dispatch.
    MachineLiveness {
        machine_id: uuid::Uuid,
        liveness: crate::models::MachineLiveness,
    },
    /// A daemon heartbeat carried a fresh host resource snapshot. Broadcast on
    /// every heartbeat so a pinned header gauge follows the machine live.
    MachineResources {
        machine_id: uuid::Uuid,
        resources: crate::resources::MachineResources,
    },
    /// An account's usage windows were just refreshed upstream. Broadcast from
    /// the refresh itself, so pushing costs no extra upstream call and a header
    /// battery follows real consumption instead of its own poll. `usage` is the
    /// serialized row the accounts usage routes return.
    AccountUsage {
        account_id: uuid::Uuid,
        usage: serde_json::Value,
    },
    /// An enrolled dispatcher's liveness tier just changed. Peer of
    /// [`Self::MachineLiveness`], derived from `dispatchers.last_seen_at`.
    DispatcherLiveness {
        dispatcher_id: uuid::Uuid,
        liveness: crate::models::MachineLiveness,
    },
    /// A single archive file has just finished uploading.
    ArchiveUploaded {
        machine_id: uuid::Uuid,
        project_dir: String,
        session_id: String,
        size_bytes: i64,
        sha256: String,
    },
    /// A piece of synced GitHub state was just upserted by the `github`
    /// connector (webhook or reconcile poll), so the `/github` inbox can
    /// refresh the affected PR without a full poll (docs §6.1 "Live push").
    ///
    /// Carried as one envelope rather than a variant per object so new GitHub
    /// object kinds don't churn the wire enum; `kind` tells the client what
    /// changed and `payload` is a small, credential-free locator (repo +
    /// stable ids — never tokens or raw webhook bodies). Clients refetch the
    /// affected rows over HTTP; the event is only a "something changed" nudge.
    GithubEvent {
        kind: crate::github::GithubEventKind,
        payload: crate::github::GithubEventPayload,
    },
    /// A session's gateway request was just refused by the per-account soft
    /// limit: cctui's own share of the account's usage window is at
    /// cap, so the worker got a 429 and the conversation stalled. Broadcast on
    /// the clear→blocked transition so the webui can show a per-chat banner
    /// offering to continue on another same-provider account. `reason` is the
    /// human-readable 429 body; `retry_after_secs` mirrors the `Retry-After`.
    SoftLimitReached {
        session_id: String,
        account_id: uuid::Uuid,
        account_name: String,
        reason: String,
        retry_after_secs: i64,
    },
    /// A session's soft-limit block has cleared: either a later
    /// passthrough succeeded, or the user rebound the session to another
    /// account via `POST /sessions/{id}/switch-account`. Clients dismiss the
    /// per-chat soft-limit banner.
    SoftLimitCleared {
        session_id: String,
    },
    /// A coalesced slice of a session's live PTY byte stream, relayed to the
    /// browsers watching its read-only terminal. `data` is
    /// standard-base64 of the raw terminal bytes; the client base64-decodes and
    /// writes it straight into xterm.js. Not persisted — dropped by any client
    /// not currently rendering the terminal.
    PtyChunk {
        session_id: String,
        data: String,
    },
    /// Liveness tick for the browser socket, on the same interval as the
    /// WebSocket `Ping`. The JS `WebSocket` API exposes no ping/pong event, so a
    /// client watchdog can only be fed by an application frame.
    Heartbeat {},
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_tagged_serialization() {
        let event = AgentEvent::Text {
            content: "hello".into(),
            meta: false,
            kind: None,
            operation: None,
            ts: 1_234_567_890,
            message_id: None,
            usage: None,
            seq: None,
            turn_id: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"text""#));
        assert!(json.contains(r#""content":"hello""#));
    }

    #[test]
    fn agent_event_seq_roundtrips_and_defaults_to_none() {
        // Legacy payload (no seq field) decodes as None.
        let legacy = r#"{"type":"turn_end","ts":5}"#;
        let ev: AgentEvent = serde_json::from_str(legacy).unwrap();
        assert_eq!(ev.seq(), None);

        // A stamped seq survives a wire roundtrip and is readable via `seq()`.
        let mut ev = AgentEvent::Text {
            content: "hi".into(),
            meta: false,
            kind: None,
            operation: None,
            ts: 10,
            message_id: None,
            usage: None,
            seq: None,
            turn_id: None,
        };
        ev.set_seq(42);
        assert_eq!(ev.seq(), Some(42));
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""seq":42"#));
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq(), Some(42));
    }

    #[test]
    fn agent_event_seq_orders_ask_turn_when_ts_ties_or_inverts() {
        // Reload scenario: a late-flushed AskUserQuestion card+preamble
        // carry a `ts` at/after the user's answer, but their insert `seq` is
        // lower. Ordering by `seq` restores causal order; ordering by `ts` does
        // not. `seq` is the DB insert sequence, always a strict total order.
        let preamble = AgentEvent::Text {
            content: "Here is my analysis.".into(),
            meta: false,
            kind: None,
            operation: None,
            ts: 100, // ties the answer's ts
            message_id: None,
            usage: None,
            seq: Some(1),
            turn_id: None,
        };
        let card = AgentEvent::ToolCall {
            tool: "AskUserQuestion".into(),
            input: serde_json::json!({}),
            kind: None,
            ts: 100, // ties, and flushed late
            seq: Some(2),
        };
        let answer = AgentEvent::Text {
            content: "▷ User: option A".into(),
            meta: false,
            kind: None,
            operation: None,
            ts: 100,
            message_id: None,
            usage: None,
            seq: Some(3),
            turn_id: None,
        };
        // Deliberately shuffled so a stable ts-only sort would leave the answer
        // ahead of its own question.
        let mut events = [answer, preamble, card];
        events.sort_by_key(super::AgentEvent::seq);
        let seqs: Vec<Option<i64>> = events.iter().map(AgentEvent::seq).collect();
        assert_eq!(seqs, vec![Some(1), Some(2), Some(3)]);
        // The user answer now renders last, after its preamble + card.
        assert!(matches!(&events[2], AgentEvent::Text { content, .. } if content.contains("User")));
    }

    #[test]
    fn tui_command_tagged_serialization() {
        let cmd = TuiCommand::Subscribe { session_id: "test-session".into() };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""type":"subscribe""#));
    }

    #[test]
    fn agent_event_reply_serialization() {
        let event =
            AgentEvent::Reply { content: "acknowledged".into(), ts: 100, seq: None, turn_id: None };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"reply""#));
        assert!(json.contains(r#""content":"acknowledged""#));
    }

    #[test]
    fn agent_event_tool_call_serialization() {
        let event = AgentEvent::ToolCall {
            tool: "Bash".into(),
            input: serde_json::json!({"command": "ls"}),
            kind: None,
            ts: 42,
            seq: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"tool_call""#));
        assert!(json.contains(r#""tool":"Bash""#));
    }

    #[test]
    fn agent_event_tool_result_serialization() {
        let event = AgentEvent::ToolResult {
            tool: "Bash".into(),
            output_summary: "file.txt".into(),
            kind: None,
            error: false,
            ts: 42,
            seq: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"tool_result""#));
        assert!(json.contains(r#""output_summary":"file.txt""#));
    }

    #[test]
    fn agent_event_heartbeat_serialization() {
        let event = AgentEvent::Heartbeat {
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.01,
            ts: 42,
            seq: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"heartbeat""#));
        assert!(json.contains(r#""tokens_in":100"#));
    }

    #[test]
    fn agent_event_roundtrip_all_variants() {
        let variants = vec![
            AgentEvent::Text {
                content: "hello".into(),
                meta: false,
                kind: None,
                operation: None,
                ts: 1,
                message_id: None,
                usage: None,
                seq: None,
                turn_id: None,
            },
            AgentEvent::ToolCall {
                tool: "Read".into(),
                input: serde_json::json!({}),
                kind: None,
                ts: 2,
                seq: None,
            },
            AgentEvent::ToolResult {
                tool: "Read".into(),
                output_summary: "ok".into(),
                kind: Some("server_tool_result".into()),
                error: true,
                ts: 3,
                seq: None,
            },
            AgentEvent::Heartbeat {
                tokens_in: 10,
                tokens_out: 5,
                cost_usd: 0.001,
                ts: 4,
                seq: None,
            },
            AgentEvent::Reply { content: "done".into(), ts: 5, seq: None, turn_id: None },
            AgentEvent::TurnEnd { ts: 6, seq: None },
        ];
        for event in variants {
            let json = serde_json::to_string(&event).unwrap();
            let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
            let re_json = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(json, re_json, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn server_event_serialization() {
        let event = ServerEvent::Stream {
            session_id: "test-session".into(),
            data: AgentEvent::Text {
                content: "hi".into(),
                meta: false,
                kind: None,
                operation: None,
                ts: 1,
                message_id: None,
                usage: None,
                seq: None,
                turn_id: None,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"stream""#));
    }

    #[test]
    fn daemon_frame_up_event_serializes_tagged() {
        let f = DaemonFrameUp::Event {
            adapter_id: "claude-code".into(),
            event: AdapterEvent::SessionStarted {
                local_id: "abc".into(),
                meta: crate::adapter::SessionMeta::default(),
            },
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"event""#));
        assert!(json.contains(r#""adapter_id":"claude-code""#));
        let _back: DaemonFrameUp = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn heartbeat_carries_bandwidth_and_accepts_legacy_payload() {
        let hb = DaemonFrameUp::Heartbeat {
            sent_at: chrono::Utc::now(),
            bandwidth: Some(crate::bandwidth::BandwidthSummary {
                forward: 900,
                blob_put: 42,
                ..Default::default()
            }),
            update_hook: Some(true),
            resources: Some(crate::resources::MachineResources {
                cpu_pct: 12.5,
                ..Default::default()
            }),
            claude_jobs: Some(vec!["deadbeef".into()]),
            harness: Some(crate::harness::HarnessReport::default()),
        };
        let json = serde_json::to_string(&hb).unwrap();
        assert!(json.contains(r#""forward":900"#), "{json}");
        assert!(json.contains(r#""claude_jobs":["deadbeef"]"#), "{json}");
        assert!(json.contains(r#""cpu_pct":12.5"#), "{json}");
        assert!(json.contains(r#""blob_put":42"#), "{json}");
        assert!(json.contains(r#""update_hook":true"#), "{json}");

        let legacy = r#"{"type":"heartbeat","sent_at":"2026-07-21T00:00:00Z"}"#;
        let back: DaemonFrameUp = serde_json::from_str(legacy).unwrap();
        match back {
            // A daemon that predates either field says nothing about both; the
            // server must not read that silence as "no hook".
            DaemonFrameUp::Heartbeat {
                bandwidth,
                update_hook,
                resources,
                claude_jobs,
                harness,
                ..
            } => {
                assert!(harness.is_none());
                assert!(bandwidth.is_none());
                assert!(update_hook.is_none());
                assert!(resources.is_none());
                assert!(claude_jobs.is_none());
            }
            _ => panic!("expected Heartbeat"),
        }
    }

    #[test]
    fn run_update_hook_roundtrips() {
        let f = DaemonFrameDown::RunUpdateHook {
            run_id: uuid::Uuid::nil(),
            version: "0.7.319".into(),
            release_url: "https://github.com/DorskFR/cctui/releases/tag/v0.7.319".into(),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"run_update_hook""#), "{json}");
        let back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
        match back {
            DaemonFrameDown::RunUpdateHook { version, .. } => assert_eq!(version, "0.7.319"),
            _ => panic!("expected RunUpdateHook"),
        }
    }

    #[test]
    fn harness_update_policy_roundtrips() {
        let f = DaemonFrameDown::HarnessUpdatePolicy {
            policy: crate::harness::HarnessUpdatePolicy { enabled: true, ..Default::default() },
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"harness_update_policy""#), "{json}");
        match serde_json::from_str::<DaemonFrameDown>(&json).unwrap() {
            DaemonFrameDown::HarnessUpdatePolicy { policy } => assert!(policy.enabled),
            _ => panic!("expected HarnessUpdatePolicy"),
        }
    }

    #[test]
    fn daemon_frame_down_reconcile_roundtrips() {
        let f = DaemonFrameDown::Reconcile {
            adapters: vec![],
            secret_scrub: SecretScrubConfig::default(),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"reconcile""#));
        let _back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn daemon_frame_down_command_roundtrips() {
        let f = DaemonFrameDown::Command {
            adapter_id: "claude-code".into(),
            command: Box::new(AdapterCommand::Kill { local_id: "abc".into(), signal: None }),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"command""#));
        let _back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn daemon_frame_down_resume_marks_roundtrips() {
        let f = DaemonFrameDown::ResumeMarks {
            session_marks: vec![("sess-1".into(), 4096), ("sess-2".into(), 0)],
            archived: vec!["sess-3".into()],
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"resume_marks""#));
        let back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
        match back {
            DaemonFrameDown::ResumeMarks { session_marks, archived } => {
                assert_eq!(session_marks, vec![("sess-1".into(), 4096), ("sess-2".into(), 0)]);
                assert_eq!(archived, vec!["sess-3".to_string()]);
            }
            _ => panic!("expected ResumeMarks"),
        }
    }

    #[test]
    fn daemon_frame_down_archived_jobs_roundtrips() {
        let f = DaemonFrameDown::ArchivedJobs { session_ids: vec!["sess-3".into()] };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"archived_jobs""#));
        match serde_json::from_str::<DaemonFrameDown>(&json).unwrap() {
            DaemonFrameDown::ArchivedJobs { session_ids } => {
                assert_eq!(session_ids, vec!["sess-3".to_string()]);
            }
            _ => panic!("expected ArchivedJobs"),
        }
    }

    #[test]
    fn tui_command_message_serialization() {
        let cmd = TuiCommand::Message {
            session_id: "test-session".into(),
            content: "hello".into(),
            client_msg_id: None,
            ask_picks: None,
            turn_id: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""type":"message""#));
        assert!(json.contains(r#""content":"hello""#));
        let deserialized: TuiCommand = serde_json::from_str(&json).unwrap();
        match deserialized {
            TuiCommand::Message { content, .. } => assert_eq!(content, "hello"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tui_command_message_omits_client_msg_id_when_none() {
        // Old clients send no `client_msg_id`; the field is skipped on the wire
        // so the payload stays byte-compatible with readers.
        let cmd = TuiCommand::Message {
            session_id: "s".into(),
            content: "hi".into(),
            client_msg_id: None,
            ask_picks: None,
            turn_id: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(!json.contains("client_msg_id"), "None must be skipped: {json}");
    }

    #[test]
    fn tui_command_message_accepts_legacy_payload_without_client_msg_id() {
        // A frame from an older client (no field) must still decode (serde default).
        let legacy = r#"{"type":"message","session_id":"s","content":"hi"}"#;
        let cmd: TuiCommand = serde_json::from_str(legacy).unwrap();
        match cmd {
            TuiCommand::Message { client_msg_id, .. } => assert_eq!(client_msg_id, None),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tui_command_message_carries_client_msg_id_when_set() {
        let cmd = TuiCommand::Message {
            session_id: "s".into(),
            content: "hi".into(),
            client_msg_id: Some("abc-123".into()),
            ask_picks: None,
            turn_id: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""client_msg_id":"abc-123""#));
        let back: TuiCommand = serde_json::from_str(&json).unwrap();
        match back {
            TuiCommand::Message { client_msg_id, .. } => {
                assert_eq!(client_msg_id.as_deref(), Some("abc-123"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_event_message_ack_roundtrips() {
        let ev = ServerEvent::MessageAck {
            session_id: "s".into(),
            client_msg_id: "abc-123".into(),
            ok: false,
            error: Some("no daemon connected for machine …".into()),
            command_id: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"message_ack""#));
        assert!(json.contains(r#""ok":false"#));
        assert!(json.contains(r#""client_msg_id":"abc-123""#));
        let _back: ServerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn dispatcher_frame_down_dispatch_roundtrips() {
        let f = DispatcherFrameDown::Dispatch {
            request_id: uuid::Uuid::nil(),
            spec: WireDispatchSpec {
                session_id: "sess-1".into(),
                timeout_minutes: Some(30),
                reply_url: None,
                dedup_key: None,
                profile: None,
                payload: serde_json::json!({"name": "demo"}),
            },
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"dispatch""#));
        assert!(!json.contains("reply_url"), "None reply_url must be skipped: {json}");
        let _back: DispatcherFrameDown = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn dispatcher_frame_up_roundtrips() {
        let frames = vec![
            DispatcherFrameUp::Hello { kind: "docker".into(), version: "0.0.0".into() },
            DispatcherFrameUp::Heartbeat { sent_at: chrono::Utc::now() },
            DispatcherFrameUp::DispatchResult {
                request_id: uuid::Uuid::nil(),
                session_id: "sess-1".into(),
                handle: "container/cctui-worker-abc".into(),
                namespace: None,
                status: Some("dispatched".into()),
                error: None,
            },
            DispatcherFrameUp::StatusResult {
                request_id: uuid::Uuid::nil(),
                handle: "container/cctui-worker-abc".into(),
                state: Some("running".into()),
                error: None,
            },
            DispatcherFrameUp::CancelResult {
                request_id: uuid::Uuid::nil(),
                handle: "container/cctui-worker-abc".into(),
                ok: true,
                error: None,
            },
        ];
        for f in frames {
            let json = serde_json::to_string(&f).unwrap();
            let _back: DispatcherFrameUp = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn daemon_frame_down_list_dirs_roundtrips() {
        let f = DaemonFrameDown::ListDirs { request_id: uuid::Uuid::nil(), path: "/home".into() };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"list_dirs""#));
        let _back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn daemon_frame_down_refresh_codex_models_roundtrips() {
        let json = serde_json::to_string(&DaemonFrameDown::RefreshCodexModels {}).unwrap();
        assert!(json.contains(r#""type":"refresh_codex_models""#));
        let back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, DaemonFrameDown::RefreshCodexModels {}));
    }

    #[test]
    fn daemon_frame_up_list_dirs_result_roundtrips() {
        let f = DaemonFrameUp::ListDirsResult {
            request_id: uuid::Uuid::nil(),
            ok: true,
            dirs: vec!["projects".into()],
            error: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains(r#""type":"list_dirs_result""#));
        assert!(!json.contains("error"), "None error must be skipped: {json}");
        let _back: DaemonFrameUp = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn daemon_read_file_frames_roundtrip() {
        let down = DaemonFrameDown::ReadFile {
            request_id: uuid::Uuid::nil(),
            path: "~/out/report.md".into(),
            max_bytes: READ_FILE_MAX_BYTES,
            cwd: Some("/home/u/proj".into()),
        };
        let json = serde_json::to_string(&down).unwrap();
        assert!(json.contains(r#""type":"read_file""#));
        let _back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
        let legacy = r#"{"type":"read_file","request_id":"00000000-0000-0000-0000-000000000000","path":"/x","max_bytes":1}"#;
        let back: DaemonFrameDown = serde_json::from_str(legacy).unwrap();
        assert!(matches!(back, DaemonFrameDown::ReadFile { cwd: None, .. }));

        let up = DaemonFrameUp::ReadFileResult {
            request_id: uuid::Uuid::nil(),
            ok: true,
            file: Some(ReadFileOk {
                name: "report.md".into(),
                size: 3,
                sha256: "ab".into(),
                media_type: Some("text/markdown".into()),
                data: Some("YWJj".into()),
                blob_hash: None,
            }),
            error_kind: None,
            error: None,
        };
        let json = serde_json::to_string(&up).unwrap();
        assert!(json.contains(r#""type":"read_file_result""#));
        assert!(!json.contains("blob_hash"), "None fields must be skipped: {json}");
        let _back: DaemonFrameUp = serde_json::from_str(&json).unwrap();

        let err = DaemonFrameUp::ReadFileResult {
            request_id: uuid::Uuid::nil(),
            ok: false,
            file: None,
            error_kind: Some(ReadFileErrorKind::TooLarge),
            error: Some("too big".into()),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains(r#""error_kind":"too_large""#));
        let _back: DaemonFrameUp = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn daemon_git_info_frames_roundtrip() {
        let down = DaemonFrameDown::GitInfo {
            request_id: uuid::Uuid::nil(),
            path: "~/repo".into(),
            include_dirty: false,
        };
        let json = serde_json::to_string(&down).unwrap();
        assert!(json.contains(r#""type":"git_info""#));
        let _back: DaemonFrameDown = serde_json::from_str(&json).unwrap();
        // Legacy senders omit include_dirty.
        let legacy = r#"{"type":"git_info","request_id":"00000000-0000-0000-0000-000000000000","path":"/x"}"#;
        let back: DaemonFrameDown = serde_json::from_str(legacy).unwrap();
        assert!(matches!(back, DaemonFrameDown::GitInfo { include_dirty: false, .. }));

        let up = DaemonFrameUp::GitInfoResult {
            request_id: uuid::Uuid::nil(),
            ok: true,
            info: Some(crate::git::GitInfo {
                is_repo: true,
                branch: Some("main".into()),
                ..Default::default()
            }),
            error: None,
        };
        let json = serde_json::to_string(&up).unwrap();
        assert!(json.contains(r#""type":"git_info_result""#));
        assert!(!json.contains("detached_sha"), "None fields must be skipped: {json}");
        let _back: DaemonFrameUp = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn watch_terminal_command_and_pty_chunk_event_roundtrip() {
        let cmd = TuiCommand::WatchTerminal { session_id: "s1".into(), watch: true };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains(r#""type":"watch_terminal""#));
        assert!(json.contains(r#""watch":true"#));
        let _back: TuiCommand = serde_json::from_str(&json).unwrap();

        let ev = ServerEvent::PtyChunk { session_id: "s1".into(), data: "aGk=".into() };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"pty_chunk""#));
        assert!(json.contains(r#""data":"aGk=""#));
        let _back: ServerEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn server_event_message_ack_omits_error_when_ok() {
        let ev = ServerEvent::MessageAck {
            session_id: "s".into(),
            client_msg_id: "abc-123".into(),
            ok: true,
            error: None,
            command_id: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("error"), "None error must be skipped: {json}");
    }

    #[test]
    fn server_event_heartbeat_serializes_with_type_tag() {
        let json = serde_json::to_string(&ServerEvent::Heartbeat {}).unwrap();
        assert_eq!(json, r#"{"type":"heartbeat"}"#);
        let back: ServerEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ServerEvent::Heartbeat {}));
    }
}
