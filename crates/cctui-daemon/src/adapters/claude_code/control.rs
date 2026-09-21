//! Driver for the `claude daemon` control-socket adapter path.
//!
//! Polls `list` every `poll_interval`, diffs against the previous roster
//! to emit `SessionStarted` / `SessionEnded`, and merges identity fields
//! from `~/.claude/jobs/<short>/state.json` to produce `Status` events.
//!
//! Per-session `subscribe` streams and the transcript tail land in
//! Phase 3 — `list` already gives us state/tempo/detail at the
//! 2s poll cadence.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use cctui_proto::adapter::{AdapterCommand, AdapterEvent, EndReason, JobShort, SessionMeta};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use cctui_proto::diagnose::{
    AttachStatus, DiagnoseFact, DispatchStatus, EffectiveState, GatewayStatus, HookEvent,
    PendingPrompts, PtyOutputStats, SessionDiagnose, SocketStatus, TranscriptStatus,
};

use super::backfill::{self, BackfillConfig, CursorFile, default_cursor_path};
use super::diagnose::{
    ActivityInput, ActivityVerdict, ArbitrationInput, arbitrate, arbitrate_activity, now_unix_ms,
    to_unix_ms,
};
use super::discovery::Discovery;
use super::dispatch_done::{self, DispatchDoneTracker};
use super::kickstart::Kickstarter;
use super::state::{StateJson, default_jobs_root};
use super::transcript::{self, OffsetStore, default_projects_root};
use super::{SessionMap, socket};
use crate::git::{read_git_branch, read_git_remote};

/// Config knobs read from `adapters_enabled.config`.
#[derive(Debug, Clone)]
pub struct DriverConfig {
    pub poll_interval: Duration,
    pub jobs_root: PathBuf,
    pub projects_root: PathBuf,
    /// Override the discovery base for tests / non-standard layouts.
    pub discovery: Discovery,
    /// Optional override for the transcript-offsets store path. `None`
    /// uses the default `$XDG_CONFIG_HOME/cctui/transcript-offsets.json`.
    pub offsets_path: Option<PathBuf>,
    /// Optional override for the backfill cursor path. `None` uses the
    /// default `$XDG_CONFIG_HOME/cctui/backfill.json`.
    pub backfill_cursor_path: Option<PathBuf>,
    /// Skip the startup backfill pass. Default: false (backfill runs).
    pub skip_backfill: bool,
    /// Binary used for the `claude rm <short>` removal invoked by
    /// [`AdapterCommand::Remove`]. Defaults to `claude` (resolved on `PATH`).
    pub claude_bin: String,
    /// Local socket the `AskUserQuestion` hook delivers to. Shared
    /// with the listener spawned in [`super::ClaudeCodeAdapter::start`] so the
    /// injected `--settings` hook command targets the same path the daemon
    /// binds.
    pub hook_socket_path: PathBuf,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            jobs_root: default_jobs_root(),
            projects_root: default_projects_root(),
            discovery: Discovery::for_current_user(),
            offsets_path: None,
            backfill_cursor_path: None,
            skip_backfill: false,
            claude_bin: "claude".to_string(),
            hook_socket_path: super::resolve_legacy_socket_path(&serde_json::Value::Null),
        }
    }
}

impl DriverConfig {
    pub fn from_value(v: &serde_json::Value) -> Self {
        let mut cfg = Self::default();
        if let Some(ms) = v.get("poll_interval_ms").and_then(serde_json::Value::as_u64) {
            cfg.poll_interval = Duration::from_millis(ms);
        }
        if let Some(p) = v.get("jobs_root").and_then(serde_json::Value::as_str) {
            cfg.jobs_root = PathBuf::from(p);
        }
        if let Some(p) = v.get("projects_root").and_then(serde_json::Value::as_str) {
            cfg.projects_root = PathBuf::from(p);
        }
        if let Some(p) = v.get("discovery_base").and_then(serde_json::Value::as_str) {
            cfg.discovery = Discovery::with_base(PathBuf::from(p));
        }
        if let Some(p) = v.get("offsets_path").and_then(serde_json::Value::as_str) {
            cfg.offsets_path = Some(PathBuf::from(p));
        }
        if let Some(p) = v.get("backfill_cursor_path").and_then(serde_json::Value::as_str) {
            cfg.backfill_cursor_path = Some(PathBuf::from(p));
        }
        if let Some(b) = v.get("skip_backfill").and_then(serde_json::Value::as_bool) {
            cfg.skip_backfill = b;
        }
        if let Some(s) = v.get("claude_bin").and_then(serde_json::Value::as_str) {
            cfg.claude_bin = s.to_string();
        }
        cfg.hook_socket_path = super::resolve_legacy_socket_path(v);
        cfg
    }
}

/// The `list` op returns `{ok: true, op: "list", jobs: [LiveSnapshot]}`.
#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    jobs: Vec<LiveSnapshot>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct LiveSnapshot {
    pub short: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default, alias = "sessionId")]
    pub session_id_camel: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tempo: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    /// Set by the claude daemon when the worker is awaiting a decision; for a
    /// tool-permission prompt it reads e.g. `"approve Bash: touch /tmp/x"`.
    /// Empty/absent when nothing is pending.
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dying: bool,
    /// Claude's per-session "process gone" flag. When the worker
    /// process exits while still listed (e.g. it died while the supervisor was
    /// down — "process gone while supervisor down"), claude keeps the entry in
    /// `daemon list` but marks it dead. We have no live known-dead sample of
    /// the exact wire shape, so we parse DEFENSIVELY: a boolean `gone`/`dead`
    /// flag, OR an explicit `alive: false`, OR a terminal `status` string
    /// (`gone`/`exited`/`dead`). `is_dead()` folds them together.
    #[serde(default)]
    pub gone: bool,
    #[serde(default)]
    pub dead: bool,
    /// Defensive: some builds may report liveness positively. `Some(false)`
    /// means dead; `None`/`Some(true)` mean "no signal / alive".
    #[serde(default)]
    pub alive: Option<bool>,
    /// Defensive: a free-form lifecycle string distinct from `state`/`tempo`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, alias = "cliVersion")]
    pub cli_version: Option<String>,
}

impl LiveSnapshot {
    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref().or(self.session_id_camel.as_deref())
    }

    /// §7.2 of the protocol doc: skip spares and dying workers.
    fn is_user_visible(&self) -> bool {
        !self.dying && self.source.as_deref() != Some("spare")
    }

    /// Whether claude reports this still-listed session as dead / "process
    /// gone". Parsed DEFENSIVELY across the plausible wire shapes:
    ///   - boolean `gone` / `dead` flags,
    ///   - `alive: false`,
    ///   - a terminal `status`/`state`/`tempo` string
    ///     (`gone`/`exited`/`dead`/`process gone`),
    ///   - a `detail` that CONTAINS `process gone` — the live wire shape we
    ///     actually observed is `state:"failed"`, `tempo:"idle"`,
    ///     `detail:"process gone while supervisor was down"`. None of
    ///     those fields exact-match a terminal token, so the session would
    ///     otherwise linger showing that detail with a non-terminal status.
    ///
    /// `dying` is handled separately (it filters the session out entirely), so
    /// it is intentionally NOT folded in here.
    fn is_dead(&self) -> bool {
        const TERMINAL: &[&str] = &["gone", "exited", "dead", "process gone"];
        let terminal_str = |o: &Option<String>| {
            o.as_deref().is_some_and(|s| {
                let s = s.trim().to_ascii_lowercase();
                TERMINAL.contains(&s.as_str())
            })
        };
        // The "process gone" phrase arrives wrapped in a longer sentence in the
        // `detail` field (e.g. "process gone while supervisor was down"), so we
        // match it as a substring rather than an exact token.
        let detail_gone =
            self.detail.as_deref().is_some_and(|s| s.to_ascii_lowercase().contains("process gone"));
        self.gone
            || self.dead
            || self.alive == Some(false)
            || detail_gone
            || terminal_str(&self.status)
            || terminal_str(&self.state)
            || terminal_str(&self.tempo)
    }
}

/// Spawn-time `(model, effort)` pair remembered per worker `short`.
type SpawnModelEffort = (Option<String>, Option<String>);

/// How long after a reply op its turn id keeps landing on the session's user
/// events. Claude emits the re-encodings within seconds; anything later is a
/// different turn, most likely typed into the native TUI.
const TURN_ID_WINDOW: Duration = Duration::from_mins(1);

#[derive(Clone, Copy)]
struct PendingTurn {
    id: uuid::Uuid,
    at: Instant,
}

/// A prepared spawn/fork `dispatch` request whose control-socket round-trip
/// is awaited outside the run loop, so a slow or silent claude daemon can't
/// wedge polling and every later command for the adapter.
pub struct DeferredDispatch {
    sock: PathBuf,
    req: serde_json::Value,
    short: String,
    what: String,
    session_id: String,
}

impl DeferredDispatch {
    /// Send the dispatch and await the daemon's reply. An `ok:false` reply or
    /// a silent daemon (see [`socket::ONE_SHOT_TIMEOUT`]) is an error, and the
    /// worker's managed config files are swept so nothing dangles.
    pub async fn send(self) -> anyhow::Result<()> {
        let resp: serde_json::Value = socket::call(&self.sock, &self.req)
            .await
            .inspect_err(|_| crate::configsweep::remove_session_files(&self.short))
            .with_context(|| format!("dispatch {}", self.what))?;
        tracing::info!(?resp, session_id = %self.session_id, "{} dispatched via control socket", self.what);
        Ok(())
    }

    /// [`send`](Self::send) on its own task, reporting the outcome as the
    /// `CommandResult` for `command_id`.
    fn run_detached(self, events: mpsc::Sender<AdapterEvent>, command_id: Option<uuid::Uuid>) {
        tokio::spawn(async move {
            let res = self.send().await;
            if let Err(err) = &res {
                tracing::warn!(%err, "command dispatch failed");
            }
            Driver::report_command(&events, command_id, res).await;
        });
    }
}

pub struct Driver {
    cfg: DriverConfig,
    events: mpsc::Sender<AdapterEvent>,
    /// Inbound: commands routed from server → daemon → adapter.
    commands: mpsc::Receiver<AdapterCommand>,
    shutdown: CancellationToken,
    roster: HashSet<String>,
    last_status: HashMap<String, StatusSnapshot>,
    /// Shorts claude reports dead-but-still-listed. Once we emit the
    /// dead transition (hibernated or `SessionEnded`) we record the short here
    /// and suppress further live-status emits for it, so the still-present
    /// roster entry can't re-emit a non-terminal Status and re-green the dot
    /// (daemon-side sticky, mirroring the server's sticky terminal status).
    /// Cleared when the worker revives (reports alive again) or drops
    /// off the roster.
    dead_shorts: HashSet<String>,
    /// Shared `session_id → stable local_id` map. Populated as transcripts are
    /// pinned (incl. across `/clear` rotations) and read by the ask-hook
    /// listener so a hook's live `session_id` resolves to the `local_id` the
    /// server keys on.
    session_to_local: SessionMap,
    /// Reverse lookup: `local_id` (`session_id`) → worker `short`. Built
    /// from list snapshots so command dispatch can target the right
    /// worker even though the server identifies sessions by their
    /// `session_id`.
    short_by_session: HashMap<String, String>,
    /// Per-session transcript byte offsets, persisted across daemon
    /// restarts to avoid replay.
    offsets: OffsetStore,
    /// Cache: short → (cwd, `session_id`) so we can locate the transcript
    /// without re-reading `state.json` on every tick.
    transcript_locations: HashMap<String, TranscriptLocation>,
    /// Task-tool subagents currently tracked, keyed by `agentId`.
    /// Observe-only nested sessions discovered by scanning each parent's
    /// `subagents/` transcript directory.
    subagents: HashMap<String, SubagentState>,
    /// `agentId`s already ended (via quiescence). Prevents a finished
    /// subagent's still-present transcript from being rediscovered and
    /// re-announced on the next poll.
    ended_subagents: HashSet<String>,
    /// Self-heals the on-demand `claude daemon`: when the control socket is
    /// missing (idle shutdown, sleep, teardown) this boots it via `claude
    /// agents --json` so polling/dispatch stop failing with "no claude daemon
    /// socket present".
    kickstarter: Kickstarter,
    /// Cycles the claude daemon when a CLI auto-update left it on an older
    /// version, but only while no worker is running.
    version_gate: super::version_gate::VersionGate,
    /// Holds a persistent headless `attach` open per live session so the
    /// dispatched worker actually wakes (focus-in seed) and is kept off the
    /// 60s idle-retire path. Without this, dispatched/replied sessions sit in
    /// limbo until a human opens them in `claude agents`.
    attach: super::attach::AttachManager,
    /// Tool-permission prompts currently pending, keyed by worker `short`.
    /// Derived from the snapshot's `tempo:"blocked"`/`needs` signal:
    /// a fresh/changed `needs` emits a `PermissionRequest`, and clearing it
    /// emits `PermissionResolved`. Dedups so a still-pending prompt isn't
    /// re-emitted on every poll.
    pending_perms: HashMap<String, PendingPerm>,
    /// Monotonic counter minting synthesized permission `request_id`s. Claude's
    /// control socket exposes no id for an interactive prompt (the `needs`
    /// string is all we get), so we mint our own purely as a correlation token
    /// the server/clients echo back; the answer is keyed on the worker `short`.
    perm_seq: u64,
    /// Sessions with an `AskUserQuestion` form currently up in the PTY,
    /// maintained by the ask-hook listener. A `reply` injected while
    /// the form is up would just confirm the highlighted option — the reply
    /// path dismisses the form (attach+ESC) first so the user's actual text is
    /// what claude receives.
    pending_asks: super::PendingAsks,
    /// Tool-permission `PreToolUse` hooks currently parked in the ask-hook
    /// listener, long-polling for a human's decision. Keyed by the
    /// session's stable `local_id`. The `PermissionResponse` handler resolves
    /// the matching entry — handing the decision straight back to the blocked
    /// hook (which returns an `allow`/`deny` decision to Claude Code) — instead
    /// of attaching + injecting `1\r`/ESC keystrokes. The keystroke path is kept
    /// only as the fallback for when no hook is registered (hook timed out, or
    /// a prompt that surfaced via the legacy `tempo:"blocked"` signal).
    pending_perm_hooks: super::PendingPermHooks,
    /// When the last periodic divergence check ran. At idle
    /// it's a no-op; it re-sends a bounded window only when a session's offset
    /// has run ahead of the server's mark (see [`Driver::reconcile_tail`]).
    last_reconcile: Instant,
    /// Set when the control socket vanished (roster flushed) so the next
    /// successful poll triggers an immediate reconciliation re-tail rather
    /// than waiting for the periodic cycle.
    churned: bool,
    /// Best-known server transcript high-water mark per `offset_key`.
    /// Seeded by the server's `ResumeMarks` on connect and advanced as the
    /// forward tail emits, so the periodic pass only re-sends on real
    /// divergence (local offset ahead of what the server holds).
    server_marks: HashMap<String, u64>,
    /// Spawn-time `--model`/`--effort` remembered per worker `short`.
    /// Used as a fallback for the Status event when `state.json` isn't on disk
    /// yet (freshly spawned) or transiently absent (`/clear` rotation), so the
    /// session list still shows the model/effort we launched the worker with.
    /// `Mutex` because `spawn` takes `&self` while the poll loop holds `&mut self`.
    spawn_model_effort: std::sync::Mutex<HashMap<String, SpawnModelEffort>>,
    /// The turn id of the reply most recently injected into each session, with
    /// the instant it was injected. Claude re-encodes one injected turn as
    /// several transcript lines, so every user event inside
    /// [`TURN_ID_WINDOW`] takes the same id; past the window a turn typed
    /// straight into the native TUI would otherwise inherit it.
    /// `Mutex` because the reply path takes `&self` while the poll loop holds
    /// `&mut self`.
    pending_turns: std::sync::Mutex<HashMap<String, PendingTurn>>,
    /// Parent session id remembered per freshly-forked child `short`.
    /// `fork` dispatches a new worker but the `SessionStarted` for it is emitted
    /// later by the poll loop when the short first appears in the roster — that
    /// path has no idea it was a fork, so we stash the parent here and the
    /// roster-discovery emit reads it to set `SessionMeta::parent_local_id` (the
    /// link the server resolves into `parent_id`). `Mutex` for the same reason as
    /// `spawn_model_effort`.
    fork_parent_by_short: std::sync::Mutex<HashMap<String, (String, &'static str)>>,
    /// Authenticated server client + machine key for the launch-time gateway-env
    /// pull. Every worker (re)launch resolves the session's account
    /// env here from the server's durable `sessions.account_id` binding, so
    /// routing survives a daemon / claude-daemon restart and session-id rotation
    /// instead of depending on env carried by the triggering command. `None` in
    /// tests / when no server is configured — the chokepoint then falls back to
    /// the pushed env hint.
    server: Option<crate::client::ServerClient>,
    machine_key: Option<String>,
    /// Turn-complete watcher for the one session `maybe_dispatch_on_start`
    /// launched: writes `<jobs_root>/<short>/dispatch_done` once
    /// that session has been busy and then settles idle, so the worker
    /// entrypoint can wind the pod down instead of idling to the Job
    /// deadline. `None` on normal (non-dispatched) daemons — interactive
    /// sessions never get a marker. `Mutex` because it's armed from
    /// `maybe_dispatch_on_start` (`&self`).
    dispatch_done: std::sync::Mutex<Option<DispatchDoneTracker>>,
    /// Last ask/permission/plan hook delivery per `local_id`, maintained by
    /// the ask-hook listener. Read by the diagnose aggregation.
    hook_log: super::HookLog,
    /// When each short was last seen in a `list` snapshot — the observation
    /// timestamp behind the diagnose report's effective-state fact.
    last_status_at: HashMap<String, std::time::SystemTime>,
    /// Kind + time of the last event parsed out of each transcript tail,
    /// keyed by `offset_key`.
    last_parsed: HashMap<String, (String, std::time::SystemTime)>,
    /// Permission posture (`default`/`auto`/`yolo`/`whip`) recorded per
    /// worker `short` at spawn/fork time. `Mutex` for the same
    /// reason as `spawn_model_effort`.
    spawn_permission_mode: std::sync::Mutex<HashMap<String, String>>,
    /// Read-only live-view PTY relay. Opens a fresh viewer attach per
    /// watched session while a browser has its terminal open, forwarding
    /// coalesced PTY bytes as `PtyChunk` events. Interior-mutable so the `&self`
    /// command path can start/stop viewers.
    pty_view: super::pty_view::PtyViewManager,
    last_reseed: Option<Instant>,
    /// Memory-pressure sleep policy (see [`crate::mempressure`]); `None` when
    /// disabled by the environment.
    sleep_policy: Option<crate::mempressure::Policy>,
    /// No memory-pressure decision before this instant: set after a worker is
    /// put to sleep so the freed memory shows before the next one.
    next_sleep_check: Option<Instant>,
}

#[derive(Debug, Clone)]
struct PendingPerm {
    /// Synthesized id echoed back via `AdapterCommand::PermissionResponse`.
    request_id: String,
    /// Stable session `local_id` the request was emitted under.
    local_id: String,
    /// The raw `needs` string this request was emitted for. A change means a
    /// new prompt (the previous one resolved), so we re-emit.
    needs: String,
}

/// How many consecutive idle polls end a subagent whose transcript tail reads
/// finished. Lifecycle end never arrives over the control socket (subagents
/// aren't `list` jobs), so the tail is all we have; ~30s at the 2s poll.
///
/// Quiescence ALONE is not completion. A subagent waiting on the model
/// (extended thinking after a `tool_result`) or on a slow tool writes nothing
/// for tens of seconds while very much alive, and ending it there marked live
/// subagents `completed` about 30s after their last tool. Because
/// `ended_subagents` blocks re-discovery, every later line of a subagent
/// killed that way was dropped on the floor. The tail verdict
/// (`Driver::tail_verdict`) and the parent's `tool_result`
/// (`SubagentState::parent_resolved`) are what actually decide.
const SUBAGENT_IDLE_TICKS_TO_END: u32 = 15;

/// Backstop for a subagent no other signal can close: no `toolUseId` in its
/// sidecar (nothing to correlate in the parent transcript), a tail that reads
/// mid-turn, and a parent that keeps running. 30min at the 2s default poll.
const SUBAGENT_STUCK_TICKS_TO_END: u32 = 900;

#[derive(Debug, Clone)]
struct TranscriptLocation {
    path: PathBuf,
    local_id: String,
    /// Working directory of the parent session — reused as the subagents'
    /// `working_dir` and to locate their `subagents/` transcript dir.
    cwd: String,
    /// Key into the offset store. Stable across daemon restarts.
    offset_key: String,
}

#[derive(Debug, Clone)]
struct SubagentState {
    /// The parent session id (== parent's `local_id` / DB row id).
    parent_local_id: String,
    /// Consecutive polls during which the transcript did not grow.
    idle_ticks: u32,
    /// The parent `Task` call this subagent answers (`toolUseId` in the
    /// `.meta.json` sidecar). The parent's `tool_result` for it is the one
    /// authoritative end-of-life signal we get.
    tool_use_id: Option<String>,
    /// That `tool_result` has been seen in the parent transcript: the Task
    /// tool returned, so the subagent is over whatever its own tail says.
    parent_resolved: bool,
    /// The last tail batch left the turn open (a tool call in flight, a
    /// `tool_result` the model still owes an answer to, a thinking block).
    /// Such a subagent is working, however long its transcript stays quiet.
    /// True on discovery: a subagent that just appeared is running.
    tail_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusSnapshot {
    tempo: Option<String>,
    state: Option<String>,
    detail: Option<String>,
    name: Option<String>,
    activity: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// Everything the launch chokepoint pulls from the server's durable binding for
/// a (re)launch: the gateway-routing `env`, the per-account `settings_json`
/// the daemon merges under its managed hook settings, and the user's
/// `whipStopPhrases` override.
#[derive(Debug, Default)]
pub(super) struct LaunchEnv {
    pub env: std::collections::BTreeMap<String, String>,
    pub settings: Option<serde_json::Value>,
    pub whip_phrases: Option<serde_json::Value>,
    /// Present only when the server says this session may spawn subagents; it
    /// gates whether the `CctuiAgent` MCP server is registered at all.
    pub spawn_capability: Option<cctui_proto::api::SpawnCapability>,
}

/// Resolve a launch env into a full [`LaunchEnv`] from the server pull, shared
/// by the control/headless/oneshot drivers. Fail-closed refusal (account-bound
/// but missing/partial gateway env) surfaces as `Err`; a pull failure or absent
/// server degrades to `hint`.
pub(super) async fn resolve_launch_env_for(
    server: Option<&crate::client::ServerClient>,
    machine_key: Option<&String>,
    local_id: &str,
    hint: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<LaunchEnv> {
    let (Some(server), Some(mk)) = (server, machine_key) else {
        return Ok(LaunchEnv { env: hint.clone(), ..Default::default() });
    };
    match server.gateway_env(mk, local_id).await {
        Ok(resp) => Ok(LaunchEnv {
            env: crate::adapters::gateway_env::launch_env_decision(
                "claude",
                local_id,
                &resp,
                hint,
                crate::adapters::gateway_env::CLAUDE_GATEWAY_KEYS,
            )?,
            settings: resp.settings,
            whip_phrases: resp.whip_phrases,
            spawn_capability: resp.spawn_capability,
        }),
        Err(e) => {
            tracing::warn!(%local_id, "gateway-env pull failed; falling back to pushed env: {e}");
            Ok(LaunchEnv { env: hint.clone(), ..Default::default() })
        }
    }
}

/// Parse `CCTUI_GATEWAY_RESEED_SECS` (positive integer seconds) or fall back to
/// one hour — comfortably under the server's default 12h token TTL.
fn reseed_interval_from(var: Option<String>) -> Duration {
    var.and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(Duration::from_hours(1), Duration::from_secs)
}

/// Whether the gateway-env re-seed pass should run this poll: always on a
/// (re)attach, otherwise once `interval` has elapsed since the last pass (and
/// unconditionally on the very first pass).
fn reseed_due(last: Option<Instant>, interval: Duration, reattached: bool) -> bool {
    reattached || last.is_none_or(|t| t.elapsed() >= interval)
}

impl Driver {
    pub fn new(
        cfg: DriverConfig,
        events: mpsc::Sender<AdapterEvent>,
        commands: mpsc::Receiver<AdapterCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        // Offsets are kept in-memory only in production: the
        // transcript-tail offset could otherwise advance + persist past
        // events that hadn't yet shipped over the WS, losing them on a
        // disconnect. With server-side idempotency on `stream_events`
        // we can safely re-tail from 0 on every adapter (re)start.
        // Tests still pass an explicit path so they can verify the file
        // I/O path itself.
        let offsets = cfg
            .offsets_path
            .clone()
            .map_or_else(|| OffsetStore::open(None), |p| OffsetStore::open(Some(p)));
        let kickstarter = Kickstarter::new(cfg.claude_bin.clone());
        let version_gate = super::version_gate::VersionGate::new(cfg.claude_bin.clone());
        let attach = super::attach::AttachManager::new(cfg.discovery.clone(), shutdown.clone());
        let pty_view = super::pty_view::PtyViewManager::new(
            events.clone(),
            cfg.discovery.clone(),
            shutdown.clone(),
        );
        Self {
            cfg,
            events,
            commands,
            shutdown,
            roster: HashSet::new(),
            last_status: HashMap::new(),
            dead_shorts: HashSet::new(),
            session_to_local: Arc::new(Mutex::new(HashMap::new())),
            offsets,
            transcript_locations: HashMap::new(),
            short_by_session: HashMap::new(),
            subagents: HashMap::new(),
            ended_subagents: HashSet::new(),
            kickstarter,
            version_gate,
            attach,
            pending_perms: HashMap::new(),
            perm_seq: 0,
            pending_asks: super::PendingAsks::default(),
            pending_perm_hooks: super::PendingPermHooks::default(),
            last_reconcile: Instant::now(),
            churned: false,
            server_marks: HashMap::new(),
            spawn_model_effort: std::sync::Mutex::new(HashMap::new()),
            pending_turns: std::sync::Mutex::new(HashMap::new()),
            fork_parent_by_short: std::sync::Mutex::new(HashMap::new()),
            server: None,
            machine_key: None,
            dispatch_done: std::sync::Mutex::new(None),
            hook_log: super::HookLog::default(),
            last_status_at: HashMap::new(),
            last_parsed: HashMap::new(),
            spawn_permission_mode: std::sync::Mutex::new(HashMap::new()),
            pty_view,
            last_reseed: None,
            sleep_policy: crate::mempressure::Policy::from_env(),
            next_sleep_check: None,
        }
    }

    /// Attach the authenticated server client + machine key used by the
    /// launch-time gateway-env pull. Builder-style so the test
    /// constructor and any future caller can omit it.
    #[must_use]
    pub fn with_server(
        mut self,
        server: Option<crate::client::ServerClient>,
        machine_key: Option<String>,
    ) -> Self {
        self.server = server;
        self.machine_key = machine_key;
        self
    }

    /// How often the periodic reconciliation re-tail runs. Chosen
    /// in the 30–60s band: frequent enough that a dropped-send gap self-heals
    /// quickly, infrequent enough that the re-emitted (then deduped) volume is
    /// negligible next to the regular poll tail.
    const RECONCILE_INTERVAL: Duration = Duration::from_secs(45);

    /// Must stay well under the server's session-token TTL (default 12h) so a
    /// live worker's token is re-minted before it can expire.
    fn reseed_interval() -> Duration {
        reseed_interval_from(std::env::var("CCTUI_GATEWAY_RESEED_SECS").ok())
    }

    /// Clone handle to the shared `session_id → local_id` map, for the
    /// ask-hook listener to translate live `session_id`s.
    pub fn session_map(&self) -> SessionMap {
        self.session_to_local.clone()
    }

    /// Clone handle to the shared pending-ask set, for the ask-hook listener
    /// to maintain.
    pub fn pending_asks(&self) -> super::PendingAsks {
        self.pending_asks.clone()
    }

    /// Clone handle to the shared pending tool-permission hook map, for the
    /// ask-hook listener to register blocked `PreToolUse` hooks into.
    pub fn pending_perm_hooks(&self) -> super::PendingPermHooks {
        self.pending_perm_hooks.clone()
    }

    /// Clone handle to the shared hook-delivery log, for the ask-hook
    /// listener to maintain.
    pub fn hook_log(&self) -> super::HookLog {
        self.hook_log.clone()
    }

    #[allow(clippy::cognitive_complexity)]
    pub async fn run(mut self) -> anyhow::Result<()> {
        if !self.cfg.skip_backfill {
            self.run_backfill().await;
        }
        // Dispatched-worker bring-up: if this daemon was launched as a
        // dispatched kube/docker worker, self-start its session before entering
        // the poll loop. Best-effort — never aborts `run`.
        self.maybe_dispatch_on_start().await;
        let mut tick = tokio::time::interval(self.cfg.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    self.flush_before_teardown().await;
                    return Ok(());
                }
                _ = tick.tick() => {
                    if let Err(err) = self.poll_once().await {
                        tracing::debug!(%err, "claude daemon poll failed (will retry)");
                    }
                    // Periodic reconciliation re-tail: catch up any
                    // transcript gap the forward-only tail left behind. Driven
                    // off the poll tick (rather than a second timer) so it
                    // can't race apply_snapshot's tail/offset updates.
                    if self.last_reconcile.elapsed() >= Self::RECONCILE_INTERVAL {
                        self.reconcile_tail(false).await;
                        self.maybe_cycle_stale_daemon().await;
                    }
                }
                Some(cmd) = self.commands.recv() => {
                    if let AdapterCommand::ResumeMarks { marks } = cmd {
                        // Clamping cursors needs &mut self, so it can't ride the
                        // &self handle_command dispatch below.
                        self.apply_resume_marks(marks).await;
                    } else {
                    // Capture the correlation id before `cmd` is moved so we can
                    // report the outcome back to the originating client.
                    let command_id = cmd.command_id();
                    match self.handle_command(cmd).await {
                        Ok(Some(dispatch)) => {
                            dispatch.run_detached(self.events.clone(), command_id);
                        }
                        Ok(None) => {
                            Self::report_command(&self.events, command_id, Ok(())).await;
                        }
                        Err(err) => {
                            tracing::warn!(%err, "command dispatch failed");
                            Self::report_command(&self.events, command_id, Err(err)).await;
                        }
                    }
                    }
                }
            }
        }
    }

    /// Shorts the claude daemon currently lists as live — the jobs backfill
    /// must NOT touch (see `backfill::run_once`). No live socket
    /// (true cold start, claude daemon down) means nothing is live and the
    /// pass may sweep everything, as before.
    async fn live_shorts(&self) -> std::collections::HashSet<String> {
        let Some(sock) = self.cfg.discovery.locate_live().await else {
            return std::collections::HashSet::new();
        };
        match socket::call::<ListResponse>(&sock, &json!({"proto": 1, "op": "list"})).await {
            Ok(resp) => resp.jobs.into_iter().map(|j| j.short).collect(),
            Err(err) => {
                tracing::debug!(%err, "backfill live-roster list failed; treating none live");
                std::collections::HashSet::new()
            }
        }
    }

    // Linear setup (config → cursor → live roster → one pass) plus outcome
    // logging; no nesting to split.
    #[allow(clippy::cognitive_complexity)]
    async fn run_backfill(&mut self) {
        let cfg = BackfillConfig {
            jobs_root: self.cfg.jobs_root.clone(),
            projects_root: self.cfg.projects_root.clone(),
            cursor_path: self.cfg.backfill_cursor_path.clone().or_else(default_cursor_path),
        };
        let mut cursor = cfg
            .cursor_path
            .clone()
            .map_or_else(CursorFile::open_default, |p| CursorFile::open(Some(p)));
        let live_shorts = self.live_shorts().await;
        match backfill::run_once(&cfg, &live_shorts, &self.events, &mut cursor, &mut self.offsets)
            .await
        {
            Ok(n) if n > 0 => {
                tracing::info!(backfilled = n, "claude-code backfill pass complete");
                self.offsets.flush();
            }
            Ok(_) => tracing::debug!("no historical sessions to backfill"),
            Err(err) => tracing::warn!(%err, "backfill pass failed"),
        }
    }

    /// Deliver a user message to a worker, handling a pending `AskUserQuestion`
    /// form. With structured `ask_picks` and the hook-captured questions we
    /// answer the form *natively* — keystrokes on the real form, so claude
    /// records a genuine `tool_result` with the selected labels.
    /// Otherwise (free-text answer, missing questions, keystroke failure) fall
    /// back to dismiss-then-reply: attach+ESC the form away, then `reply` the
    /// text (claude records the ask as declined and reads the text as a new
    /// user turn).
    // Sequential reply-delivery pipeline (resolve short → resume-on-reply →
    // ask-form vs text → submit) whose complexity is linear `?`-propagating I/O
    // steps plus hibernation/ENOJOB recovery branches, not nesting. Splitting risks
    // the resume/recovery control flow; kept whole deliberately.
    #[allow(clippy::cognitive_complexity)]
    async fn deliver_reply(
        &self,
        sock: &std::path::Path,
        local_id: &str,
        text: &str,
        ask_picks: Option<Vec<Vec<usize>>>,
        env: &std::collections::BTreeMap<String, String>,
        turn_id: Option<uuid::Uuid>,
    ) -> anyhow::Result<()> {
        // Recorded before the op so the transcript tail cannot observe the
        // injected turn ahead of its id. A reply with no id clears any previous
        // one rather than letting it leak onto an unrelated turn.
        self.note_turn(local_id, turn_id);
        // Hibernated sessions (worker exited, job state still on disk)
        // have left `short_by_session`, so fall back to deriving the
        // short from the session id — same as the removal path. The derived
        // short is a pure function of the session id, so it "resolves" on a
        // machine that has never seen the session; without the on-disk job
        // check a misrouted reply would cold-resume a duplicate worker for
        // another daemon's session.
        let short = if let Ok(short) = self.resolve_short(local_id) {
            short
        } else {
            let short = self.resolve_short_for_removal(local_id)?;
            anyhow::ensure!(
                self.cfg.jobs_root.join(&short).is_dir(),
                "session {local_id} is not on this machine",
            );
            short
        };
        // Resume-on-reply: a reply to an exited worker is
        // ENOJOB'd by the claude daemon and silently lost. Revive it
        // first via a resume `dispatch`, then deliver as normal. Live
        // workers take the existing path with zero extra ops.
        self.resume_if_hibernated(sock, &short, local_id, env).await?;
        // If an AskUserQuestion form is up in the worker's PTY, a bare
        // `reply` just presses Enter on the highlighted option — claude
        // records option 1 ("Proceed"-style) and the user's text is
        // swallowed.
        let pending_ask = self.pending_asks.lock().ok().and_then(|mut m| m.remove(local_id));
        let had_pending_ask = pending_ask.is_some();
        if let Some(questions) = pending_ask {
            // Native answer first: drive the real form via keystrokes.
            let native_picks = ask_picks
                .as_ref()
                .and_then(|picks| questions.as_ref().and_then(|q| ask_keystrokes(q, picks)));
            if let Some(chunks) = native_picks {
                match socket::attach_answer_keys(sock, &short, &chunks).await {
                    Ok(()) => {
                        tracing::info!(%short, "answered ask form natively via keystrokes");
                        // PostToolUse fires for the real answer and emits
                        // `resolved`, but synthesize one too so the live card
                        // drops immediately (it's idempotent client-side).
                        let _ = self
                            .events
                            .send(AdapterEvent::AskResolved { local_id: local_id.to_owned() })
                            .await;
                        // A plan prompt is stored in the same pending map; emit
                        // PlanResolved too so a live Plan card drops. Idempotent
                        // (clients only clear their own kind).
                        let _ = self
                            .events
                            .send(AdapterEvent::PlanResolved { local_id: local_id.to_owned() })
                            .await;
                        return Ok(());
                    }
                    Err(err) => {
                        // do NOT fall through to attach+ESC here. The
                        // pending-ask record can be stale (the form already
                        // resolved in the native TUI or timed out, and the
                        // `resolved` hook hasn't reached us yet), in which case
                        // an ESC lands on whatever is now on screen — typically a
                        // running tool — and aborts the turn. That is exactly the
                        // "answering interrupted the tool" symptom. A stray text
                        // reply is harmless by comparison, so just deliver it.
                        tracing::warn!(%err, %short, "native ask answer failed; delivering text reply without ESC");
                    }
                }
            } else if ask_picks.is_none() {
                // Genuine free-text answer: the user typed prose rather than
                // picking options, so the form must be dismissed before the text
                // lands or claude records option 1 + swallows the text.
                // This is the only path that intentionally dismisses the form.
                if let Err(err) = socket::attach_interrupt(sock, &short).await {
                    tracing::warn!(%err, %short, "failed to dismiss pending ask form");
                } else {
                    tracing::info!(%short, "dismissed pending ask form before free-text reply");
                    // PostToolUse never fires for a cancelled ask, so synthesize
                    // `resolved` so the server/clients drop the live card.
                    let _ = self
                        .events
                        .send(AdapterEvent::AskResolved { local_id: local_id.to_owned() })
                        .await;
                    let _ = self
                        .events
                        .send(AdapterEvent::PlanResolved { local_id: local_id.to_owned() })
                        .await;
                    // Give the TUI a beat to settle after the ESC before the
                    // reply lands.
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
            }
            // Fallback paths (native answer failed, or option picks with an
            // unanswerable shape) deliver the text reply without dismissing the
            // form. The user has still answered, so tell clients to drop the live
            // card; idempotent if a real `resolved` hook follows, and a harmless
            // repeat of the free-text branch's own emit above.
            let _ =
                self.events.send(AdapterEvent::AskResolved { local_id: local_id.to_owned() }).await;
            let _ = self
                .events
                .send(AdapterEvent::PlanResolved { local_id: local_id.to_owned() })
                .await;
        }
        // The `reply` op types into the live composer, which is not empty after
        // an ESC ESC draft restore. Only an idle worker with no form up is safe
        // to clear (mid-turn the composer is a queue, and a form eats the keys).
        if !had_pending_ask && !self.is_busy(&short) {
            match socket::attach_clear_composer(sock, &short).await {
                Ok(()) => tracing::debug!(%short, "cleared composer before reply"),
                Err(err) => tracing::warn!(%err, %short, "failed to clear composer before reply"),
            }
        }
        // Baseline before the reply op: a build that auto-submits multiline
        // replies grows the transcript immediately, and a later baseline would
        // hide that submit from the confirm loop.
        let confirm = self.submit_confirm(&short, local_id);
        let resp =
            socket::one_shot(sock, &json!({"proto":1,"op":"reply","short":short,"text":text}))
                .await?;
        tracing::debug!(?resp, %short, "reply ack");
        if text.contains('\n')
            && let Err(err) = socket::attach_submit(sock, &short, &confirm).await
        {
            tracing::warn!(%err, %short, "failed to submit multiline reply draft");
        }
        Ok(())
    }

    /// Pick the submit-confirmation signal for a worker: transcript growth
    /// when idle (the only signal image ingestion can't fake), repaint when
    /// mid-turn (a submit only queues the message, so the transcript won't
    /// grow) or when no transcript can be located.
    fn submit_confirm(&self, short: &str, session_id: &str) -> socket::SubmitConfirm {
        if self.is_busy(short) {
            return socket::SubmitConfirm::Repaint;
        }
        let path = self.transcript_locations.get(short).map(|loc| loc.path.clone()).or_else(|| {
            transcript::newest_transcript_for_session(&self.cfg.projects_root, session_id)
        });
        path.map_or(socket::SubmitConfirm::Repaint, |path| {
            let baseline = std::fs::metadata(&path).map_or(0, |m| m.len());
            socket::SubmitConfirm::Transcript { path, baseline }
        })
    }

    /// Whether the worker's last status reported a non-idle tempo (mid-turn).
    fn is_busy(&self, short: &str) -> bool {
        self.last_status
            .get(short)
            .and_then(|s| s.tempo.as_deref())
            .is_some_and(|tempo| tempo != "idle")
    }

    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    /// Handles `cmd` inline, except for the control-socket dispatch of a
    /// spawn/fork: that reply can take as long as a cold worker bring-up (or
    /// never come), so it is returned as a [`DeferredDispatch`] for the run
    /// loop to await off the poll path.
    async fn handle_command(
        &self,
        cmd: AdapterCommand,
    ) -> anyhow::Result<Option<DeferredDispatch>> {
        // Diagnose is read-only aggregation and must answer even
        // when the claude daemon is down (the report *says* the socket is
        // gone), so it is handled before the socket requirement below.
        if let AdapterCommand::Diagnose { local_id, request_id } = cmd {
            return self.handle_diagnose(&local_id, request_id).await.map(|()| None);
        }
        // Live-view watch toggle only needs the in-memory short map, and
        // stopping a viewer must work even when the claude daemon has gone away —
        // so it is handled before the socket requirement below.
        if let AdapterCommand::WatchPty { local_id, watch } = cmd {
            match self.resolve_short(&local_id) {
                Ok(short) if watch => self.pty_view.watch(local_id, short),
                Ok(short) => self.pty_view.unwatch(&short),
                Err(err) => tracing::debug!(%err, watch, "watch_pty for unknown session; ignoring"),
            }
            return Ok(None);
        }
        // A command (spawn/reply/kill/…) needs a live control socket. If the
        // on-demand claude daemon has shut down, boot it and wait briefly for
        // the socket rather than failing the command outright.
        let sock = self.ensure_socket().await?;
        match cmd {
            AdapterCommand::SendMessage { local_id, text } => {
                self.deliver_reply(
                    &sock,
                    &local_id,
                    &text,
                    None,
                    &std::collections::BTreeMap::default(),
                    None,
                )
                .await?;
            }
            AdapterCommand::Reply { local_id, text, ask_picks, env, turn_id, .. } => {
                self.deliver_reply(&sock, &local_id, &text, ask_picks, &env, turn_id).await?;
            }
            AdapterCommand::Kill { local_id, signal } => {
                let short = self.resolve_short(&local_id)?;
                let mut req = json!({"proto":1,"op":"kill","short":short});
                if let Some(s) = signal {
                    // Claude's control-socket `kill` op validates `signal`
                    // against the string enum ["SIGTERM","SIGKILL"] (zod). A
                    // numeric signal (e.g. the interrupt route's `15`) fails
                    // that validation and the whole op is rejected silently. The
                    // control socket exposes no in-place turn-interrupt op, so
                    // the best we can do for a headless worker is terminate it;
                    // map to the enum name the daemon accepts.
                    req["signal"] = serde_json::Value::String(kill_signal_name(s).to_owned());
                }
                let resp = socket::one_shot(&sock, &req).await?;
                tracing::debug!(?resp, %short, "kill ack");
            }
            AdapterCommand::Interrupt { local_id, .. } => {
                // Keep-alive turn interrupt: the control socket has
                // no turn-interrupt op, so attach to the worker PTY and inject
                // an ESC keystroke — the same key that aborts a turn in the
                // TUI. Unlike `Kill`, the worker stays live and resumable.
                let short = self.resolve_short(&local_id)?;
                socket::attach_interrupt(&sock, &short).await?;
                tracing::info!(%short, "interrupted in-flight turn via attach+ESC");
            }
            AdapterCommand::Resume { local_id, working_dir, env } => {
                let short = self
                    .resolve_short(&local_id)
                    .or_else(|_| self.resolve_short_for_removal(&local_id))?;
                // Fall back to (local_id, working_dir) when the on-disk job state
                // is gone — archiving runs `claude rm`, which deletes state.json
                // but keeps the conversation transcript, so an explicit Resume of
                // an archived session must not depend on it.
                self.resume_worker(
                    &sock,
                    &short,
                    &local_id,
                    Some(&local_id),
                    working_dir.as_deref(),
                    &env,
                )
                .await?;
                tracing::info!(%short, %local_id, "resumed session via explicit command");
            }
            AdapterCommand::PermissionResponse { local_id, request_id, allow } => {
                // Preferred path: a bidirectional `PreToolUse` hook is
                // blocked in the listener long-polling for this decision. Hand
                // it the human's allow/deny straight back — the hook returns the
                // decision to Claude Code, so the tool runs/skips with no attach
                // and no keystroke at all. `take`n so a duplicate response can't
                // double-fire on an already-resolved (and dropped) channel.
                let hook =
                    self.pending_perm_hooks.lock().ok().and_then(|mut map| map.remove(&local_id));
                if let Some(tx) = hook {
                    if tx.send(allow).is_ok() {
                        tracing::info!(%local_id, %request_id, allow, "answered permission prompt via PreToolUse hook");
                        return Ok(None);
                    }
                    // The hook already gave up (its wait timed out and the
                    // receiver was dropped). Fall through to the keystroke path,
                    // which handles the now-rendered native prompt.
                    tracing::debug!(%local_id, %request_id, "perm hook receiver gone; falling back to keystroke");
                }
                // Fallback: no hook registered (timed out, or the
                // prompt surfaced only via the legacy `tempo:"blocked"`/`needs`
                // signal). The control socket's `permission-response` op is a
                // no-op stub, so answer the way a human does — attach to the PTY
                // and inject `1`+Enter (approve) or ESC (deny).
                let short = self.resolve_short(&local_id)?;
                socket::attach_permission_response(&sock, &short, allow).await?;
                tracing::info!(%short, %request_id, allow, "answered permission prompt via attach (fallback)");
            }
            AdapterCommand::Remove { local_id, .. } => {
                let short = self.resolve_short_for_removal(&local_id)?;
                // Imitate the agent-view Ctrl+X: there is no
                // control-socket removal op, so (1) stop the worker if it is
                // still live, (2) wait for it to actually exit, then (3) let
                // `claude rm` delete the on-disk job metadata + worktree. This
                // clears the session from Claude Code's own `claude agents`
                // view as well as our discovery; the transcript is preserved.
                let _ =
                    socket::one_shot(&sock, &json!({"proto":1,"op":"kill","short":short})).await;
                Self::await_worker_exit(&sock, &short).await;
                let rm = self.claude_rm(&short).await;
                crate::configsweep::remove_session_files(&short);
                rm?;
            }
            AdapterCommand::Spawn { spec, session_id, .. } => {
                let dispatch =
                    self.spawn(&sock, &spec, session_id.map(|id| id.to_string())).await?;
                return Ok(Some(dispatch));
            }
            AdapterCommand::Fork { parent_local_id, spec, session_id, extract, .. } => {
                let dispatch = self
                    .fork(&sock, &parent_local_id, &spec, session_id.as_deref(), extract.as_ref())
                    .await?;
                return Ok(Some(dispatch));
            }
            AdapterCommand::Rename { local_id, name } => {
                let short = self.resolve_short(&local_id)?;
                // No control-socket rename op exists; persist to the on-disk
                // state.json the status poll reads from. The next poll re-emits
                // Status with the new name (the server also updates its DB row
                // synchronously in the PATCH route).
                StateJson::write_name(&self.cfg.jobs_root, &short, &name)
                    .with_context(|| format!("rename session {short} -> {name}"))?;
                tracing::info!(%short, %name, "renamed session via state.json");
            }
            AdapterCommand::SetModel { local_id, .. } => {
                // In-place model/effort switch is not supported on claude via
                // cctui's path: the `claude daemon` control socket has
                // no set-model op, and the Agent SDK's `setModel()` is only
                // reachable in streaming-input mode, not through this socket.
                // The supported substitute is fork-with-`--model`, so
                // surface a clear error the webui can route to the fork flow.
                tracing::warn!(%local_id, "claude: in-place model/effort switch not supported; fork to change model");
                anyhow::bail!(
                    "in-place model/effort switch is not supported for claude sessions — fork to change model"
                );
            }
            _ => {
                // AdapterCommand is #[non_exhaustive]; tolerate unknown
                // future variants by logging.
                tracing::warn!("unhandled AdapterCommand variant");
            }
        }
        Ok(None)
    }

    async fn report_command(
        events: &mpsc::Sender<AdapterEvent>,
        command_id: Option<uuid::Uuid>,
        res: anyhow::Result<()>,
    ) {
        let Some(command_id) = command_id else { return };
        let (ok, error) = match res {
            Ok(()) => (true, None),
            Err(err) => (false, Some(err.to_string())),
        };
        let _ = events.send(AdapterEvent::CommandResult { command_id, ok, error }).await;
    }

    fn resolve_short(&self, local_id: &str) -> anyhow::Result<String> {
        self.short_by_session
            .get(local_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown session {local_id}"))
    }

    /// Resolve a short for removal, tolerating sessions that have already left
    /// the live roster. Removal most often targets *completed*
    /// sessions, but `short_by_session` only holds live ones — it's cleared
    /// when a session exits. Claude-code's short is the first group of the
    /// session UUID (state.json `daemonShort`), so fall back to deriving it.
    /// A wrong guess just makes `claude rm` a no-op (ENOJOB), so this stays
    /// best-effort rather than erroring.
    fn resolve_short_for_removal(&self, local_id: &str) -> anyhow::Result<String> {
        if let Some(short) = self.short_by_session.get(local_id) {
            return Ok(short.clone());
        }
        let candidate = local_id.split('-').next().unwrap_or(local_id);
        JobShort::parse(candidate)
            .map(|j| j.as_str().to_string())
            .ok_or_else(|| anyhow::anyhow!("cannot resolve short for {local_id}"))
    }

    /// Assemble the session-diagnose report: everything this driver
    /// already knows about `local_id`, each fact dated + sourced, and emit it
    /// back as an [`AdapterEvent::Diagnose`] echoing `request_id`.
    ///
    /// Fail-soft by construction: facts that cannot be produced right now
    /// come back `missing(reason)`; the only hard failure is the events
    /// channel being gone.
    // One fact per block — linear assembly, no nesting to split. The
    // effective-state `if let` stays a plain branch (not `map_or_else`): both
    // arms borrow `self` and the readable two-arm shape is the point.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines, clippy::option_if_let_else)]
    async fn handle_diagnose(&self, local_id: &str, request_id: uuid::Uuid) -> anyhow::Result<()> {
        let now_ms = now_unix_ms();
        let short =
            self.resolve_short(local_id).or_else(|_| self.resolve_short_for_removal(local_id)).ok();

        // Hook-side prompt state (shared with the ask-hook listener).
        let pending_ask = self.pending_asks.lock().is_ok_and(|m| m.contains_key(local_id));
        let parked_perm_hook =
            self.pending_perm_hooks.lock().is_ok_and(|m| m.contains_key(local_id));
        // Control-socket permission prompt, keyed by short.
        let pending_perm = short.as_deref().and_then(|s| self.pending_perms.get(s)).cloned();

        // Held-attach keep-alive + PTY-activity snapshot, reused by the
        // effective-state activity signal, the attach fact, and
        // the PTY-output fact below.
        let attach_snap = short.as_deref().and_then(|s| self.attach.snapshot(s));

        // Hook-event freshness feeds the PTY-vs-hook arbitration.
        let hook_age_ms = self
            .hook_log
            .lock()
            .ok()
            .and_then(|m| m.get(local_id).map(|(_, at)| to_unix_ms(*at)))
            .map(|at| now_ms - at);

        // Effective state + arbitration verdict.
        let effective_state = if let Some(short) = short.as_deref() {
            let snap = self.last_status.get(short);
            let (verdict, source) = arbitrate(&ArbitrationInput {
                pending_ask,
                parked_perm_hook,
                control_needs: pending_perm.as_ref().map(|p| p.needs.as_str()),
                reported_dead: self.dead_shorts.contains(short),
                in_roster: self.roster.contains(short),
                state_json_on_disk: StateJson::read(&self.cfg.jobs_root, short).is_some(),
                tempo: snap.and_then(|s| s.tempo.as_deref()),
                state: snap.and_then(|s| s.state.as_deref()),
            });
            // Second (PTY) signal: herdr-style arbitration of held-attach byte
            // flow against hook freshness. Surfaced on `activity`
            // when it carries a real verdict, never clobbering a status one.
            let pty_activity = {
                let av = arbitrate_activity(&ActivityInput {
                    hook_age_ms,
                    pty_last_output_age_ms: attach_snap
                        .as_ref()
                        .and_then(|s| s.last_output_at)
                        .map(|at| now_ms - to_unix_ms(at)),
                    pty_bytes_per_min: attach_snap
                        .as_ref()
                        .and_then(|s| s.bytes_per_min(SystemTime::now()))
                        .unwrap_or(0.0),
                    liveness_alive: attach_snap
                        .as_ref()
                        .and_then(|s| s.last_probe_alive)
                        .unwrap_or_else(|| self.roster.contains(short)),
                    idle_confirmations: attach_snap.as_ref().map_or(0, |s| s.idle_confirmations),
                });
                match av {
                    ActivityVerdict::Uncertain => None,
                    v => Some(v.as_str().to_owned()),
                }
            };
            let value = EffectiveState {
                verdict,
                tempo: snap.and_then(|s| s.tempo.clone()),
                state: snap.and_then(|s| s.state.clone()),
                detail: snap.and_then(|s| s.detail.clone()),
                activity: pty_activity.or_else(|| snap.and_then(|s| s.activity.clone())),
            };
            match self.last_status_at.get(short) {
                Some(at) => DiagnoseFact::observed(value, source.as_str(), to_unix_ms(*at), now_ms),
                None => DiagnoseFact::undated(value, source.as_str()),
            }
        } else {
            DiagnoseFact::missing("activity", "unknown session (no worker short resolvable)")
        };

        let last_hook_event =
            self.hook_log.lock().ok().and_then(|m| m.get(local_id).cloned()).map_or_else(
                || DiagnoseFact::missing("hook", "no hook delivery seen for this session"),
                |(kind, at)| {
                    DiagnoseFact::observed(HookEvent { kind }, "hook", to_unix_ms(at), now_ms)
                },
            );

        let attach = attach_snap.as_ref().map_or_else(
            || DiagnoseFact::missing("attach", "no keep-alive attach task for this session"),
            |snap| {
                let value = AttachStatus {
                    phase: snap.phase.clone(),
                    backoff_ms: snap
                        .backoff
                        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    last_probe_alive: snap.last_probe_alive,
                    last_probe_at_ms: snap.last_probe_at.map(to_unix_ms),
                };
                match snap.updated_at {
                    Some(at) => DiagnoseFact::observed(value, "attach", to_unix_ms(at), now_ms),
                    None => DiagnoseFact::undated(value, "attach"),
                }
            },
        );

        // PTY output age/throughput from the held-attach drain loop:
        // the raw activity signal state derivation weighs against hook freshness.
        let pty_output: DiagnoseFact<PtyOutputStats> = match attach_snap
            .as_ref()
            .filter(|s| s.last_output_at.is_some())
        {
            Some(snap) => {
                let last = snap.last_output_at.expect("filtered to Some");
                let value = PtyOutputStats {
                    last_output_age_ms: Some(now_ms - to_unix_ms(last)),
                    recent_bytes_per_min: snap.bytes_per_min(SystemTime::now()),
                };
                DiagnoseFact::observed(value, "pty", to_unix_ms(last), now_ms)
            }
            None => DiagnoseFact::missing("pty", "no PTY output observed on the held attach yet"),
        };

        // Live probe at report time: which socket discovery picks, and the
        // full candidate list. Bounded (per-candidate probe timeout), no
        // kickstart side effects.
        let candidates: Vec<String> = self
            .cfg
            .discovery
            .candidate_paths()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let live_sock = self.cfg.discovery.locate_live().await;
        let claude_socket = DiagnoseFact::fresh(
            SocketStatus {
                live: live_sock.is_some(),
                path: live_sock.map(|p| p.to_string_lossy().into_owned()),
                candidates,
            },
            "discovery",
            now_ms,
        );

        let transcript =
            short.as_deref().and_then(|s| self.transcript_locations.get(s)).map_or_else(
                || DiagnoseFact::missing("filesystem", "no transcript pinned for this session yet"),
                |loc| {
                    let meta = std::fs::metadata(&loc.path).ok();
                    let mtime = meta.as_ref().and_then(|m| m.modified().ok());
                    let parsed = self.last_parsed.get(&loc.offset_key);
                    let value = TranscriptStatus {
                        path: loc.path.to_string_lossy().into_owned(),
                        mtime_ms: mtime.map(to_unix_ms),
                        size_bytes: meta.as_ref().map(std::fs::Metadata::len),
                        tail_offset: self.offsets.get(&loc.offset_key),
                        last_parsed_event: parsed.map(|(kind, _)| kind.clone()),
                        last_parsed_at_ms: parsed.map(|(_, at)| to_unix_ms(*at)),
                    };
                    match mtime {
                        Some(at) => {
                            DiagnoseFact::observed(value, "filesystem", to_unix_ms(at), now_ms)
                        }
                        None => DiagnoseFact::undated(value, "filesystem"),
                    }
                },
            );

        let prompts = DiagnoseFact::fresh(
            PendingPrompts {
                pending_ask,
                parked_perm_hook,
                control_needs: pending_perm.as_ref().map(|p| p.needs.clone()),
                perm_request_id: pending_perm.map(|p| p.request_id),
            },
            "hook+control_socket",
            now_ms,
        );

        let permission_mode = {
            let recorded = short.as_deref().and_then(|s| {
                self.spawn_permission_mode.lock().ok().and_then(|m| m.get(s).cloned())
            });
            match recorded {
                Some(label) => DiagnoseFact::undated(label, "spawn"),
                // The spawn-time record dies with the daemon process; the whip
                // Stop hook in the managed settings file survives on disk.
                None if short.as_deref().is_some_and(detect_whip_from_settings) => {
                    DiagnoseFact::undated("whip".to_owned(), "settings-file")
                }
                None => DiagnoseFact::missing(
                    "spawn",
                    "not recorded (session predates this daemon process or was launched externally)",
                ),
            }
        };

        let dispatch = self
            .dispatch_done
            .lock()
            .ok()
            .and_then(|guard| {
                guard.as_ref().and_then(|t| {
                    (Some(t.short()) == short.as_deref()).then(|| DispatchStatus {
                        seen_busy: t.seen_busy(),
                        done: t.is_done(),
                        marker_path: t.marker_path().to_string_lossy().into_owned(),
                    })
                })
            })
            .map_or_else(
                || {
                    DiagnoseFact::missing(
                        "dispatch",
                        "not a dispatched session (no turn-complete watcher armed)",
                    )
                },
                |value| DiagnoseFact::fresh(value, "dispatch", now_ms),
            );

        let gateway = DiagnoseFact::fresh(
            GatewayStatus {
                server_configured: self.server.is_some() && self.machine_key.is_some(),
            },
            "daemon-config",
            now_ms,
        );

        let report = SessionDiagnose {
            local_id: local_id.to_owned(),
            short,
            generated_at_ms: now_ms,
            adapter: "claude-code".to_owned(),
            effective_state,
            last_hook_event,
            attach,
            pty_output,
            claude_socket,
            transcript,
            prompts,
            permission_mode,
            dispatch,
            gateway,
            codex: None,
        };
        self.events
            .send(AdapterEvent::Diagnose {
                local_id: local_id.to_owned(),
                request_id,
                report: Box::new(report),
            })
            .await
            .map_err(|_| anyhow::anyhow!("events channel closed while sending diagnose report"))
    }

    /// Poll the `has` op until the worker is no longer alive (or we give up).
    /// `claude rm` is documented to work on already-exited sessions; racing it
    /// against a still-live worker is undefined, so we drain the kill first.
    /// Best-effort: a socket error or timeout just falls through to `claude rm`.
    async fn await_worker_exit(sock: &std::path::Path, short: &str) {
        for _ in 0..20 {
            match socket::one_shot(sock, &json!({"proto":1,"op":"has","short":short})).await {
                Ok(resp) => {
                    let alive =
                        resp.get("alive").and_then(serde_json::Value::as_bool).unwrap_or(false);
                    if !alive {
                        return;
                    }
                }
                // Socket gone / op failed — nothing more to wait on.
                Err(_) => return,
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tracing::warn!(%short, "worker still live 2s after kill; proceeding to `claude rm`");
    }

    /// Resume-on-reply: if `short` has no live worker, revive it
    /// before a reply is delivered. The claude control socket cannot wake an
    /// exited job itself — `attach`/`reply` both return ENOJOB; the picker's
    /// "enter to resume" is client-side. What does work (probed against
    /// claude v2.1.162) is a `dispatch` that reuses the dead job's identity:
    /// same `short`, the `resumeSessionId` from its on-disk `state.json`, and
    /// `--resume <id>` as the launch argv — the daemon spawns a fresh worker
    /// bound to the saved conversation, original transcript re-pinned.
    ///
    /// No-op (one cheap `has` round-trip) when the worker is alive.
    async fn resume_if_hibernated(
        &self,
        sock: &std::path::Path,
        short: &str,
        local_id: &str,
        env: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        self.resume_worker(sock, short, local_id, None, None, env).await
    }

    /// The single source of gateway-routing env for every worker (re)launch:
    /// pull it from the server's durable `sessions.account_id` binding
    /// so routing survives a daemon / claude-daemon restart and session-id
    /// rotation, instead of trusting whatever env the triggering command carried.
    ///
    /// `hint` is that carried env (spawn `spec.env`, reply/resume push) — used as
    /// a fallback only when the authoritative pull is unavailable (older server,
    /// transient network) so a rollout or blip degrades to the prior push
    /// behavior rather than failing.
    ///
    /// Fail-closed: when the server reports the session IS account-bound but the
    /// resolved env is empty (account gone / unmintable), refuse the launch — a
    /// worker started without the gateway credential would silently route to the
    /// default upstream and 401. Returning `Err` aborts the dispatch loudly
    /// instead of producing another silent auth drop.
    async fn resolve_launch_env(
        &self,
        local_id: &str,
        hint: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<LaunchEnv> {
        match resolve_launch_env_for(
            self.server.as_ref(),
            self.machine_key.as_ref(),
            local_id,
            hint,
        )
        .await
        {
            Ok(launch) => Ok(launch),
            // Surface the fail-closed refusal as a visible failure state before
            // aborting, so the UI shows the account problem rather than the launch
            // silently dying.
            Err(e) => {
                self.emit(AdapterEvent::SessionEnded {
                    local_id: local_id.to_owned(),
                    reason: EndReason::Crashed {
                        detail: format!(
                            "account-bound session refused: the server returned no gateway \
                             credential (account missing/unmintable). The worker was NOT \
                             launched on ambient credentials — reconnect the account in cctui. \
                             ({e})"
                        ),
                    },
                })
                .await;
                Err(e)
            }
        }
    }

    /// Revive an exited worker bound to its saved conversation. Prefers the
    /// on-disk `state.json` (so `/clear`/`/compact`'s rotated `resumeSessionId`
    /// is honored); when it's gone — e.g. an archived session whose
    /// `claude rm` deleted the job metadata but left the transcript — falls back
    /// to the caller-supplied `(session_id, cwd)` from the server's DB row.
    /// No-op (one cheap `has` round-trip) when the worker is alive.
    async fn resume_worker(
        &self,
        sock: &std::path::Path,
        short: &str,
        local_id: &str,
        fallback_session_id: Option<&str>,
        fallback_cwd: Option<&str>,
        env: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let alive = |resp: &serde_json::Value| {
            resp.get("alive").and_then(serde_json::Value::as_bool).unwrap_or(false)
        };
        let has = socket::one_shot(sock, &json!({"proto":1,"op":"has","short":short})).await?;
        if alive(&has) {
            return Ok(());
        }

        // Re-derive the gateway env + per-account settings from the server's
        // durable binding, falling back to the pushed `env` hint if the pull is
        // unavailable — a cold-resume must never relaunch the worker with empty
        // env (it would 401). Fail-closed inside `resolve_launch_env`.
        let launch = self.resolve_launch_env(local_id, env).await?;
        let env = launch.env;

        // Re-apply the managed hook settings on cold resume so the revived worker
        // keeps its ask/permission/Stop hooks AND picks up the (possibly
        // refreshed) per-account settings the env pull re-served.
        // `whip` is recovered from the settings file the original spawn wrote for
        // this `short` (its `hooks.Stop` block is whip-only) — cold resume has no
        // `spec` to read it from directly, and defaulting false would silently
        // downgrade a 🐎 session's enforcement profile.
        let whip = detect_whip_from_settings(short);
        let st = StateJson::read(&self.cfg.jobs_root, short);
        // Carry the gateway env + the session's model/effort (from `state.json`)
        // into the managed `--settings` file so a spare-claimed resume
        // keeps its routing env and model/effort — the settings file survives the
        // spare-claim, the dispatch `env` and a `--model` CLI arg do not.
        let settings_arg = ensure_hook_settings(
            &self.cfg.hook_socket_path,
            whip,
            short,
            launch.settings.as_ref(),
            &env,
            st.as_ref().and_then(|s| s.model.as_deref()),
            st.as_ref().and_then(|s| s.effort.as_deref()),
            launch.whip_phrases.as_ref(),
        )
        .map(|p| p.to_string_lossy().into_owned());

        // `/clear`/`/compact` rotate the live conversation into the id recorded
        // in `resumeSessionId`; resuming the stale spawn id would fork the
        // conversation back at the pre-reset state. When state.json is
        // gone, fall back to the id/cwd the server passed from its DB row.
        let session_id = st
            .as_ref()
            .and_then(|s| s.resume_session_id.clone().or_else(|| s.session_id.clone()))
            .or_else(|| fallback_session_id.map(str::to_owned))
            .ok_or_else(|| {
                anyhow::anyhow!("no session id on disk or from caller to resume {short}")
            })?;
        let cwd = st
            .as_ref()
            .and_then(|s| s.cwd.clone())
            .or_else(|| fallback_cwd.map(str::to_owned))
            .ok_or_else(|| anyhow::anyhow!("no cwd on disk or from caller to resume {short}"))?;

        let agent = "claude";
        let nonce: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
        let created_at = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
        )
        .unwrap_or(0);
        // Launch argv + respawn flags, appending the managed `--settings` file so
        // the revived worker keeps its hooks + account settings.
        let mut args =
            vec!["--resume".to_owned(), session_id.clone(), "--agent".to_owned(), agent.to_owned()];
        let mut respawn_flags = vec!["--agent".to_owned(), agent.to_owned()];
        // NB: resume deliberately does NOT pass `--model`/`--effort`.
        // Asserting `--model` on a `--resume` forces the claude
        // daemon down its spare-claim/cold relaunch, which does NOT reapply
        // cctui's dispatch gateway env (background workers don't inherit gateway
        // vars) — so the revived worker came up with no `ANTHROPIC_BASE_URL`/
        // token and 401ed/ConnectionRefused. The resumed session already carries
        // its model/effort in the transcript; only `spawn` seeds them as flags.
        if let Some(settings) = &settings_arg {
            args.push("--settings".to_owned());
            args.push(settings.clone());
            respawn_flags.push("--settings".to_owned());
            respawn_flags.push(settings.clone());
        }
        let req = json!({
            "proto": 1,
            "op": "dispatch",
            "timeoutMs": 15000,
            "d": {
                "proto": 1,
                "short": short,
                "nonce": nonce,
                "sessionId": session_id,
                "createdAt": created_at,
                "source": "fleet",
                "cwd": cwd,
                "launch": { "mode": "prompt", "args": args },
                // Re-inject the gateway env resolved for this session's bound
                // OAuth account so the revived worker keeps routing through the
                // gateway rather than hitting the default upstream with no
                // credential and 401ing. Mirror into `reattachEnv` so
                // claude's own daemon reapplies it on any internal respawn
                // (`/clear`, `/compact`) while it's alive. Empty for sessions with
                // no account binding.
                "env": &env,
                "reattachEnv": &env,
                "isolation": "none",
                "respawnFlags": respawn_flags,
                "agent": agent,
                // `state.json` already exists for this short; the daemon keeps
                // its identity fields, so the seed is just protocol filler.
                "seed": { "intent": st.as_ref().and_then(|s| s.intent.clone()).unwrap_or_default() },
                "cols": 120,
                "rows": 40,
            }
        });
        let resp: serde_json::Value = socket::call(sock, &req)
            .await
            .with_context(|| format!("resume dispatch for hibernated session {short}"))?;
        tracing::info!(?resp, %short, %session_id, "resumed hibernated session via dispatch");

        // Wait (bounded) for the revived worker to report alive, then give the
        // PTY a moment to finish booting so the reply isn't swallowed by a
        // half-started claude. The next poll tick re-adds the short to the
        // roster and the AttachManager's persistent attach keeps it awake.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            if let Ok(resp) =
                socket::one_shot(sock, &json!({"proto":1,"op":"has","short":short})).await
                && alive(&resp)
            {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                return Ok(());
            }
        }
        anyhow::bail!("resumed session {short} did not come alive within 10s");
    }

    /// Run `claude rm <short>` to delete the job metadata + Claude-created
    /// worktree. A job the CLI no longer knows is already gone and counts as
    /// success; any other non-zero exit (typically a worktree with
    /// uncommitted changes) is a real failure the archive must report.
    async fn claude_rm(&self, short: &str) -> anyhow::Result<()> {
        let mut cmd = tokio::process::Command::new(&self.cfg.claude_bin);
        cmd.arg("rm")
            .arg(short)
            // `claude` lives in `~/.local/bin`, off launchd's minimal PATH
            // — give the child an augmented PATH so exec succeeds.
            .env("PATH", crate::childenv::child_path());
        crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
        let out = cmd
            .output()
            .await
            .with_context(|| format!("spawning `{} rm {short}`", self.cfg.claude_bin))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        match classify_claude_rm(out.status.code(), out.status.success(), &stdout, &stderr) {
            ClaudeRmOutcome::Removed => {
                tracing::info!(%short, "removed claude job via `claude rm`");
                Ok(())
            }
            ClaudeRmOutcome::AlreadyGone => {
                tracing::info!(%short, "claude job already gone; nothing to remove");
                Ok(())
            }
            ClaudeRmOutcome::Refused(detail) => {
                tracing::warn!(%short, %detail, "`claude rm` refused");
                anyhow::bail!("claude rm {short} failed: {detail}")
            }
        }
    }

    /// Dispatched-worker bring-up.
    ///
    /// A kube/docker worker pod is a peer machine whose daemon must *start* the
    /// dispatched session itself. The server pre-mints the session id + gateway
    /// token and tells the enrolled dispatcher to spawn the pod, but — unlike a
    /// desktop machine — it sends no WS `Spawn` command, because every
    /// dispatched pod registers under the single shared `dispatch` machine row
    /// and can't be addressed individually. So when the dispatcher-injected env
    /// (`SESSION_ID` + `TASK_PAYLOAD_JSON`) is present, we self-issue the exact
    /// control-socket `dispatch` a server-driven spawn would, reusing
    /// [`Self::spawn`] and forcing the pre-minted `session_id` so the
    /// gateway token resolves and the registered id matches the dispatch.
    ///
    /// Best-effort: any failure logs and lets the daemon keep observing — it
    /// never aborts `run`. A normal machine daemon lacks these env vars and is
    /// unaffected.
    // Linear startup-dispatch pipeline (read env → build spec → force session_id
    // → spawn) with best-effort bail-out logging at each step; complexity is the
    // env-validation branches, not nesting. Kept whole to preserve the dispatch flow.
    #[allow(clippy::cognitive_complexity)]
    async fn maybe_dispatch_on_start(&self) {
        let session_id = match std::env::var("SESSION_ID") {
            Ok(s) if !s.is_empty() => s,
            _ => return,
        };
        let payload_raw = match std::env::var("TASK_PAYLOAD_JSON") {
            Ok(s) if !s.is_empty() => s,
            _ => return,
        };
        let payload: serde_json::Value = match serde_json::from_str(&payload_raw) {
            Ok(v) => v,
            Err(err) => {
                tracing::error!(%err, "dispatch-on-start: TASK_PAYLOAD_JSON is not valid JSON");
                return;
            }
        };
        // Codex-native dispatch: a `adapter = "codex"` payload runs
        // headlessly via `codex exec`, NOT the claude control socket. This path
        // is separate from the interactive codex app-server adapter.
        let adapter = crate::dispatch_codex::payload_adapter(&payload);
        if crate::dispatch_codex::is_codex_adapter(&adapter) {
            match crate::dispatch_codex::CodexDispatch::from_payload(&payload) {
                Ok(dispatch) => {
                    tracing::info!(session_id = %session_id, "dispatch-on-start: launching codex dispatch");
                    tokio::spawn(async move {
                        if let Err(err) = dispatch.run().await {
                            tracing::error!(%err, "dispatch-on-start: codex dispatch failed");
                        }
                    });
                }
                Err(err) => {
                    tracing::error!(%err, "dispatch-on-start: could not build codex dispatch");
                }
            }
            return;
        }
        let spec = match Self::build_dispatch_spec(&payload) {
            Ok(spec) => spec,
            Err(err) => {
                tracing::error!(%err, "dispatch-on-start: could not build session spec");
                return;
            }
        };
        let sock = match self.ensure_socket().await {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(%err, "dispatch-on-start: claude daemon socket unavailable");
                return;
            }
        };
        tracing::info!(session_id = %session_id, "dispatch-on-start: launching dispatched session");
        let dispatched = match self.spawn(&sock, &spec, Some(session_id.clone())).await {
            Ok(dispatch) => dispatch.send().await,
            Err(err) => Err(err),
        };
        if let Err(err) = dispatched {
            tracing::error!(%err, session_id = %session_id, "dispatch-on-start: spawn failed");
        } else {
            tracing::info!(session_id = %session_id, "dispatch-on-start: session dispatched");
            // Arm the turn-complete watcher for this — and only
            // this — session, so the pod entrypoint gets a done-signal when
            // the session settles idle after its work.
            let settle = dispatch_done::settle_from_env(
                std::env::var("CCTUI_DISPATCH_DONE_SETTLE_SECS").ok().as_deref(),
            );
            if let Ok(mut guard) = self.dispatch_done.lock() {
                *guard = Some(DispatchDoneTracker::new(&session_id, &self.cfg.jobs_root, settle));
            }
        }
    }

    /// Build a [`SessionSpec`](cctui_proto::adapter::SessionSpec) from the
    /// dispatcher's `TASK_PAYLOAD_JSON` (`prompt_file`/`prompt`, `model`,
    /// `effort`, `repo`, `env`). Working dir is `CCTUI_DISPATCH_WORKDIR`
    /// (default `/workspace`). Dispatched workers run headless, so the
    /// permission posture is `Yolo` (bypass — the pod is already sandboxed by
    /// landlock + seccomp + the guard-proxy).
    fn build_dispatch_spec(
        payload: &serde_json::Value,
    ) -> anyhow::Result<cctui_proto::adapter::SessionSpec> {
        let prompt = Self::resolve_dispatch_prompt(payload)?;
        let workdir =
            std::env::var("CCTUI_DISPATCH_WORKDIR").unwrap_or_else(|_| "/workspace".to_owned());
        let env: std::collections::BTreeMap<String, String> = payload
            .get("env")
            .and_then(serde_json::Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let name = payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| std::env::var("TASK_NAME").ok().filter(|s| !s.is_empty()));
        Ok(cctui_proto::adapter::SessionSpec {
            adapter_id: cctui_proto::adapter::AdapterId("claude-code".to_owned()),
            working_dir: Some(workdir),
            prompt: Some(prompt),
            name,
            permission_mode: Some(cctui_proto::adapter::PermissionMode::Yolo),
            effort: payload
                .get("effort")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned),
            model: payload.get("model").and_then(serde_json::Value::as_str).map(ToOwned::to_owned),
            service_tier: None,
            env,
            bootstrap: serde_json::Value::Null,
            parent_local_id: None,
        })
    }

    /// Resolve the dispatched prompt: an inline `prompt`, else a `prompt_file`
    /// searched across `CCTUI_DISPATCH_PROMPT_DIRS` (default
    /// `/opt/context/prompts:/prompts`). An absolute `prompt_file` is read as-is.
    fn resolve_dispatch_prompt(payload: &serde_json::Value) -> anyhow::Result<String> {
        crate::dispatch_codex::resolve_dispatch_prompt(payload)
    }

    /// Spawn a fresh claude session via the `dispatch` op on the `claude
    /// daemon` control socket — the same primitive claude's own `FleetView`
    /// uses (`source:"fleet"`). The new session surfaces in the next `list`
    /// poll and goes through the normal observe path; there is no separate
    /// ACK beyond the dispatch reply.
    ///
    /// The payload shape is the daemon's private, proto-gated dispatch
    /// record. It is NOT `{op,cwd,prompt}` — that older guess was rejected
    /// outright (`malformed request`) and the rejection was swallowed,
    /// producing a silent no-op. We mint the session id / short /
    /// nonce client-side exactly as claude does and hand the worker its
    /// launch argv.
    #[allow(clippy::too_many_lines)]
    async fn spawn(
        &self,
        sock: &std::path::Path,
        spec: &cctui_proto::adapter::SessionSpec,
        forced_session_id: Option<String>,
    ) -> anyhow::Result<DeferredDispatch> {
        let cwd = spec
            .working_dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("spawn: working_dir required"))?;
        let cwd_path = std::path::Path::new(cwd);
        if !cwd_path.is_dir() {
            anyhow::bail!("spawn: working_dir does not exist or is not a directory: {cwd}");
        }

        let agent = "claude";
        // Use the server-pre-minted session id when supplied so the
        // id the server bound the gateway session token to matches the id the
        // worker registers as (otherwise `account_name` never resolves). Falls
        // back to a fresh uuid for non-account / non-HTTP spawns.
        let session_id = forced_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // `short` is the first uuid group (8 hex chars); `nonce` is 8 fresh
        // hex chars. Both satisfy the daemon's /^[a-f0-9]{8}$/ validator.
        let short = &session_id[..8];
        let nonce: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
        let created_at = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
        )
        .unwrap_or(0);

        // Worker launch argv, mirroring claude's own fleet dispatch:
        // `--session-id <id> --agent claude [--name <name>] [-- <prompt>]`.
        let mut args = vec![
            "--session-id".to_owned(),
            session_id.clone(),
            "--agent".to_owned(),
            agent.to_owned(),
        ];
        if let Some(name) = &spec.name {
            args.push("--name".to_owned());
            args.push(name.clone());
        }
        // Per-spawn permission posture. `None` inherits whatever
        // the claude daemon was launched with (the user's global default).
        if let Some(mode) = spec.permission_mode {
            args.push("--permission-mode".to_owned());
            args.push(mode.claude_flag().to_owned());
        }
        // Inject the managed `AskUserQuestion` hook settings, scoped
        // to this fleet-spawned worker only — the user's hand-run `claude` is
        // untouched. `--settings` merges over the resolved hierarchy, so it
        // only ADDS the hook. Goes into `respawnFlags` too so it survives the
        // `/clear`/`/compact` relaunch the claude daemon drives off them.
        let mut respawn_flags = vec!["--agent".to_owned(), agent.to_owned()];
        // NB: model/effort are NOT passed as `--model`/`--effort` CLI args.
        // They ride the managed `--settings` file below (`model` /
        // `effortLevel` / `CLAUDE_CODE_EFFORT_LEVEL`), which the claude daemon
        // applies to a spare-claimed worker — whereas a `--model` CLI arg forces
        // the spare-claim/cold relaunch that drops the dispatch gateway env.
        // Remember the spawn-time model/effort keyed by `short` so the
        // Status emit can fall back to it while `state.json` is still being
        // written (or transiently gone across a `/clear`).
        {
            let model = spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
            let effort = spec.effort.as_deref().map(str::trim).filter(|e| !e.is_empty());
            if (model.is_some() || effort.is_some())
                && let Ok(mut map) = self.spawn_model_effort.lock()
            {
                map.insert(short.to_owned(), (model.map(str::to_owned), effort.map(str::to_owned)));
            }
        }
        // Remember the launch posture for the diagnose report.
        if let Some(mode) = spec.permission_mode
            && let Ok(mut map) = self.spawn_permission_mode.lock()
        {
            map.insert(short.to_owned(), super::diagnose::permission_label(mode).to_owned());
        }
        // A `CctuiAgent` child links to its caller through the same stash the
        // fork path uses: roster discovery emits the `SessionStarted` and has no
        // other way to know the spawn had a parent. Relation "subagent", not
        // "fork" — the webui nests subagents but renders forks as siblings.
        if let Some(parent) = spec.parent_local_id.as_deref()
            && let Ok(mut map) = self.fork_parent_by_short.lock()
        {
            map.insert(short.to_owned(), (parent.to_owned(), "subagent"));
        }
        let whip = spec.permission_mode.is_some_and(cctui_proto::adapter::PermissionMode::is_whip);
        // Resolve the gateway env + per-account settings from the server's
        // durable binding BEFORE writing the managed hook-settings file, so the
        // account settings can be deep-merged under the managed hooks.
        // Fail-closed inside `resolve_launch_env` (account-bound but
        // unmintable → abort rather than launch a worker that will 401).
        let launch = self.resolve_launch_env(&session_id, &spec.env).await?;
        if let Some(settings) = ensure_hook_settings(
            &self.cfg.hook_socket_path,
            whip,
            short,
            launch.settings.as_ref(),
            &launch.env,
            spec.model.as_deref(),
            spec.effort.as_deref(),
            launch.whip_phrases.as_ref(),
        ) {
            let settings = settings.to_string_lossy().into_owned();
            args.push("--settings".to_owned());
            args.push(settings.clone());
            respawn_flags.push("--settings".to_owned());
            respawn_flags.push(settings);
        }
        let agent_tool =
            ensure_agent_mcp_config(short, &session_id, launch.spawn_capability.as_ref());
        if let Some(mcp) = &agent_tool {
            let mcp = mcp.to_string_lossy().into_owned();
            args.push("--mcp-config".to_owned());
            args.push(mcp.clone());
            respawn_flags.push("--mcp-config".to_owned());
            respawn_flags.push(mcp);
        }
        // Stage any uploaded files under /tmp/cctui-uploads/<session-id>/ and
        // prepend their absolute paths to the prompt so the worker reads them.
        // A staging failure is fatal to the spawn — silently dropping
        // an attachment the user expects the worker to read would be worse.
        let staged = stage_uploads(&session_id, &spec.bootstrap).inspect_err(|_| {
            crate::configsweep::remove_session_files(short);
        })?;
        // Prepend a delimited `<session-context>` block to the SPAWN prompt only:
        // give the agent the same at-a-glance context a human sees in
        // the UI — name, model·effort, permission posture, env var NAMES (never
        // values — those live only in `env_json` below), cwd, and the staged
        // file list (folded in here from the old client-side `Attached files:`
        // append). Subsequent messages are untouched.
        let session_context = build_session_context(
            spec,
            cwd,
            &staged,
            launch.spawn_capability.as_ref().filter(|_| agent_tool.is_some()),
        );
        let launch_prompt = match spec.prompt.as_deref().map(str::trim) {
            Some(b) if !b.is_empty() => Some(format!("{session_context}\n\n{b}")),
            _ => Some(session_context),
        };
        if let Some(prompt) = &launch_prompt {
            args.push("--".to_owned());
            args.push(prompt.clone());
        }
        // Keep the display intent the user's original prompt/name — the staged
        // paths live in the launch arg, not the session label.
        let intent = spec.prompt.clone().or_else(|| spec.name.clone()).unwrap_or_default();

        // The daemon's seed schema is `{intent, name?, nameSource?, …}` and
        // its state.json writer reads `name`/`intent` off the seeded roster
        // entry. Seeding only `intent` (as we did before) left dispatched
        // sessions with no display name. Seed `name` + `nameSource:"user"`
        // when the caller provided one.
        let mut seed = serde_json::Map::new();
        seed.insert("intent".to_owned(), json!(intent));
        if let Some(name) = &spec.name {
            seed.insert("name".to_owned(), json!(name));
            seed.insert("nameSource".to_owned(), json!("user"));
        }

        // Environment secrets: merged on top of the spare's baseline
        // env in the worker process. Mirror into `reattachEnv` so they survive
        // the respawn/reattach the claude daemon drives after a CLI upgrade.
        // These values are NOT placed in `seed`/`intent`/`launch.args`, so they
        // never reach the transcript, timeline, or `state.json`.
        //
        // Gateway env resolved above: a spawn whose server-side mint
        // silently produced nothing already failed closed there rather than
        // launching a worker that will 401.
        let env = launch.env;
        let env_json: serde_json::Map<String, serde_json::Value> =
            env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();

        let req = json!({
            "proto": 1,
            "op": "dispatch",
            "timeoutMs": 15000,
            "d": {
                "proto": 1,
                "short": short,
                "nonce": nonce,
                "sessionId": session_id,
                "createdAt": created_at,
                "source": "fleet",
                "cwd": cwd,
                "launch": { "mode": "prompt", "args": args },
                "env": env_json,
                "reattachEnv": env_json,
                "isolation": "none",
                "respawnFlags": respawn_flags,
                "agent": agent,
                "seed": seed,
                "cols": 120,
                "rows": 40,
            }
        });

        Ok(DeferredDispatch {
            sock: sock.to_path_buf(),
            req,
            short: short.to_owned(),
            what: format!("spawn in {cwd}"),
            session_id,
        })
    }

    /// Fork an existing conversation into a brand-new claude session.
    ///
    /// Mirrors [`spawn`] — mints a fresh `short`/`sessionId`/`nonce` and
    /// dispatches a new worker via the control socket — but prepends `--resume
    /// <parent-session-id> --fork-session` to the launch argv so claude copies
    /// the parent's history into the new session id, leaving the parent intact.
    /// `--model`/`--effort` from `spec` ride on top (this is the supported
    /// "switch model mid-conversation" path).
    ///
    /// The parent session id is resolved to the id claude should resume from:
    /// the parent's on-disk `resumeSessionId` when present (so a `/clear`ed or
    /// `/compact`ed parent forks from the live conversation, not the stale spawn
    /// id), else the parent's `sessionId`, else the `parent_local_id`
    /// itself (covers reopening an archived parent whose `state.json` was removed
    /// by `claude rm` but whose transcript still resumes).
    ///
    /// The child's `SessionStarted` is emitted later by the roster-discovery
    /// path, which has no fork context, so we stash `parent_local_id` keyed by
    /// the new `short` in `fork_parent_by_short` for it to read.
    /// Write a sliced copy of the parent transcript as the child's own
    /// `<child>.jsonl` for a subset fork. Reads the parent JSONL,
    /// keeps only the lines the `extract` selects, and writes the repaired
    /// slice to the child's path under the SAME encoded cwd. Writes are strictly
    /// to the new child file — the parent transcript is never touched.
    fn materialize_fork_slice(
        &self,
        cwd: &str,
        parent_session_id: &str,
        child_session_id: &str,
        extract: &cctui_proto::adapter::ForkExtract,
    ) -> anyhow::Result<()> {
        let parent_path =
            transcript::transcript_path(&self.cfg.projects_root, cwd, parent_session_id);
        let raw = std::fs::read_to_string(&parent_path).with_context(|| {
            format!("fork slice: read parent transcript {}", parent_path.display())
        })?;
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .with_context(|| format!("fork slice: parse {}", parent_path.display()))?;
        let kept = super::fork_slice::slice_transcript(&lines, extract, child_session_id)?;
        let child_path =
            transcript::transcript_path(&self.cfg.projects_root, cwd, child_session_id);
        if let Some(dir) = child_path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("fork slice: create child dir {}", dir.display()))?;
        }
        let body: String = kept.iter().map(|l| format!("{l}\n")).collect::<Vec<_>>().concat();
        std::fs::write(&child_path, body).with_context(|| {
            format!("fork slice: write child transcript {}", child_path.display())
        })?;
        tracing::info!(
            parent = %parent_path.display(),
            child = %child_path.display(),
            kept = kept.len(),
            mode = ?extract.mode,
            "materialized subset fork transcript"
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn fork(
        &self,
        sock: &std::path::Path,
        parent_local_id: &str,
        spec: &cctui_proto::adapter::SessionSpec,
        forced_session_id: Option<&str>,
        extract: Option<&cctui_proto::adapter::ForkExtract>,
    ) -> anyhow::Result<DeferredDispatch> {
        let cwd = spec
            .working_dir
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("fork: working_dir required"))?;
        let cwd_path = std::path::Path::new(cwd);
        if !cwd_path.is_dir() {
            anyhow::bail!("fork: working_dir does not exist or is not a directory: {cwd}");
        }

        // Resolve the id to resume+fork from. Prefer the parent's on-disk
        // `resumeSessionId` (the live conversation head after `/clear`/`/compact`),
        // then its `sessionId`, then the raw parent id (archived
        // parent whose job state was removed by `claude rm`, but whose transcript
        // still resumes — the native "reopen archived as a new conversation").
        let resume_id = self
            .resolve_short_for_removal(parent_local_id)
            .ok()
            .and_then(|short| StateJson::read(&self.cfg.jobs_root, &short))
            .and_then(|st| st.resume_session_id.or(st.session_id))
            .unwrap_or_else(|| parent_local_id.to_owned());

        let agent = "claude";
        // Use the server-pre-minted child id when supplied so the id
        // the webui navigated to matches the worker the daemon launches.
        let session_id =
            forced_session_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
        let short = session_id[..8].to_owned();
        let nonce: String = uuid::Uuid::new_v4().simple().to_string().chars().take(8).collect();
        let created_at = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
        )
        .unwrap_or(0);

        // Subset fork: the child `<child>.jsonl` is a standalone
        // sliced transcript, so it is resumed WITHOUT `--fork-session` (that
        // flag branches off the parent's live history, which we don't want).
        let sliced = if let Some(extract) = extract {
            self.materialize_fork_slice(cwd, &resume_id, &session_id, extract)?;
            true
        } else {
            false
        };
        let mut args = if sliced {
            vec![
                "--resume".to_owned(),
                session_id.clone(),
                "--session-id".to_owned(),
                session_id.clone(),
                "--agent".to_owned(),
                agent.to_owned(),
            ]
        } else {
            vec![
                "--resume".to_owned(),
                resume_id.clone(),
                "--fork-session".to_owned(),
                "--session-id".to_owned(),
                session_id.clone(),
                "--agent".to_owned(),
                agent.to_owned(),
            ]
        };
        if let Some(name) = &spec.name {
            args.push("--name".to_owned());
            args.push(name.clone());
        }
        if let Some(mode) = spec.permission_mode {
            args.push("--permission-mode".to_owned());
            args.push(mode.claude_flag().to_owned());
        }
        let mut respawn_flags = vec!["--agent".to_owned(), agent.to_owned()];
        if let Some(effort) = spec.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            args.push("--effort".to_owned());
            args.push(effort.to_owned());
            respawn_flags.push("--effort".to_owned());
            respawn_flags.push(effort.to_owned());
        }
        if let Some(model) = spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            args.push("--model".to_owned());
            args.push(model.to_owned());
            respawn_flags.push("--model".to_owned());
            respawn_flags.push(model.to_owned());
        }
        {
            let model = spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
            let effort = spec.effort.as_deref().map(str::trim).filter(|e| !e.is_empty());
            if (model.is_some() || effort.is_some())
                && let Ok(mut map) = self.spawn_model_effort.lock()
            {
                map.insert(short.clone(), (model.map(str::to_owned), effort.map(str::to_owned)));
            }
        }
        // Remember the launch posture for the diagnose report.
        if let Some(mode) = spec.permission_mode
            && let Ok(mut map) = self.spawn_permission_mode.lock()
        {
            map.insert(short.clone(), super::diagnose::permission_label(mode).to_owned());
        }
        let whip = spec.permission_mode.is_some_and(cctui_proto::adapter::PermissionMode::is_whip);
        // Gateway env + per-account settings for the fork child: the
        // Resolve for the child id first; if the server
        // hasn't bound it yet, inherit the parent's account env (and settings) so
        // the child routes through the gateway from its first turn. Empty when
        // neither is account-bound. Resolved BEFORE the hook-settings file is
        // written so the account settings can be merged under the managed
        // hooks.
        let mut launch =
            self.resolve_launch_env(&session_id, &std::collections::BTreeMap::default()).await?;
        if launch.env.is_empty() {
            launch = self
                .resolve_launch_env(parent_local_id, &std::collections::BTreeMap::default())
                .await?;
        }
        if let Some(settings) = ensure_hook_settings(
            &self.cfg.hook_socket_path,
            whip,
            &short,
            launch.settings.as_ref(),
            &launch.env,
            None,
            None,
            launch.whip_phrases.as_ref(),
        ) {
            let settings = settings.to_string_lossy().into_owned();
            args.push("--settings".to_owned());
            args.push(settings.clone());
            respawn_flags.push("--settings".to_owned());
            respawn_flags.push(settings);
        }
        // Optional first turn on the forked branch.
        if let Some(prompt) = spec.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            args.push("--".to_owned());
            args.push(prompt.to_owned());
        }
        let intent = spec.prompt.clone().or_else(|| spec.name.clone()).unwrap_or_default();
        let mut seed = serde_json::Map::new();
        seed.insert("intent".to_owned(), json!(intent));
        if let Some(name) = &spec.name {
            seed.insert("name".to_owned(), json!(name));
            seed.insert("nameSource".to_owned(), json!("user"));
        }

        // Remember the parent BEFORE dispatching so the roster-discovery emit
        // (which can race in on the very next poll) finds the link.
        if let Ok(mut map) = self.fork_parent_by_short.lock() {
            map.insert(short.clone(), (parent_local_id.to_owned(), "fork"));
        }

        // Gateway env resolved above.
        let env = launch.env;
        let env_json: serde_json::Map<String, serde_json::Value> =
            env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();

        let req = json!({
            "proto": 1,
            "op": "dispatch",
            "timeoutMs": 15000,
            "d": {
                "proto": 1,
                "short": short,
                "nonce": nonce,
                "sessionId": session_id,
                "createdAt": created_at,
                "source": "fleet",
                "cwd": cwd,
                "launch": { "mode": "prompt", "args": args },
                "env": env_json,
                "reattachEnv": env_json,
                "isolation": "none",
                "respawnFlags": respawn_flags,
                "agent": agent,
                "seed": seed,
                "cols": 120,
                "rows": 40,
            }
        });
        tracing::info!(%cwd, %session_id, %parent_local_id, %resume_id, "fork prepared for control socket");
        Ok(DeferredDispatch {
            sock: sock.to_path_buf(),
            req,
            short: short.clone(),
            what: format!("fork of {parent_local_id} in {cwd}"),
            session_id,
        })
    }

    /// Locate the control socket, booting the on-demand claude daemon if it's
    /// missing and waiting (up to ~12s) for the socket to appear. Used on the
    /// command path, where failing to find a socket means a dropped spawn/
    /// reply rather than just a skipped poll. `claude daemon run`
    /// needs a few seconds to start the supervisor and bind the socket, so the
    /// window is generous.
    async fn ensure_socket(&self) -> anyhow::Result<PathBuf> {
        if let Some(sock) = self.cfg.discovery.locate_live().await {
            return Ok(sock);
        }
        self.kickstarter.kick(true);
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(sock) = self.cfg.discovery.locate_live().await {
                return Ok(sock);
            }
        }
        anyhow::bail!("no claude daemon socket present (kickstart did not bring it up in time)");
    }

    /// Pick up a `claude` CLI auto-update the running daemon missed. Cycles
    /// only when both our roster and the daemon's own worker count agree
    /// nothing is running; a mismatch over live work is logged and left alone.
    async fn maybe_cycle_stale_daemon(&self) {
        use super::version_gate::{CycleMethod, Decision};

        // The direct-spawn fallback (containers) is excluded: those workers run
        // `--no-auto-update`, so they never drift, and we keep no pid to stop.
        if !super::claude_service::manager_available() {
            return;
        }
        if let Some(Decision::Cycle { running, local, escalated: _ }) =
            self.version_gate.check(self.roster.len()).await
        {
            let method = if tokio::task::spawn_blocking(super::claude_service::service_active)
                .await
                .unwrap_or(false)
            {
                CycleMethod::ManagedService
            } else {
                CycleMethod::StopAny
            };
            self.cycle_daemon(method, &running, &local).await;
        }
    }

    #[allow(clippy::cognitive_complexity)]
    async fn cycle_daemon(&self, method: super::version_gate::CycleMethod, run: &str, new: &str) {
        tracing::info!(%run, %new, ?method, "cycling idle claude daemon onto the new CLI");
        if let Err(err) = self.version_gate.cycle(method).await {
            tracing::warn!(%err, ?method, "failed to cycle the stale claude daemon");
            return;
        }
        match self.ensure_socket().await {
            Ok(sock) => tracing::info!(sock = %sock.display(), %new, "claude daemon back up"),
            Err(err) => tracing::warn!(%err, "claude daemon did not come back after the cycle"),
        }
    }

    async fn poll_once(&mut self) -> anyhow::Result<()> {
        let Some(sock) = self.cfg.discovery.locate_live().await else {
            // Daemon isn't running. Boot it (rate-limited) so it self-heals
            // before the next dispatch, and treat any sessions we
            // previously knew about as ended.
            self.kickstarter.kick(false);
            self.flush_roster(EndReason::Other { detail: "daemon gone".into() }).await;
            // Roster churn: the socket vanished and sessions were
            // flushed. When it comes back the workers are re-pinned and the
            // tail resumes from the persisted offset — but a re-home can leave
            // a gap (briefly tailing a file no longer appended, or a send
            // dropped during the churn). Arm an immediate reconcile on the
            // next successful poll so the gap self-heals without waiting for
            // the periodic cycle.
            self.churned = true;
            return Ok(());
        };

        let resp: ListResponse = socket::call(&sock, &json!({"proto": 1, "op": "list"})).await?;
        let finished: Vec<String> = resp
            .jobs
            .iter()
            .filter(|j| {
                j.is_user_visible()
                    && !j.is_dead()
                    && crate::mempressure::turn_is_over(
                        j.tempo.as_deref(),
                        j.state.as_deref(),
                        j.needs.as_deref(),
                    )
            })
            .map(|j| j.short.clone())
            .collect();
        self.apply_snapshot(resp.jobs).await;
        self.sleep_under_memory_pressure(&sock, &finished).await;
        let reattached = self.churned;
        if self.churned {
            self.churned = false;
            self.reconcile_tail(true).await;
        }
        if reseed_due(self.last_reseed, Self::reseed_interval(), reattached) {
            self.reseed_gateway_env().await;
            self.last_reseed = Some(Instant::now());
        }
        Ok(())
    }

    /// When host memory runs short, stop the worker of the session idle the
    /// longest among `finished` (shorts whose turn is over), leaving its job
    /// state on disk: the roster then reports it hibernated and the next reply
    /// revives it. One worker per call, then a pause (see [`crate::mempressure`]).
    async fn sleep_under_memory_pressure(&mut self, sock: &Path, finished: &[String]) {
        let Some(policy) = self.sleep_policy else { return };
        if self.next_sleep_check.is_some_and(|t| Instant::now() < t) {
            return;
        }
        // A dispatched worker pod runs its one session; stopping it would only
        // end the job early.
        if self.dispatch_done.lock().is_ok_and(|g| g.is_some()) {
            return;
        }
        let Some(mem) = crate::mempressure::Memory::read() else { return };
        if !policy.under_pressure(mem) {
            return;
        }
        let candidates: Vec<crate::mempressure::Candidate> = {
            let asks: HashSet<String> =
                self.pending_asks.lock().map(|m| m.keys().cloned().collect()).unwrap_or_default();
            finished
                .iter()
                .filter(|short| {
                    !self.dead_shorts.contains(*short) && !self.pending_perms.contains_key(*short)
                })
                .filter_map(|short| {
                    let loc = self.transcript_locations.get(short)?;
                    let busy = asks.contains(&loc.local_id)
                        || self.subagents.values().any(|s| s.parent_local_id == loc.local_id);
                    if busy {
                        return None;
                    }
                    // The transcript's last write is when the session last did
                    // anything, and it survives a daemon restart.
                    let quiet_since =
                        std::fs::metadata(&loc.path).and_then(|m| m.modified()).ok()?;
                    Some(crate::mempressure::Candidate { short: short.clone(), quiet_since })
                })
                .collect()
        };
        let now = SystemTime::now();
        let Some(victim) = crate::mempressure::pick(&candidates, policy.min_idle, now) else {
            tracing::debug!(
                available = mem.available,
                total = mem.total,
                "memory pressure but no idle session to put to sleep"
            );
            return;
        };
        let quiet_mins = now.duration_since(victim.quiet_since).unwrap_or_default().as_secs() / 60;
        let short = victim.short.clone();
        match socket::one_shot(sock, &json!({"proto":1,"op":"kill","short":short})).await {
            Ok(_) => tracing::info!(
                %short,
                quiet_mins,
                available_mb = mem.available / (1024 * 1024),
                total_mb = mem.total / (1024 * 1024),
                threshold_pct = policy.threshold_pct,
                "memory pressure: put the longest idle session to sleep (hibernated, a reply wakes it)"
            ),
            Err(err) => {
                tracing::warn!(%short, %err, "memory pressure: stopping an idle worker failed");
            }
        }
        self.next_sleep_check = Some(Instant::now() + crate::mempressure::COOLDOWN);
    }

    /// Renew each live account-bound worker's gateway token by re-pulling its
    /// env. The token STRING is stable, so the running worker (and its persisted
    /// `--settings` delivery) needs no rewrite — the pull only re-mints to bump
    /// the short-TTL expiry. A genuinely env-less worker is deliberately NOT
    /// force-respawned here; that fails loud at the launch chokepoint instead.
    async fn reseed_gateway_env(&self) {
        let (Some(server), Some(mk)) = (self.server.as_ref(), self.machine_key.as_ref()) else {
            return;
        };
        let targets: Vec<String> = self
            .short_by_session
            .iter()
            .filter(|(_, short)| self.roster.contains(*short))
            .map(|(local_id, _)| local_id.clone())
            .collect();
        let mut renewed = 0usize;
        for local_id in &targets {
            match server.gateway_env(mk, local_id).await {
                Ok(resp) if resp.account_bound => renewed += 1,
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(%local_id, %err, "gateway-env re-seed pull failed (will retry)");
                }
            }
        }
        if renewed > 0 {
            tracing::info!(renewed, "re-seeded gateway env for live account-bound workers");
        }
    }

    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn apply_snapshot(&mut self, jobs: Vec<LiveSnapshot>) {
        let visible: Vec<LiveSnapshot> =
            jobs.into_iter().filter(LiveSnapshot::is_user_visible).collect();
        if visible.iter().any(|j| {
            !j.is_dead() && DispatchDoneTracker::is_busy(j.tempo.as_deref(), j.state.as_deref())
        }) {
            self.version_gate.note_roster_busy();
        }
        let now_shorts: HashSet<String> = visible.iter().map(|j| j.short.clone()).collect();

        // Ground-truth effort for every live worker in one `/proc` pass,
        // reused across the per-job Status build below so a busy roster doesn't
        // rescan `/proc` per session.
        let observed_efforts = super::envcheck::worker_efforts(&now_shorts);

        // Newly started.
        for job in &visible {
            if !self.roster.contains(&job.short) {
                let session_id = job.session_id().map_or_else(|| job.short.clone(), str::to_owned);
                self.short_by_session.insert(session_id.clone(), job.short.clone());
                // If this short was just forked or spawned as a subagent,
                // carry the parent link so the server resolves it into
                // `parent_id`. Consumed once.
                let parent =
                    self.fork_parent_by_short.lock().ok().and_then(|mut m| m.remove(&job.short));
                let (parent_local_id, relation) = match parent {
                    Some((parent, relation)) => (Some(parent), relation),
                    None => (None, "root"),
                };
                let on_disk = StateJson::read(&self.cfg.jobs_root, &job.short);
                let created_at = on_disk.as_ref().and_then(|s| s.created_at.clone());
                let mut extra = json!({
                    "short": job.short,
                    "relation": relation,
                });
                // The server merges metadata with jsonb `||`, where an explicit
                // null overwrites. Omit what this poll could not read so a
                // rediscovery never erases what an earlier one published.
                for (key, value) in [
                    ("cli_version", job.cli_version.clone()),
                    ("created_at", created_at),
                    ("git_branch", job.cwd.as_deref().and_then(read_git_branch)),
                    ("git_remote", job.cwd.as_deref().and_then(read_git_remote)),
                ] {
                    if let Some(value) = value {
                        extra[key] = json!(value);
                    }
                }
                self.emit(AdapterEvent::SessionStarted {
                    local_id: session_id,
                    meta: SessionMeta { working_dir: job.cwd.clone(), parent_local_id, extra },
                })
                .await;
            }
        }

        // Status updates (live snapshot + on-disk state.json reconciliation).
        for job in &visible {
            // The emitted `local_id` is STABLE for a worker's whole life. Once a
            // transcript is pinned we keep reusing its `local_id` even when the
            // session id rotates in place (`/clear`, `/compact`), so every
            // message lands in the one session the server already knows. Only
            // the very first pin derives the id from the live `session_id`.
            let local_id = self
                .transcript_locations
                .get(&job.short)
                .map(|loc| loc.local_id.clone())
                .or_else(|| job.session_id().map(str::to_owned))
                .unwrap_or_else(|| job.short.clone());
            let on_disk = StateJson::read(&self.cfg.jobs_root, &job.short);

            // Observation timestamp for the diagnose report: when
            // this short was last seen on the control socket.
            self.last_status_at.insert(job.short.clone(), std::time::SystemTime::now());

            // Dead-but-still-listed. claude can keep a session in
            // `daemon list` while its worker process is gone (e.g. it died
            // while the supervisor was down — "process gone while supervisor
            // down"). Previously cctui only transitioned a session to
            // hibernated/ended when its `short` DROPPED OFF the roster
            // (`gone` handling below), and liveness was otherwise purely
            // time-derived (server `derive_liveness`), so such a session kept
            // showing its last status with a green/stale dot for minutes. Here
            // we surface the dead state within one poll instead.
            if job.is_dead() {
                // Emit the terminal transition exactly once, then mark the
                // short sticky so the still-present roster entry can't re-emit
                // a non-terminal Status and re-green it (daemon-side mirror of
                // the server's sticky terminal status). Mirrors the
                // roster-disappearance path: hibernated if job state survives
                // on disk (revivable red dot), else SessionEnded.
                if self.dead_shorts.insert(job.short.clone()) {
                    self.clear_permission(&job.short).await;
                    if on_disk.is_some() {
                        self.emit(AdapterEvent::Status {
                            local_id: local_id.clone(),
                            tempo: Some("hibernated".to_owned()),
                            state: None,
                            detail: None,
                            activity: None,
                            name: None,
                            intent: None,
                            model: None,
                            effort: None,
                            permission_mode: None,
                            children: Vec::new(),
                        })
                        .await;
                    } else {
                        self.emit(AdapterEvent::SessionEnded {
                            local_id: local_id.clone(),
                            reason: EndReason::Completed,
                        })
                        .await;
                        // Truly gone — no job state left to cold resume from, so
                        // the spawn flags and per-session config files are dead.
                        if let Ok(mut m) = self.spawn_model_effort.lock() {
                            m.remove(&job.short);
                        }
                        crate::configsweep::remove_session_files(&job.short);
                    }
                    // Drop the cached status so a later revive (worker reports
                    // alive again) is detected as a change and re-emitted.
                    self.last_status.remove(&job.short);
                }
                // Sticky: skip transcript re-pin + Status for this poll. The
                // roster-disappearance branch still cleans up if it later
                // drops off; a revive clears `dead_shorts` (below) so live
                // status resumes.
                continue;
            }
            // Revived: claude reports this short alive again after we marked it
            // dead — clear the sticky flag so live status flows again.
            self.dead_shorts.remove(&job.short);

            // Gateway-env delivery is handled entirely at the launch chokepoint
            // (`resolve_launch_env`): the resolved env rides the per-session
            // `--settings` file + `reattachEnv`, both of which the claude daemon
            // re-applies on its own autonomous respawns (`/clear`, `/compact`,
            // spare-claim), so a revived worker keeps its routing without cctui
            // killing it. The former proactive kill+cold-resume heal
            // and the `/proc`-env delivery probe were removed: post
            // they mis-fired on healthy spare-claimed workers (env
            // delivered via `--settings`, not process env) and were a no-op on
            // macOS (no `/proc`). A genuinely env-less launch now fails LOUD in
            // `launch_env_decision` instead of being silently healed.

            // Surface (or clear) a tool-permission prompt from the live
            // `tempo`/`needs` signal, before the Status emit below.
            self.reconcile_permission(
                &job.short,
                &local_id,
                job.tempo.as_deref(),
                job.needs.as_deref(),
            )
            .await;

            // Pin (or re-pin) the transcript location. A resume or an in-process
            // reset (`/clear`, `/compact`) changes the session's `sessionId` and
            // starts a NEW transcript file (`<newId>.jsonl`); if we kept tailing
            // the original file the message stream would silently stop while
            // `list`/Status polls kept the heartbeat fresh. So re-pin
            // whenever the live `session_id` differs from the one we cached,
            // following the transcript to the new file.
            //
            // A reset keeps the same worker `short`, so the "Newly
            // started" branch never fires for the new id. We deliberately keep
            // emitting under the ORIGINAL `local_id` (set on the first pin, kept
            // in `loc.local_id`) and only move `path`/`offset_key` to the new
            // file — so the post-reset transcript appends to the one session the
            // server already knows. Splitting it into a second session would be
            // worse: archive is worker-scoped (`claude rm <short>`), so a single
            // archive would wipe both conversations at once. Instead we inject a
            // `context_reset` boundary marker so the cut is visible in the UI.
            // `/clear` rotates the live session into a new transcript
            // file but the control socket's `list` keeps reporting the stale
            // spawn `sessionId` (it's the immutable `--session-id` launch arg in
            // `roster.json`). The rotated id only surfaces in `state.json`'s
            // `resumeSessionId`, so prefer that; fall back to the snapshot id
            // when no reset has happened. Without this the rotation check below
            // never fires for `/clear` and the message stream silently stops.
            let live_session = on_disk
                .as_ref()
                .and_then(|s| s.resume_session_id.as_deref())
                .or_else(|| job.session_id());
            if let (Some(cwd), Some(sess)) = (job.cwd.as_deref(), live_session) {
                let rotated = self
                    .transcript_locations
                    .get(&job.short)
                    .is_some_and(|loc| loc.offset_key != sess);
                let first_pin = !self.transcript_locations.contains_key(&job.short);
                let path = self.resolve_live_transcript(cwd, sess);
                let moved = !first_pin
                    && !rotated
                    && self
                        .transcript_locations
                        .get(&job.short)
                        .is_some_and(|loc| loc.path != path);
                if moved {
                    // Same session id, new file: the session entered/left a git
                    // worktree so claude relocated the transcript. The move is
                    // content-continuous, so keep offset_key + offset and the
                    // stable local_id — only follow the path.
                    if let Some(loc) = self.transcript_locations.get_mut(&job.short) {
                        tracing::info!(
                            short = %job.short,
                            from = %loc.path.display(),
                            to = %path.display(),
                            "transcript moved (worktree enter/exit); following"
                        );
                        loc.path.clone_from(&path);
                    }
                }
                if first_pin {
                    self.short_by_session.insert(sess.to_owned(), job.short.clone());
                    self.map_session(sess, &local_id);
                    self.transcript_locations.insert(
                        job.short.clone(),
                        TranscriptLocation {
                            path,
                            local_id: local_id.clone(),
                            cwd: cwd.to_owned(),
                            offset_key: sess.to_owned(),
                        },
                    );
                } else if rotated {
                    // Follow the file, keep the stable `local_id`. The new
                    // `sess` is mapped to the same `short` too so command
                    // dispatch keeps working if a snapshot ever reports the new
                    // id directly. The rotated id maps to the unchanged stable
                    // `local_id` so a hook firing post-`/clear` still resolves
                    // to the session the server knows.
                    self.short_by_session.insert(sess.to_owned(), job.short.clone());
                    self.map_session(sess, &local_id);
                    if let Some(loc) = self.transcript_locations.get_mut(&job.short) {
                        loc.path = path;
                        sess.clone_into(&mut loc.offset_key);
                    }
                    self.emit(AdapterEvent::Message {
                        local_id: local_id.clone(),
                        payload: json!({
                            "role": "context_reset",
                            "text": "context reset (/clear · /compact)",
                            // The new session id keys this marker uniquely so a
                            // second reset isn't collapsed by the server's
                            // content-hash dedup (identical text would hash the
                            // same).
                            "session_id": sess,
                        }),
                        turn_id: None,
                    })
                    .await;
                }
            }

            let name = on_disk.as_ref().and_then(|s| s.name.clone()).or_else(|| job.name.clone());
            let intent =
                on_disk.as_ref().and_then(|s| s.intent.clone()).or_else(|| job.intent.clone());
            let activity = on_disk.as_ref().and_then(|s| s.activity.clone());
            // Prefer the on-disk state.json; fall back to the spawn-time flags
            // we remembered while state.json is absent/transient.
            let spawned =
                self.spawn_model_effort.lock().ok().and_then(|m| m.get(&job.short).cloned());
            let model = on_disk
                .as_ref()
                .and_then(|s| s.model.clone())
                .or_else(|| spawned.as_ref().and_then(|(m, _)| m.clone()));
            // Prefer the GROUND-TRUTH effort the live worker actually booted at
            // (read from its `CLAUDE_EFFORT` env), so the UI shows what the
            // session is running rather than what we requested — a spare-claim or
            // a silent background clamp can make them differ. Fall back
            // to the requested value (state.json flags, then the spawn cache)
            // while the worker is mid-exec / not yet found in `/proc`.
            let effort = observed_efforts
                .get(&job.short)
                .cloned()
                .or_else(|| on_disk.as_ref().and_then(|s| s.effort.clone()))
                .or_else(|| spawned.as_ref().and_then(|(_, e)| e.clone()));
            let children = on_disk.as_ref().map(StateJson::proto_children).unwrap_or_default();

            // NB: live `AskUserQuestion` surfacing is NOT derived from status
            // here. Real questions report `state:"done"`, not `blocked`, and a
            // `blocked` state is a background status (e.g. "needs input"), not a
            // question. The `AskUserQuestion` PreToolUse hook delivers the real
            // prompt over the daemon socket.

            let snap = StatusSnapshot {
                tempo: job.tempo.clone(),
                state: job.state.clone(),
                detail: job.detail.clone(),
                name: name.clone(),
                activity: activity.clone(),
                model: model.clone(),
                effort: effort.clone(),
            };
            let changed = self.last_status.get(&job.short) != Some(&snap);
            if changed {
                self.last_status.insert(job.short.clone(), snap);
                self.emit(AdapterEvent::Status {
                    local_id,
                    tempo: job.tempo.clone(),
                    state: job.state.clone(),
                    detail: job.detail.clone(),
                    activity,
                    name,
                    intent,
                    model,
                    effort,
                    permission_mode: None,
                    children,
                })
                .await;
            }
        }

        // Tail transcripts for every visible session and emit new events.
        // Done after Status updates so the UI's identity fields land
        // before the message stream they describe.
        let mut dirty_offsets = false;
        let locations: Vec<TranscriptLocation> =
            self.transcript_locations.values().cloned().collect();
        for loc in locations {
            let prev = self.offsets.get(&loc.offset_key);
            let off = self.resume_offset(&loc.offset_key, &loc.path);
            if off != prev {
                dirty_offsets = true;
            }
            match transcript::tail_once(&loc.path, &loc.local_id, off) {
                Ok((events, new_off)) => {
                    if new_off != off {
                        self.offsets.set(loc.offset_key.clone(), new_off);
                        self.server_marks.insert(loc.offset_key.clone(), new_off);
                        dirty_offsets = true;
                    }
                    if let Some(last) = events.last() {
                        // "Last parsed event" for diagnose.
                        self.last_parsed.insert(
                            loc.offset_key.clone(),
                            (
                                super::diagnose::event_kind(last).to_owned(),
                                std::time::SystemTime::now(),
                            ),
                        );
                    }
                    // A parent's `tool_result` closes the `Task` call it
                    // answers, and with it the subagent that ran the call.
                    // Read before emitting (which consumes `events`).
                    for evt in &events {
                        self.note_parent_tool_result(evt);
                    }
                    for evt in events {
                        self.emit(evt).await;
                    }
                    if new_off != off {
                        self.emit(AdapterEvent::TranscriptMark {
                            local_id: loc.local_id.clone(),
                            offset: new_off,
                        })
                        .await;
                    }
                }
                Err(err) => {
                    tracing::debug!(%err, path = %loc.path.display(), "transcript tail failed");
                }
            }
        }
        // Discover + tail Task-tool subagents nested under each live parent.
        // Runs after the parent tail so a subagent's parent row
        // exists before its own SessionStarted references it.
        self.scan_subagents(&mut dirty_offsets).await;

        if dirty_offsets {
            self.offsets.flush();
        }

        // Ended sessions.
        let gone: Vec<String> = self.roster.difference(&now_shorts).cloned().collect();
        for short in &gone {
            self.last_status.remove(short);
            let was_dead = self.dead_shorts.remove(short);
            self.clear_permission(short).await;
            if let Some(loc) = self.transcript_locations.remove(short) {
                // Hibernated, not gone: the worker process exited but
                // its job state survives on disk, so a reply will revive it
                // (resume-on-reply above). Mark the session so the UI can show
                // the claude-style "exited, will resume on reply" red dot
                // instead of a plain dead one. Carried in `tempo` (not
                // `agent_state`) so the bucket classifier still sees the final
                // state (`done` → Completed); a revived worker's next live
                // snapshot overwrites it.
                //
                // Skip if we already emitted this short's dead transition while
                // it was still listed (`dead_shorts`) — the hibernated
                // Status already went out; re-emitting it here is redundant.
                if !was_dead && StateJson::read(&self.cfg.jobs_root, short).is_some() {
                    self.emit(AdapterEvent::Status {
                        local_id: loc.local_id.clone(),
                        tempo: Some("hibernated".to_owned()),
                        state: None,
                        detail: None,
                        activity: None,
                        name: None,
                        intent: None,
                        model: None,
                        effort: None,
                        permission_mode: None,
                        children: Vec::new(),
                    })
                    .await;
                }
                self.short_by_session.remove(&loc.local_id);
                self.session_to_local
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, v| v != &loc.local_id);
                self.end_subagents_of(&loc.local_id).await;
            }
            // We don't retain the session_id mapping after roster removal,
            // so fall back to the short as the local_id. Sessions on the
            // server side are indexed by (machine_id, adapter_id,
            // local_id), and the prior SessionStarted carried the real
            // session_id; the server reconciles on the running row.
            self.emit(AdapterEvent::SessionEnded {
                local_id: short.clone(),
                reason: EndReason::Completed,
            })
            .await;
        }

        // Keep a headless `attach` open for every live session so the worker
        // stays focused/awake and `reply` actually drives its PTY.
        self.attach.reconcile(now_shorts.iter().map(String::as_str));

        self.tick_dispatch_done(&visible);

        self.roster = now_shorts;
    }

    /// The `meta.extra` a newly discovered subagent is announced with. The
    /// `.meta.json` sidecar supplies the agent type and the exact parent
    /// `Task` tool call; Workflow-tool agents add run context so the UI can
    /// group them under a named "Workflow: <name> (<runId>)" node. A
    /// workflow agent with no sidecar keeps the `workflow-subagent` default.
    fn subagent_extra(
        agent_id: &str,
        workflow: Option<&transcript::WorkflowContext>,
        meta: Option<&transcript::SubagentMeta>,
    ) -> serde_json::Value {
        let mut extra = json!({ "subagent": true, "agent_id": agent_id });
        let obj = extra.as_object_mut().expect("json object literal");
        if let Some(wf) = workflow {
            obj.insert("workflow_run_id".into(), json!(wf.run_id));
            if let Some(name) = &wf.name {
                obj.insert("workflow_name".into(), json!(name));
            }
        }
        let agent_type = meta
            .and_then(|m| m.agent_type.clone())
            .or_else(|| workflow.is_some().then(|| "workflow-subagent".to_owned()));
        if let Some(agent_type) = agent_type {
            obj.insert("agent_type".into(), json!(agent_type));
        }
        if let Some(m) = meta {
            if let Some(tool_use_id) = &m.tool_use_id {
                obj.insert("tool_use_id".into(), json!(tool_use_id));
            }
            if let Some(parent_agent_id) = &m.parent_agent_id {
                obj.insert("parent_agent_id".into(), json!(parent_agent_id));
            }
            if let Some(depth) = m.spawn_depth {
                obj.insert("spawn_depth".into(), json!(depth));
            }
        }
        extra
    }

    /// Carry the sidecar's `description` on the ordinary session-name path, so
    /// it persists like any other name and a sidecarless subagent keeps the
    /// 6-char-id fallback.
    fn subagent_name_status(agent_id: &str, name: String) -> AdapterEvent {
        AdapterEvent::Status {
            local_id: agent_id.to_owned(),
            tempo: None,
            state: None,
            detail: None,
            activity: None,
            name: Some(name),
            intent: None,
            model: None,
            effort: None,
            permission_mode: None,
            children: Vec::new(),
        }
    }

    /// Feed the dispatch turn-complete watcher one roster snapshot
    /// and write the `dispatch_done` marker when it fires. No-op on normal
    /// daemons (`dispatch_done` is only armed by `maybe_dispatch_on_start`).
    fn tick_dispatch_done(&self, jobs: &[LiveSnapshot]) {
        let Ok(mut guard) = self.dispatch_done.lock() else { return };
        let Some(tracker) = guard.as_mut() else { return };
        let job = jobs.iter().find(|j| j.short == tracker.short());
        // Absent from the roster before it ever ran isn't idleness — it's a
        // cold start still booting (the entrypoint's boot deadline bounds
        // that). Once seen busy, absence (session ended/retired) does count
        // toward settle.
        if job.is_none() && !tracker.seen_busy() {
            return;
        }
        // A growing transcript is authoritative liveness: the control-socket
        // snapshot can read idle while a worktree-entered session is still
        // working, so never let the settle clock run purely on that signal.
        let transcript_offset = self
            .transcript_locations
            .get(tracker.short())
            .map_or(0, |loc| self.offsets.get(&loc.offset_key));
        let grew = tracker.transcript_grew(transcript_offset);
        let busy = grew
            || job.is_some_and(|j| {
                !j.is_dead() && DispatchDoneTracker::is_busy(j.tempo.as_deref(), j.state.as_deref())
            });
        if grew {
            self.version_gate.note_roster_busy();
        }
        if tracker.observe(busy, Instant::now()) {
            let path = tracker.marker_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(path, b"") {
                Ok(()) => {
                    tracing::info!(
                        short = %tracker.short(),
                        path = %path.display(),
                        "dispatched session settled idle after work; wrote dispatch_done marker"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        %err,
                        path = %path.display(),
                        "failed to write dispatch_done marker"
                    );
                }
            }
        }
    }

    /// Whether a tailed event leaves the turn OPEN (`Some(true)`), closes it
    /// (`Some(false)`), or says nothing (`None`, e.g. token usage or the model
    /// id, which ride along with every assistant line).
    ///
    /// Only a plain assistant message closes a turn. A tool call is awaiting
    /// its result, a `tool_result` is awaiting the model, and a thinking block
    /// is the model mid-flight — each of them can sit silent for minutes.
    fn tail_verdict(evt: &AdapterEvent) -> Option<bool> {
        match evt {
            AdapterEvent::ToolUse { .. } => Some(true),
            AdapterEvent::Message { payload, .. } => {
                match payload.get("role").and_then(serde_json::Value::as_str) {
                    Some("assistant") => Some(false),
                    Some(_) => Some(true),
                    None => None,
                }
            }
            _ => None,
        }
    }

    /// Note a parent's `tool_result`: it closes the `Task` call that spawned
    /// the subagent carrying the same `toolUseId`. This is the authoritative
    /// end — it closes a finished subagent on the next poll instead of waiting
    /// out the idle window, and it closes one whose own tail stopped mid-turn.
    fn note_parent_tool_result(&mut self, evt: &AdapterEvent) {
        let AdapterEvent::ToolUse { payload, .. } = evt else { return };
        if payload.get("kind").and_then(serde_json::Value::as_str) != Some("tool_result") {
            return;
        }
        let Some(id) = payload.get("tool_use_id").and_then(serde_json::Value::as_str) else {
            return;
        };
        for st in self.subagents.values_mut() {
            if st.tool_use_id.as_deref() == Some(id) {
                st.parent_resolved = true;
            }
        }
    }

    /// Discover and tail Task-tool subagents for every live parent session.
    /// Each subagent transcript lives at
    /// `<encoded-cwd>/<parent-session-id>/subagents/agent-<agentId>.jsonl`
    /// and reuses the standard transcript parser. Subagents are observe-only
    /// (no worker `short` → no command dispatch); lifecycle end is inferred
    /// from transcript quiescence.
    async fn scan_subagents(&mut self, dirty_offsets: &mut bool) {
        // Snapshot parent locations to avoid borrowing `self` across the
        // `emit`/offset mutations below.
        let parents: Vec<(String, PathBuf, String)> = self
            .transcript_locations
            .values()
            .map(|loc| (loc.offset_key.clone(), loc.path.clone(), loc.cwd.clone()))
            .collect();

        for (parent_id, parent_path, cwd) in parents {
            let dir = transcript::subagents_dir(&parent_path);
            for entry in transcript::discover_subagents(&dir) {
                let transcript::SubagentEntry { agent_id, path, workflow, meta } = entry;
                if self.ended_subagents.contains(&agent_id) {
                    continue;
                }
                if !self.subagents.contains_key(&agent_id) {
                    self.subagents.insert(
                        agent_id.clone(),
                        SubagentState {
                            parent_local_id: parent_id.clone(),
                            idle_ticks: 0,
                            tool_use_id: meta.as_ref().and_then(|m| m.tool_use_id.clone()),
                            parent_resolved: false,
                            tail_pending: true,
                        },
                    );
                    self.emit(AdapterEvent::SessionStarted {
                        local_id: agent_id.clone(),
                        meta: SessionMeta {
                            working_dir: Some(cwd.clone()),
                            parent_local_id: Some(parent_id.clone()),
                            extra: Self::subagent_extra(
                                &agent_id,
                                workflow.as_ref(),
                                meta.as_ref(),
                            ),
                        },
                    })
                    .await;
                    if let Some(name) = meta.as_ref().and_then(|m| m.description.clone()) {
                        self.emit(Self::subagent_name_status(&agent_id, name)).await;
                    }
                }

                let off = self.offsets.get(&agent_id);
                match transcript::tail_once(&path, &agent_id, off) {
                    Ok((events, new_off)) => {
                        let grew = new_off != off;
                        // Read the tail's last meaningful event before
                        // `events` is consumed by the emit loop below.
                        let verdict = events.iter().rev().find_map(Self::tail_verdict);
                        if grew {
                            self.offsets.set(agent_id.clone(), new_off);
                            self.server_marks.insert(agent_id.clone(), new_off);
                            *dirty_offsets = true;
                        }
                        for evt in events {
                            self.emit(evt).await;
                        }
                        if grew {
                            self.emit(AdapterEvent::TranscriptMark {
                                local_id: agent_id.clone(),
                                offset: new_off,
                            })
                            .await;
                        }
                        if let Some(st) = self.subagents.get_mut(&agent_id) {
                            st.idle_ticks = if grew { 0 } else { st.idle_ticks + 1 };
                            if let Some(pending) = verdict {
                                st.tail_pending = pending;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(%err, path = %path.display(), "subagent tail failed");
                    }
                }
            }
        }

        // End of life, in order of trust: the parent's `Task` call returned;
        // or a transcript that reads finished has stayed quiet for the idle
        // window; or the stuck backstop fired. A tail that reads mid-turn is
        // work in flight and is never ended by quiescence alone — that is what
        // used to kill live subagents 30s after their last tool.
        let done: Vec<String> = self
            .subagents
            .iter()
            .filter(|(_, st)| {
                st.parent_resolved
                    || (st.idle_ticks >= SUBAGENT_IDLE_TICKS_TO_END && !st.tail_pending)
                    || st.idle_ticks >= SUBAGENT_STUCK_TICKS_TO_END
            })
            .map(|(id, _)| id.clone())
            .collect();
        for agent_id in done {
            self.subagents.remove(&agent_id);
            self.ended_subagents.insert(agent_id.clone());
            self.emit(AdapterEvent::SessionEnded {
                local_id: agent_id,
                reason: EndReason::Completed,
            })
            .await;
        }
    }

    /// End any still-tracked subagents whose parent has left the roster — a
    /// backstop for the quiescence heuristic so children never outlive their
    /// parent.
    async fn end_subagents_of(&mut self, parent_local_id: &str) {
        let orphans: Vec<String> = self
            .subagents
            .iter()
            .filter(|(_, st)| st.parent_local_id == parent_local_id)
            .map(|(id, _)| id.clone())
            .collect();
        for agent_id in orphans {
            self.subagents.remove(&agent_id);
            self.ended_subagents.insert(agent_id.clone());
            self.emit(AdapterEvent::SessionEnded {
                local_id: agent_id,
                reason: EndReason::Completed,
            })
            .await;
        }
    }

    /// Cheap periodic (and churn-`force`d) divergence check (replacing
    /// the unconditional re-tail). The forward tail keeps `server_marks`
    /// level with the persisted offset as it emits, so at idle every session is
    /// in sync and this emits nothing. Only a session whose persisted offset has
    /// run AHEAD of the mark we believe the server holds (`force` = a roster
    /// churn re-home) gets one bounded re-send window to heal the gap; the
    /// per-connect `ResumeMarks` handles the reconnect case.
    async fn reconcile_tail(&mut self, force: bool) {
        self.last_reconcile = Instant::now();
        let locations: Vec<TranscriptLocation> =
            self.transcript_locations.values().cloned().collect();
        for loc in locations {
            let local = self.offsets.get(&loc.offset_key);
            let server = self.server_marks.get(&loc.offset_key).copied().unwrap_or(0);
            if force || local > server {
                self.resend_window(&loc).await;
                self.server_marks.insert(loc.offset_key.clone(), local);
            }
        }
    }

    /// Path of a session's live transcript, following a worktree move. The
    /// launch-cwd path is authoritative while it exists (cheap, no scan); once
    /// `EnterWorktree` relocates the file the launch path vanishes, so fall back
    /// to the newest `<sess>.jsonl` found across project dirs.
    fn resolve_live_transcript(&self, cwd: &str, sess: &str) -> PathBuf {
        let launch = transcript::transcript_path(&self.cfg.projects_root, cwd, sess);
        if launch.exists() {
            return launch;
        }
        transcript::newest_transcript_for_session(&self.cfg.projects_root, sess).unwrap_or(launch)
    }

    /// One last forward tail on shutdown so the tail of the conversation (a
    /// final `tool_use` and its error) reaches the server before a dispatched
    /// pod is reaped. Best-effort: the claude daemon usually outlives us at
    /// teardown, but a failed poll must not stall the shutdown.
    async fn flush_before_teardown(&mut self) {
        if let Err(err) = self.poll_once().await {
            tracing::debug!(%err, "final teardown tail failed");
        }
        self.reconcile_tail(true).await;
        self.offsets.flush();
    }

    /// The offset to tail a session from, fast-forwarded to a server resume mark
    /// that sits AHEAD of our persisted offset — the cold-start /
    /// restart case where in-memory offsets are empty but the server already
    /// holds the transcript. Bounded by the file length so a stale mark past a
    /// truncated/rotated file can't skip live bytes. Persists the clamp so a
    /// later poll doesn't re-clamp.
    fn resume_offset(&mut self, key: &str, path: &Path) -> u64 {
        let local = self.offsets.get(key);
        if let Some(&mark) = self.server_marks.get(key)
            && mark > local
        {
            let bounded = clamp_to_file_len(path, mark);
            if bounded > local {
                self.offsets.set(key.to_owned(), bounded);
                return bounded;
            }
        }
        local
    }

    /// Apply server-pushed transcript resume marks: record each mark,
    /// clamp the cursor of any session already ahead-clampable forward, and heal
    /// a session we already tail whose offset has run ahead of (or has no) mark
    /// with a single bounded re-send window — the one-time heal that replaces the
    /// old periodic re-tail.
    async fn apply_resume_marks(&mut self, marks: Vec<(String, u64)>) {
        let mark_map: HashMap<String, u64> = marks.into_iter().collect();
        for (key, mark) in &mark_map {
            let entry = self.server_marks.entry(key.clone()).or_insert(0);
            *entry = (*entry).max(*mark);
        }
        let locations: Vec<TranscriptLocation> =
            self.transcript_locations.values().cloned().collect();
        let mut dirty = false;
        for loc in locations {
            let prev = self.offsets.get(&loc.offset_key);
            if self.resume_offset(&loc.offset_key, &loc.path) != prev {
                // Clamped forward: the server already has this, no re-send.
                dirty = true;
                continue;
            }
            let behind_or_absent = match mark_map.get(&loc.offset_key) {
                Some(&mark) => mark < prev,
                None => true,
            };
            if behind_or_absent && prev > 0 {
                self.resend_window(&loc).await;
                self.server_marks.insert(loc.offset_key.clone(), prev);
            }
        }
        if dirty {
            self.offsets.flush();
        }
    }

    /// Re-emit one bounded window BEHIND a session's persisted offset (the
    /// 64 KiB re-tail) to heal a gap, then surface our offset as a mark
    /// so the server's high-water mark catches up. The persisted offset is left
    /// untouched — this is a pure catch-up replay the server dedups.
    async fn resend_window(&self, loc: &TranscriptLocation) {
        let off = self.offsets.get(&loc.offset_key);
        match transcript::reconcile_tail(&loc.path, &loc.local_id, off) {
            Ok(events) => {
                for evt in events {
                    self.emit(evt).await;
                }
                self.emit(AdapterEvent::TranscriptMark {
                    local_id: loc.local_id.clone(),
                    offset: off,
                })
                .await;
            }
            Err(err) => {
                tracing::debug!(%err, path = %loc.path.display(), "resume-mark re-send failed");
            }
        }
    }

    async fn flush_roster(&mut self, reason: EndReason) {
        // The daemon/socket is gone — stop dialing it from every attach task.
        self.attach.cancel_all();
        let shorts: Vec<String> = self.roster.drain().collect();
        self.last_status.clear();
        // do NOT clear heal bookkeeping here. A flush fires when the
        // control socket is momentarily unreachable (on-demand daemon
        // idle-shutdown / kickstart race) — that is NOT evidence the workers
        // died; they stay alive and reappear on the next successful poll.
        // Forgetting their launched-with-env trust here made cctui mistake its
        // own live, account-bound sessions for env-less autonomous respawns and
        // force-kill them as soon as the socket returned (the observed kill
        // loop). Trust is dropped only when a worker genuinely leaves a
        // *successful* roster snapshot (`apply_snapshot`'s `gone` handling),
        // which a real death/respawn does and a socket blip does not.
        for short in shorts {
            self.clear_permission(&short).await;
            self.emit(AdapterEvent::SessionEnded { local_id: short, reason: reason.clone() }).await;
        }
    }

    /// Record `session_id → local_id` in the shared map the ask-hook listener
    /// reads. Lock poisoning is non-fatal here (the map is best-effort routing
    /// metadata), so we recover the guard rather than panic.
    fn map_session(&self, session_id: &str, local_id: &str) {
        let mut guard =
            self.session_to_local.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(session_id.to_owned(), local_id.to_owned());
    }

    /// Reconcile the pending tool-permission prompt for one worker against the
    /// live snapshot. A `needs` of `"approve <Tool>: <detail>"` (set
    /// while the worker is `tempo:"blocked"`) is a permission prompt; a fresh or
    /// changed one emits `PermissionRequest`, and clearing it emits
    /// `PermissionResolved`. Deduped so an unchanged prompt isn't re-emitted on
    /// every 2s poll.
    async fn reconcile_permission(
        &mut self,
        short: &str,
        local_id: &str,
        tempo: Option<&str>,
        needs: Option<&str>,
    ) {
        let pending_needs = match needs.map(str::trim) {
            // `tempo:"blocked"` + an `approve …` need is the interactive
            // tool-permission prompt. Other `needs`/blocked states (e.g. a
            // background "needs input") are not permission prompts.
            Some(n) if tempo == Some("blocked") && n.starts_with("approve ") => Some(n.to_owned()),
            _ => None,
        };

        match pending_needs {
            Some(n) => {
                // Already surfaced this exact prompt? Nothing to do.
                if self.pending_perms.get(short).is_some_and(|p| p.needs == n) {
                    return;
                }
                // A changed `needs` means the prior prompt was superseded —
                // resolve it before emitting the new one so no stale card lingers.
                if let Some(prev) = self.pending_perms.remove(short) {
                    self.emit(AdapterEvent::PermissionResolved {
                        local_id: prev.local_id,
                        request_id: prev.request_id,
                    })
                    .await;
                }
                self.perm_seq += 1;
                let request_id = format!("{short}#perm{}", self.perm_seq);
                let (tool, description) = parse_permission_needs(&n);
                self.pending_perms.insert(
                    short.to_owned(),
                    PendingPerm {
                        request_id: request_id.clone(),
                        local_id: local_id.to_owned(),
                        needs: n.clone(),
                    },
                );
                self.emit(AdapterEvent::PermissionRequest {
                    local_id: local_id.to_owned(),
                    request_id,
                    tool,
                    input: json!({ "description": description, "needs": n }),
                })
                .await;
            }
            None => {
                if let Some(prev) = self.pending_perms.remove(short) {
                    self.emit(AdapterEvent::PermissionResolved {
                        local_id: prev.local_id,
                        request_id: prev.request_id,
                    })
                    .await;
                }
            }
        }
    }

    /// Drop any pending permission for a worker that left the roster, emitting a
    /// `PermissionResolved` so clients dismiss a prompt whose session is gone.
    async fn clear_permission(&mut self, short: &str) {
        if let Some(prev) = self.pending_perms.remove(short) {
            self.emit(AdapterEvent::PermissionResolved {
                local_id: prev.local_id,
                request_id: prev.request_id,
            })
            .await;
        }
    }

    async fn emit(&self, evt: AdapterEvent) {
        let _ = self.events.send(self.stamp_turn(evt)).await;
    }

    /// Give a user event the id of the turn cctui injected, when one is still
    /// in flight. Only `role:"user"` events are stamped: the several encodings
    /// Claude stores one turn in must share a key, while two assistant messages
    /// in the same turn must not.
    fn stamp_turn(&self, mut evt: AdapterEvent) -> AdapterEvent {
        if let AdapterEvent::Message { local_id, payload, turn_id } = &mut evt
            && turn_id.is_none()
            && payload.get("role").and_then(serde_json::Value::as_str) == Some("user")
            && let Some(id) = self.turn_for(local_id)
        {
            *turn_id = Some(id);
        }
        evt
    }

    fn note_turn(&self, local_id: &str, turn_id: Option<uuid::Uuid>) {
        let Ok(mut map) = self.pending_turns.lock() else { return };
        match turn_id {
            Some(id) => {
                map.insert(local_id.to_owned(), PendingTurn { id, at: Instant::now() });
            }
            None => {
                map.remove(local_id);
            }
        }
    }

    fn turn_for(&self, local_id: &str) -> Option<uuid::Uuid> {
        let mut map = self.pending_turns.lock().ok()?;
        let pending = *map.get(local_id)?;
        let live = pending.at.elapsed() <= TURN_ID_WINDOW;
        if !live {
            map.remove(local_id);
        }
        drop(map);
        live.then_some(pending.id)
    }
}

/// A server resume mark bounded by the transcript's current length: a
/// mark past EOF (stale after a `/clear` truncation or rotation) must never seek
/// beyond live bytes. A missing/unreadable file yields 0 so the tail restarts
/// from the top rather than trusting the mark.
fn clamp_to_file_len(path: &Path, mark: u64) -> u64 {
    std::fs::metadata(path).map_or(0, |m| mark.min(m.len()))
}

/// Parse a permission `needs` string (`"approve <Tool>: <detail>"`) into a
/// `(tool, description)` pair. Falls back to the whole remainder as both tool
/// and description when there's no `": "` separator.
fn parse_permission_needs(needs: &str) -> (String, String) {
    let rest = needs.strip_prefix("approve ").unwrap_or(needs).trim();
    match rest.split_once(": ") {
        Some((tool, detail)) => (tool.trim().to_owned(), detail.trim().to_owned()),
        None => (rest.to_owned(), rest.to_owned()),
    }
}

/// Path of the managed hook settings file: `$XDG_CONFIG_HOME/cctui/
/// ask-hook-settings.json` (falling back to `~/.config`).
/// Map a numeric kill signal to the string name Claude's control-socket `kill`
/// op accepts. The op validates `signal` against the zod enum
/// `["SIGTERM","SIGKILL"]`, so a numeric value is rejected outright.
/// Only `SIGKILL` (9) maps to a hard kill; everything else (notably the
/// interrupt route's `15`) maps to the graceful `SIGTERM`.
const fn kill_signal_name(signal: i32) -> &'static str {
    if signal == 9 { "SIGKILL" } else { "SIGTERM" }
}

/// Decode + stage `bootstrap` file uploads under
/// `/tmp/cctui-uploads/<session-id>/`, returning their absolute paths in upload
/// order. Files are written 0600 with sanitized bare names; an empty/null
/// bootstrap yields an empty vec. Errors (bad base64, unwritable dir) abort the
/// spawn so the user learns the attachment didn't land rather than the worker
/// silently starting without it.
/// Build the spawn-time `<session-context>` block prepended to the
/// initial prompt. Mirrors what a human sees on the session card — name,
/// model·effort, permission posture, env var NAMES, cwd, and staged files.
/// Env var VALUES are never included (only `spec.env` keys, sorted by the
/// `BTreeMap`). Empty fields are omitted so the block stays tight.
fn build_session_context(
    spec: &cctui_proto::adapter::SessionSpec,
    cwd: &str,
    staged: &[String],
    capability: Option<&cctui_proto::api::SpawnCapability>,
) -> String {
    let mut b = String::from("<session-context>\n");
    if let Some(name) = spec.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        let _ = writeln!(b, "session: {name}");
    }
    if let Some(model) = spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        match spec.effort.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            Some(effort) => {
                let _ = writeln!(b, "model: {model} · effort: {effort}");
            }
            None => {
                let _ = writeln!(b, "model: {model}");
            }
        }
    }
    if let Some(mode) = spec.permission_mode {
        let _ = writeln!(b, "permission-mode: {}", mode.normalized_label());
    }
    let _ = writeln!(b, "cwd: {cwd}");
    if !spec.env.is_empty() {
        let names = spec.env.keys().cloned().collect::<Vec<_>>().join(", ");
        let _ = writeln!(b, "env (names only): {names}");
    }
    if !staged.is_empty() {
        b.push_str("attached files:\n");
        for p in staged {
            let _ = writeln!(b, "  - {p}");
        }
    }
    if let Some(cap) = capability.filter(|c| !c.is_empty()) {
        b.push_str(&agent_tool_context(cap));
    }
    b.push_str("</session-context>");
    b
}

/// The `CctuiAgent` paragraph of the session context: the tool exists, which
/// adapters this session may spawn, and one worked call.
fn agent_tool_context(cap: &cctui_proto::api::SpawnCapability) -> String {
    let mut b = String::from(
        "CctuiAgent: you can delegate work to cctui subagent sessions on this \
         machine with the MCP tool `mcp__cctui__CctuiAgent`. Each child is a real \
         cctui session — nested under this one in the UI, metered, killable. The \
         call follows the child (progress streams back while it works) and \
         returns its final message when its turn completes. Parallel calls run \
         in parallel. To send a follow-up to a child, call again with its \
         session_id (included in the reply) and a new prompt.\n",
    );
    let _ = writeln!(b, "  adapters you may spawn: {}", cap.adapters.join(", "));
    if let Some(max) = cap.max_budget_usd {
        let _ = writeln!(b, "  per-child budget ceiling: ${max} (inherited when you name none)");
    }
    if let Some(max) = cap.max_children {
        let _ = writeln!(b, "  max children for this session: {max}");
    }
    let adapter = cap.adapters.first().map_or("claude-code", String::as_str);
    let _ = writeln!(
        b,
        "  example: mcp__cctui__CctuiAgent({{\"adapter\": \"{adapter}\", \"prompt\": \
         \"Review the diff on branch X and list real defects\", \"cwd\": \"/path/to/repo\"}})"
    );
    b.push_str(
        "CctuiUsage: `mcp__cctui__CctuiUsage` reports the rate limits and budget that apply to \
         THIS session — the account it is pinned to (possibly shared or pool-elected, not \
         necessarily your own), its usage windows, this session's spend, and whether each model \
         is currently allowed or soft-limit blocked. Check it before a fan-out and when picking \
         a child's model: a blocked model burns the whole batch on 429s. Takes no arguments.\n",
    );
    b
}

fn stage_uploads(session_id: &str, bootstrap: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    crate::adapters::uploads::stage_bootstrap(session_id, bootstrap)
}

/// Public entry point for mid-chat attachment staging. Thin wrapper
/// over [`crate::adapters::uploads::stage_files`] so the supervisor can stage
/// without reaching into control internals.
pub fn stage_mid_chat_files(
    session_id: &str,
    uploads: &[cctui_proto::adapter::BootstrapFile],
) -> anyhow::Result<Vec<String>> {
    crate::adapters::uploads::stage_files(session_id, uploads)
}

/// Recover a session's whip posture from the per-session settings file the
/// original spawn wrote for `short`. The whip profile is the only one
/// that emits a top-level `hooks.Stop` block (the `whip-stop-hook`), so its
/// presence is a reliable discriminator. Used by cold resume, which has no
/// `spec` to read `permission_mode` from. Absent/unreadable file → not whip.
fn detect_whip_from_settings(short: &str) -> bool {
    let Some(path) = hook_settings_path(&format!("hook-settings-{short}.json")) else {
        return false;
    };
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("hooks").and_then(|h| h.get("Stop")).cloned())
        .is_some()
}

fn hook_settings_path(file: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("cctui").join(file))
}

/// Write the per-session whip phrase override file the `whip-stop-hook`
/// reads via `--phrases`, returning its path. `None` (unwritable) → the caller
/// launches the hook without the arg, so it uses its compiled defaults.
fn write_whip_phrases(short: &str, block: &serde_json::Value) -> Option<PathBuf> {
    let path = hook_settings_path(&format!("whip-phrases-{short}.json"))?;
    if let Some(Err(err)) = path.parent().map(std::fs::create_dir_all) {
        tracing::warn!(%err, "whip-stop: cannot create phrases dir");
        return None;
    }
    match std::fs::write(&path, serde_json::to_vec_pretty(block).ok()?) {
        Ok(()) => Some(path),
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "whip-stop: cannot write phrases");
            None
        }
    }
}

/// Delete a stale whip phrase file for `short` so a spawn after the user cleared
/// the override falls back to the compiled defaults. Best-effort.
fn remove_whip_phrases(short: &str) {
    if let Some(path) = hook_settings_path(&format!("whip-phrases-{short}.json")) {
        let _ = std::fs::remove_file(path);
    }
}

/// Recursively deep-merge `overlay` into `base`, with `overlay` winning at every
/// level. Object nodes are merged key-by-key (recursing on shared
/// keys); every other node kind (scalars, arrays) is replaced wholesale by the
/// overlay value. Keys present only in `base` are preserved.
///
/// The daemon uses this to layer its load-bearing managed settings (the ask /
/// permission / Stop hooks) as the `overlay` OVER server-provided per-account
/// settings (the `base`) — so account settings can add keys but can never
/// clobber a managed key. See [`ensure_hook_settings`].
pub(super) fn deep_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, ov) in o {
                match b.get_mut(k) {
                    Some(bv) => deep_merge(bv, ov),
                    None => {
                        b.insert(k.clone(), ov.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Produce the final `--settings` document by deep-merging the server-provided
/// per-account `settings` UNDERNEATH the daemon's `managed` settings.
///
/// Managed values win at every level: we start from a clone of the account
/// settings (an object; anything non-object is discarded as malformed) and
/// overlay the managed settings on top via [`deep_merge`]. An account blob that
/// tries to set its own `hooks` therefore loses to the managed `hooks` block —
/// the ask/permission/Stop hooks survive intact.
pub(super) fn merge_account_under_managed(
    managed: serde_json::Value,
    account: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut merged = match account {
        Some(a @ serde_json::Value::Object(_)) => a.clone(),
        // No account settings, or a non-object blob we can't safely merge under:
        // fall back to managed-only, exactly as before.
        _ => return managed,
    };
    deep_merge(&mut merged, &managed);
    merged
}

/// Write (idempotently, on every spawn so it tracks binary upgrades) the
/// managed Claude Code settings file that registers the `AskUserQuestion`
/// PreToolUse/PostToolUse hooks, pointing at this daemon binary and the given
/// delivery socket. Returns the file path to inject via `--settings`,
/// or `None` if we can't locate the binary / config dir (in which case spawning
/// proceeds without the hook rather than failing).
///
/// `whip` toggles the 🐎 enforcement profile: the `AskUserQuestion`
/// `PreToolUse` hook gains `--deny` (it still notifies the UI, but returns a
/// `deny` decision so the form never renders), and a `Stop` hook
/// (`whip-stop-hook`) blocks stalling / hand-back language so the worker runs to
/// genuine completion.
///
/// The file is written to a PER-SESSION path (keyed by `short`) so different
/// sessions — potentially bound to different accounts with different
/// `account_settings` — never clobber each other's `--settings` file.
///
/// `account_settings` is the server-provided, per-account
/// `settings_json` that rode the gateway-env pull. It is deep-merged UNDERNEATH
/// the managed settings: account keys are layered in, but the managed `hooks`
/// block (and any other key the daemon sets) ALWAYS WINS — a malicious or
/// stale account blob that specifies its own `hooks` can never disable the
/// ask/permission/Stop hooks. `None` → managed settings only, exactly as before.
#[allow(clippy::cognitive_complexity, clippy::too_many_arguments)]
pub(super) fn ensure_hook_settings(
    sock: &std::path::Path,
    whip: bool,
    short: &str,
    account_settings: Option<&serde_json::Value>,
    gateway_env: &std::collections::BTreeMap<String, String>,
    model: Option<&str>,
    effort: Option<&str>,
    whip_phrases: Option<&serde_json::Value>,
) -> Option<PathBuf> {
    let path = hook_settings_path(&format!("hook-settings-{short}.json"))?;
    let exe = std::env::current_exe()
        .map_err(|err| tracing::warn!(%err, "ask-hook: cannot resolve current_exe"))
        .ok()?;
    let exe = exe.to_string_lossy();
    let sock = sock.to_string_lossy();
    let deny = if whip { " --deny" } else { "" };
    let hook = |event: &str| {
        let extra = if event == "pre" { deny } else { "" };
        json!({
            // AskUserQuestion + ExitPlanMode both fire this hook: the former
            // surfaces a live question card, the latter a live Plan card.
            // Both are single-select PTY prompts answered the same
            // way (digit keystroke / dismiss-then-reply).
            "matcher": "AskUserQuestion|ExitPlanMode",
            "hooks": [{
                "type": "command",
                "command": format!("{exe} ask-hook --event {event} --sock {sock}{extra}"),
                "timeout": 5,
            }],
        })
    };
    // Bidirectional tool-permission hook. Scoped to the mutating /
    // executing tools that actually trigger an interactive approval (read-only
    // tools auto-allow, so blocking on them would needlessly stall the turn).
    // Distinct from the `AskUserQuestion` matcher above so both PreToolUse hooks
    // can coexist. The hook BLOCKS, long-polls the daemon for the human's
    // decision, and returns an allow/deny `permissionDecision` — no keystroke in
    // the common case. The `timeout` is deliberately high (the daemon resolves
    // the hook with a `defer` well before this fires, on its own bounded wait);
    // a hook that overran *its* timeout would be treated by Claude Code as a
    // hard deny, which we must never do to a slow human.
    let perm_matcher = "Bash|Edit|MultiEdit|Write|NotebookEdit|WebFetch|Task|KillShell|BashOutput";
    let perm_hook = json!({
        "matcher": perm_matcher,
        "hooks": [{
            "type": "command",
            "command": format!("{exe} ask-hook --event perm --sock {sock}"),
            "timeout": 600,
        }],
    });
    // EnterPlanMode guard. Registered UNCONDITIONALLY of the whip
    // flag: the deny is decided at runtime from the payload's live
    // `permission_mode` (see `enter_plan_mode_decision`), so it must ride the
    // pre event for both Yolo and Whip; `deny` here only sets the posture label.
    let plan_guard = json!({
        "matcher": "EnterPlanMode",
        "hooks": [{
            "type": "command",
            "command": format!("{exe} ask-hook --event pre --sock {sock}{deny}"),
            "timeout": 5,
        }],
    });
    let pre_hooks = json!([hook("pre"), perm_hook, plan_guard]);
    // The whip Stop hook gets the user's phrase override via a
    // per-session file it reads with `--phrases`; absent/cleared → the hook falls
    // back to its compiled defaults, so a stale file from a prior spawn is removed.
    let whip_stop_command = if whip {
        let arg = whip_phrases.filter(|v| !v.is_null()).map_or_else(
            || {
                remove_whip_phrases(short);
                String::new()
            },
            |v| {
                write_whip_phrases(short, v)
                    .map(|p| format!(" --phrases {}", p.to_string_lossy()))
                    .unwrap_or_default()
            },
        );
        format!("{exe} whip-stop-hook{arg}")
    } else {
        String::new()
    };
    let hooks = if whip {
        json!({
            "PreToolUse": pre_hooks,
            "PostToolUse": [hook("post")],
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": whip_stop_command,
                    "timeout": 10,
                }],
            }],
        })
    } else {
        json!({ "PreToolUse": pre_hooks, "PostToolUse": [hook("post")] })
    };
    let managed = managed_settings(hooks, gateway_env, model, effort);
    // Layer the server-provided per-account settings UNDERNEATH the managed
    // settings: account keys are merged in, but the managed keys
    // (hooks, gateway env, model/effort) always win so they can never be
    // clobbered.
    let settings = merge_account_under_managed(managed, account_settings);
    if let Some(Err(err)) = path.parent().map(std::fs::create_dir_all) {
        tracing::warn!(%err, "ask-hook: cannot create settings dir");
        return None;
    }
    if let Err(err) = std::fs::write(&path, serde_json::to_vec_pretty(&settings).ok()?) {
        tracing::warn!(%err, path = %path.display(), "ask-hook: cannot write settings");
        return None;
    }
    // The file now carries the gateway bearer token — restrict it to
    // owner-only so the secret isn't world-readable on disk.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(%err, path = %path.display(), "ask-hook: cannot chmod settings 0600");
        }
    }
    Some(path)
}

/// Write the per-session MCP config registering the `CctuiAgent` tool, and
/// return the path to inject as `--mcp-config`.
///
/// `None` — no tool — whenever the session has no spawn capability, so a session
/// the server never granted spawn rights cannot even see the tool. The config is
/// keyed by `short` like the hook settings so sessions never clobber each other.
pub(super) fn ensure_agent_mcp_config(
    short: &str,
    session_id: &str,
    capability: Option<&cctui_proto::api::SpawnCapability>,
) -> Option<PathBuf> {
    if capability.is_none_or(cctui_proto::api::SpawnCapability::is_empty) {
        return None;
    }
    let path = hook_settings_path(&format!("mcp-agent-{short}.json"))?;
    let exe = std::env::current_exe()
        .map_err(|err| tracing::warn!(%err, "CctuiAgent: cannot resolve current_exe"))
        .ok()?;
    let config = crate::mcp::mcp_config(
        &exe.to_string_lossy(),
        session_id,
        crate::agenttool::socket_for_launch(),
    );
    if let Some(Err(err)) = path.parent().map(std::fs::create_dir_all) {
        tracing::warn!(%err, "CctuiAgent: cannot create mcp config dir");
        return None;
    }
    if let Err(err) = std::fs::write(&path, serde_json::to_vec_pretty(&config).ok()?) {
        tracing::warn!(%err, path = %path.display(), "CctuiAgent: cannot write mcp config");
        return None;
    }
    Some(path)
}

/// Build the managed `--settings` document: the ask/permission/Stop
/// `hooks`, the gateway routing `env`, and the session `model`/`effortLevel`,
/// all in one file. The claude daemon applies a session's `--settings` to a
/// spare-claimed worker but deliberately does NOT reapply the dispatch `env`, so
/// carrying the gateway env HERE is the only channel that survives the
/// spare-claim on every platform (replacing the Linux-`/proc`-only gateway-env
/// heal). Split out from [`ensure_hook_settings`] so the shape is unit-testable
/// without touching the filesystem.
fn managed_settings(
    hooks: serde_json::Value,
    gateway_env: &std::collections::BTreeMap<String, String>,
    model: Option<&str>,
    effort: Option<&str>,
) -> serde_json::Value {
    let mut managed = serde_json::Map::new();
    managed.insert("hooks".to_owned(), hooks);
    if let Some(m) = model.map(str::trim).filter(|m| !m.is_empty()) {
        managed.insert("model".to_owned(), json!(m));
    }
    let mut env_obj: serde_json::Map<String, serde_json::Value> =
        gateway_env.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    // `effortLevel` only accepts low|medium|high|xhigh; `max`/`ultracode` are
    // session-only and rejected in a settings file, so they ride the
    // `CLAUDE_CODE_EFFORT_LEVEL` env var instead (which accepts them).
    if let Some(e) = effort.map(str::trim).filter(|e| !e.is_empty()) {
        if matches!(e, "low" | "medium" | "high" | "xhigh") {
            managed.insert("effortLevel".to_owned(), json!(e));
        } else {
            env_obj.insert("CLAUDE_CODE_EFFORT_LEVEL".to_owned(), json!(e));
        }
    }
    if !env_obj.is_empty() {
        managed.insert("env".to_owned(), serde_json::Value::Object(env_obj));
    }
    serde_json::Value::Object(managed)
}

/// Translate a structured ask answer into the keystroke chunks that drive the
/// real `AskUserQuestion` form. `questions` is the raw
/// `tool_input.questions` array captured by the ask-hook; `picks` is one list
/// of 0-based option indices per question, in question order.
///
/// Form grammar (verified live against claude 2.1.162):
///   - single-select: the option digit (`1`-`9`) selects and auto-advances
///   - multiSelect: digits toggle options; `Tab` advances to the next question
///   - every form except a lone single-select question ends on a "Review your
///     answers" screen whose option 1 is "Submit answers" → final `1` submits
///
/// Returns `None` when the answer can't be expressed as form keystrokes
/// (count mismatch, out-of-range/duplicate picks, empty pick on a question,
/// several picks on a single-select) — the caller then falls back to the
/// dismiss-then-reply path, which handles free-text answers too.
fn ask_keystrokes(questions: &serde_json::Value, picks: &[Vec<usize>]) -> Option<Vec<Vec<u8>>> {
    let qs = questions.as_array()?;
    if qs.is_empty() || qs.len() != picks.len() {
        return None;
    }
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut any_multi = false;
    for (q, p) in qs.iter().zip(picks) {
        let n_opts = q.get("options").and_then(serde_json::Value::as_array)?.len();
        // Digits only address rows 1-9; real forms have ≤4 options, so >9 means
        // we're misreading the payload — bail to the fallback.
        if n_opts == 0 || n_opts > 9 || p.is_empty() || p.iter().any(|&i| i >= n_opts) {
            return None;
        }
        if q.get("multiSelect").and_then(serde_json::Value::as_bool).unwrap_or(false) {
            any_multi = true;
            let mut sorted = p.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() != p.len() {
                return None; // duplicate picks — toggling twice would deselect
            }
            for &i in &sorted {
                chunks.push(vec![b'1' + u8::try_from(i).ok()?]);
            }
            chunks.push(vec![b'\t']); // advance to the next question / review
        } else {
            if p.len() != 1 {
                return None;
            }
            chunks.push(vec![b'1' + u8::try_from(p[0]).ok()?]);
        }
    }
    // The review screen ("1. Submit answers") shows for every form except a
    // lone single-select question, which submits straight from its digit.
    if qs.len() > 1 || any_multi {
        chunks.push(vec![b'1']);
    }
    Some(chunks)
}

#[derive(Debug, PartialEq, Eq)]
enum ClaudeRmOutcome {
    Removed,
    AlreadyGone,
    Refused(String),
}

/// The CLI explains a refusal on **stdout** (`kept <short> — worktree is the
/// working directory of a live session (pid …)`, then what to do about it)
/// and reserves stderr for errors (`No job matching`, `couldn't remove …
/// EACCES`). Both go into the detail, whitespace collapsed, so the operator
/// reads the same message in the daemon log as at the prompt: a bare
/// `exit 1` cost hours of guesswork on a worktree held by a live session.
fn classify_claude_rm(
    code: Option<i32>,
    success: bool,
    stdout: &str,
    stderr: &str,
) -> ClaudeRmOutcome {
    if success {
        return ClaudeRmOutcome::Removed;
    }
    if stderr.contains("No job matching") {
        return ClaudeRmOutcome::AlreadyGone;
    }
    let code = code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
    let output =
        [stderr, stdout].iter().flat_map(|s| s.split_whitespace()).collect::<Vec<_>>().join(" ");
    let detail =
        if output.is_empty() { format!("exit {code}") } else { format!("exit {code}: {output}") };
    ClaudeRmOutcome::Refused(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    fn deferred(sock: PathBuf) -> DeferredDispatch {
        DeferredDispatch {
            sock,
            req: serde_json::json!({ "proto": 1, "op": "dispatch" }),
            short: format!("t-{}", uuid::Uuid::new_v4()),
            what: "spawn in /tmp".to_owned(),
            session_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    #[tokio::test]
    async fn a_detached_dispatch_to_a_dead_daemon_reports_a_failed_command_result() {
        let (tx, mut rx) = mpsc::channel(4);
        let command_id = uuid::Uuid::new_v4();
        deferred(std::env::temp_dir().join(format!("absent-{}.sock", uuid::Uuid::new_v4())))
            .run_detached(tx, Some(command_id));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a dead daemon must not hang the command")
            .expect("an outcome");
        match ev {
            AdapterEvent::CommandResult { command_id: got, ok, error } => {
                assert_eq!(got, command_id);
                assert!(!ok);
                assert!(error.is_some_and(|e| e.contains("spawn in /tmp")));
            }
            other => panic!("expected a CommandResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_detached_dispatch_reports_ok_once_the_daemon_answers() {
        let dir = std::env::temp_dir().join(format!("cctui-dd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("d.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(r), &mut line)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut w, b"{\"ok\":true}\n").await.unwrap();
        });
        let (tx, mut rx) = mpsc::channel(4);
        let command_id = uuid::Uuid::new_v4();
        deferred(sock).run_detached(tx, Some(command_id));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("an outcome within the bound")
            .expect("an outcome");
        assert!(matches!(ev, AdapterEvent::CommandResult { ok: true, .. }), "{ev:?}");
    }

    #[test]
    fn dispatch_spec_built_from_payload_with_inline_prompt() {
        // the dispatcher injects prompt/model/effort/env inside
        // TASK_PAYLOAD_JSON; the daemon turns it into a headless SessionSpec.
        let payload = serde_json::json!({
            "prompt": "do the thing",
            "model": "opus",
            "effort": "low",
            "repo": "acme",
            "name": "triage-PROJ",
            "env": { "ANTHROPIC_BASE_URL": "https://x/gateway/anthropic", "ANTHROPIC_AUTH_TOKEN": "cctui_s_x" },
        });
        let spec = Driver::build_dispatch_spec(&payload).expect("spec");
        assert_eq!(spec.prompt.as_deref(), Some("do the thing"));
        assert_eq!(spec.model.as_deref(), Some("opus"));
        assert_eq!(spec.effort.as_deref(), Some("low"));
        assert_eq!(spec.name.as_deref(), Some("triage-PROJ"));
        assert_eq!(spec.adapter_id.0, "claude-code");
        assert!(matches!(spec.permission_mode, Some(cctui_proto::adapter::PermissionMode::Yolo)));
        assert_eq!(spec.env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str), Some("cctui_s_x"));
    }

    #[test]
    fn dispatch_prompt_reads_absolute_file() {
        let dir = std::env::temp_dir().join(format!("cctui-disp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.md");
        std::fs::write(&f, "PROMPT BODY").unwrap();
        let payload = serde_json::json!({ "prompt_file": f.to_str().unwrap() });
        assert_eq!(Driver::resolve_dispatch_prompt(&payload).unwrap(), "PROMPT BODY");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dispatch_prompt_errors_when_neither_present() {
        let payload = serde_json::json!({ "model": "opus" });
        assert!(Driver::resolve_dispatch_prompt(&payload).is_err());
    }

    #[test]
    fn no_capability_means_no_mcp_config_and_so_no_tool() {
        assert!(ensure_agent_mcp_config("aaaaaaa1", "sess-1", None).is_none());
        let empty = cctui_proto::api::SpawnCapability::default();
        assert!(
            ensure_agent_mcp_config("aaaaaaa1", "sess-1", Some(&empty)).is_none(),
            "an empty adapter list grants nothing, so the tool must not be registered"
        );
    }

    #[test]
    fn a_capability_writes_a_session_scoped_mcp_config() {
        let cap = cctui_proto::api::SpawnCapability {
            adapters: vec!["opencode".to_owned()],
            max_budget_usd: Some(1.0),
            max_children: Some(2),
        };
        let short = format!("{:08x}", std::process::id());
        let Some(path) = ensure_agent_mcp_config(&short, "sess-42", Some(&cap)) else {
            return; // no writable config dir in this environment
        };
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let args = written["mcpServers"]["cctui"]["args"].as_array().unwrap().clone();
        assert!(args.contains(&json!("mcp-agent")));
        assert!(args.contains(&json!("sess-42")), "the session id is fixed in argv");
        assert!(path.to_string_lossy().contains(&short), "config must be per-session");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn managed_settings_carries_gateway_env_model_and_effort() {
        // gateway env + model + effort ride the `--settings` file so
        // they survive the claude-daemon spare-claim (which drops the dispatch
        // env). Enum efforts use the `effortLevel` key; `max` falls back to the
        // CLAUDE_CODE_EFFORT_LEVEL env var (the settings key rejects it).
        let mut env = std::collections::BTreeMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://x/gateway/anthropic".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "cctui_s_tok".to_owned());

        let s = managed_settings(
            json!({ "PreToolUse": [] }),
            &env,
            Some("claude-fable-5[1m]"),
            Some("medium"),
        );
        assert_eq!(s["model"], json!("claude-fable-5[1m]"));
        assert_eq!(s["effortLevel"], json!("medium"));
        assert_eq!(s["env"]["ANTHROPIC_BASE_URL"], json!("https://x/gateway/anthropic"));
        assert_eq!(s["env"]["ANTHROPIC_AUTH_TOKEN"], json!("cctui_s_tok"));
        assert!(s["hooks"].is_object());

        // `max` is not a valid `effortLevel`; it rides the env var instead.
        let s = managed_settings(json!({}), &env, None, Some("max"));
        assert!(s.get("effortLevel").is_none());
        assert_eq!(s["env"]["CLAUDE_CODE_EFFORT_LEVEL"], json!("max"));

        // No env / model / effort → only hooks, no empty `env` object.
        let s = managed_settings(json!({}), &std::collections::BTreeMap::new(), None, None);
        assert!(s.get("env").is_none());
        assert!(s.get("model").is_none());
    }

    /// The managed settings shape produced by `ensure_hook_settings` for a
    /// non-whip session (hooks the ask form + permission flow depend on). Kept
    /// in the test so the merge assertions below pin the load-bearing keys
    /// without dragging in `current_exe`/socket resolution.
    fn managed_ask_settings() -> serde_json::Value {
        json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "AskUserQuestion|ExitPlanMode", "hooks": [{ "type": "command", "command": "cctui ask-hook --event pre" }] },
                    { "matcher": "Bash|Edit", "hooks": [{ "type": "command", "command": "cctui ask-hook --event perm" }] },
                ],
                "PostToolUse": [{ "matcher": "AskUserQuestion|ExitPlanMode", "hooks": [{ "type": "command", "command": "cctui ask-hook --event post" }] }],
            },
        })
    }

    #[test]
    fn deep_merge_overlay_wins_and_recurses() {
        // overlay wins at every level; base-only keys are preserved;
        // nested objects merge key-by-key rather than replacing wholesale.
        let mut base = json!({
            "a": 1,
            "keep": "me",
            "nested": { "x": "base-x", "base_only": true },
        });
        let overlay = json!({
            "a": 2,
            "nested": { "x": "overlay-x", "y": "overlay-y" },
        });
        deep_merge(&mut base, &overlay);
        assert_eq!(base["a"], json!(2), "overlay scalar wins");
        assert_eq!(base["keep"], json!("me"), "base-only top key preserved");
        assert_eq!(base["nested"]["x"], json!("overlay-x"), "overlay wins nested");
        assert_eq!(base["nested"]["base_only"], json!(true), "base-only nested key preserved");
        assert_eq!(base["nested"]["y"], json!("overlay-y"), "overlay-only nested key added");
    }

    #[test]
    fn account_settings_cannot_clobber_managed_hooks() {
        // (a) An account blob that specifies its OWN hooks must lose to the
        // managed hooks entirely — the ask/permission hooks survive intact.
        let managed = managed_ask_settings();
        let account = json!({
            "hooks": { "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "evil --disable" }] }] },
            "env": { "MY_ACCOUNT_VAR": "1" },
            "permissions": { "allow": ["Bash(ls:*)"] },
        });
        let merged = merge_account_under_managed(managed.clone(), Some(&account));
        // Managed hooks win wholesale (the malicious PreToolUse never appears).
        assert_eq!(merged["hooks"], managed["hooks"], "managed hooks always win");
        assert_eq!(
            merged["hooks"]["PreToolUse"].as_array().unwrap().len(),
            2,
            "both managed PreToolUse hooks (ask + perm) survive"
        );
        // (b) account NON-hook keys are merged in.
        assert_eq!(merged["env"]["MY_ACCOUNT_VAR"], json!("1"));
        assert_eq!(merged["permissions"]["allow"], json!(["Bash(ls:*)"]));
    }

    #[test]
    fn account_non_hook_keys_merge_and_nested_managed_survives() {
        // (c) A nested merge where the account also supplies `hooks.PostToolUse`
        // must not drop the managed `hooks.PreToolUse` sub-key.
        let managed = managed_ask_settings();
        let account = json!({
            "hooks": { "PostToolUse": [{ "matcher": "*", "hooks": [{ "command": "acct-post" }] }], "SessionStart": [{ "hooks": [] }] },
            "statusLine": { "type": "command", "command": "mystatus" },
        });
        let merged = merge_account_under_managed(managed.clone(), Some(&account));
        // Managed PreToolUse + PostToolUse both intact (managed wins on PostToolUse).
        assert_eq!(merged["hooks"]["PreToolUse"], managed["hooks"]["PreToolUse"]);
        assert_eq!(merged["hooks"]["PostToolUse"], managed["hooks"]["PostToolUse"]);
        // Account's brand-new hook event (no managed counterpart) is added.
        assert!(merged["hooks"]["SessionStart"].is_array());
        // Non-hook top-level account key merged in.
        assert_eq!(merged["statusLine"]["command"], json!("mystatus"));
    }

    #[test]
    fn account_env_block_reaches_settings_but_managed_env_wins() {
        // curated env vars persist in the account `settings_json.env`
        // block. That block must survive the merge under managed settings so it
        // reaches the worker's process env — but a managed gateway env key of the
        // same name always wins (routing can never be clobbered).
        let managed = managed_settings(
            managed_ask_settings()["hooks"].clone(),
            &env_of(&[("ANTHROPIC_BASE_URL", "https://x/gateway/anthropic")]),
            None,
            None,
        );
        let account = json!({
            "env": { "DISABLE_TELEMETRY": "1", "ANTHROPIC_BASE_URL": "https://evil" },
        });
        let merged = merge_account_under_managed(managed, Some(&account));
        // Account's own curated env var survives.
        assert_eq!(merged["env"]["DISABLE_TELEMETRY"], json!("1"));
        // Managed gateway env wins over an account attempt to override it.
        assert_eq!(merged["env"]["ANTHROPIC_BASE_URL"], json!("https://x/gateway/anthropic"));
    }

    #[test]
    fn no_account_settings_is_managed_only() {
        let managed = managed_ask_settings();
        assert_eq!(merge_account_under_managed(managed.clone(), None), managed);
        // A non-object account blob is treated as absent (never merged).
        assert_eq!(merge_account_under_managed(managed.clone(), Some(&json!("garbage"))), managed);
    }

    #[test]
    fn reseed_interval_defaults_and_honors_override() {
        assert_eq!(reseed_interval_from(None), Duration::from_hours(1));
        assert_eq!(reseed_interval_from(Some("120".into())), Duration::from_mins(2));
        // Zero / garbage fall back to the hourly default rather than a hot loop.
        assert_eq!(reseed_interval_from(Some("0".into())), Duration::from_hours(1));
        assert_eq!(reseed_interval_from(Some("nope".into())), Duration::from_hours(1));
    }

    #[test]
    fn reseed_runs_on_first_pass_reattach_and_after_interval() {
        let interval = Duration::from_hours(1);
        // First pass (never re-seeded) always runs.
        assert!(reseed_due(None, interval, false));
        // A fresh pass is not due again until the interval elapses...
        assert!(!reseed_due(Some(Instant::now()), interval, false));
        // ...unless the daemon just (re)attached to the claude socket.
        assert!(reseed_due(Some(Instant::now()), interval, true));
        // Past the interval, the periodic renewal fires.
        let stale = Instant::now().checked_sub(Duration::from_secs(3601)).unwrap();
        assert!(reseed_due(Some(stale), interval, false));
    }

    #[test]
    fn ask_keystrokes_single_question_single_select() {
        // One single-select question: the digit submits directly, no review.
        let qs =
            json!([{ "question": "Red or blue?", "options": [{"label":"Red"},{"label":"Blue"}] }]);
        assert_eq!(ask_keystrokes(&qs, &[vec![1]]), Some(vec![b"2".to_vec()]));
    }

    #[test]
    fn ask_keystrokes_multiselect_tabs_then_submits() {
        // One multiSelect question: toggle digits, Tab to review, 1 submits.
        let qs = json!([{ "question": "Which?", "multiSelect": true,
            "options": [{"label":"A"},{"label":"B"},{"label":"C"}] }]);
        assert_eq!(
            ask_keystrokes(&qs, &[vec![2, 0]]), // unsorted on purpose
            Some(vec![b"1".to_vec(), b"3".to_vec(), b"\t".to_vec(), b"1".to_vec()])
        );
    }

    #[test]
    fn ask_keystrokes_multi_question_ends_on_review() {
        // multiSelect then single-select: toggles+Tab, digit, then review `1`.
        let qs = json!([
            { "question": "Fruits?", "multiSelect": true,
              "options": [{"label":"Apple"},{"label":"Banana"},{"label":"Cherry"}] },
            { "question": "Drink?", "options": [{"label":"Tea"},{"label":"Coffee"}] },
        ]);
        assert_eq!(
            ask_keystrokes(&qs, &[vec![0, 2], vec![1]]),
            Some(vec![b"1".to_vec(), b"3".to_vec(), b"\t".to_vec(), b"2".to_vec(), b"1".to_vec()])
        );
    }

    #[test]
    fn ask_keystrokes_rejects_unanswerable_shapes() {
        let qs = json!([{ "question": "Q", "options": [{"label":"A"},{"label":"B"}] }]);
        // count mismatch / empty pick / out of range / multi-pick on single-select
        assert_eq!(ask_keystrokes(&qs, &[]), None);
        assert_eq!(ask_keystrokes(&qs, &[vec![]]), None);
        assert_eq!(ask_keystrokes(&qs, &[vec![2]]), None);
        assert_eq!(ask_keystrokes(&qs, &[vec![0, 1]]), None);
        // duplicate toggles on multiSelect would cancel out
        let mq = json!([{ "question": "Q", "multiSelect": true,
            "options": [{"label":"A"},{"label":"B"}] }]);
        assert_eq!(ask_keystrokes(&mq, &[vec![0, 0]]), None);
        // not an array at all
        assert_eq!(ask_keystrokes(&json!({}), &[vec![0]]), None);
    }

    #[test]
    fn kill_signal_name_maps_to_claude_enum() {
        // The interrupt route sends 15; kill_session sends None (handled at the
        // call site). Anything that is not SIGKILL must map to SIGTERM so it
        // satisfies claude's `["SIGTERM","SIGKILL"]` enum; a numeric signal is
        // rejected outright.
        assert_eq!(kill_signal_name(15), "SIGTERM");
        assert_eq!(kill_signal_name(9), "SIGKILL");
        assert_eq!(kill_signal_name(2), "SIGTERM");
    }

    fn snap(short: &str, state: &str, name: Option<&str>) -> LiveSnapshot {
        LiveSnapshot {
            short: short.into(),
            session_id: Some(format!("{short}-uuid")),
            session_id_camel: None,
            cwd: Some("/tmp".into()),
            tempo: Some("active".into()),
            state: Some(state.into()),
            detail: None,
            needs: None,
            name: name.map(String::from),
            intent: None,
            source: Some("shell".into()),
            dying: false,
            gone: false,
            dead: false,
            alive: None,
            status: None,
            cli_version: Some("2.1.145".into()),
        }
    }

    #[test]
    fn stage_uploads_writes_sanitized_0600_files() {
        use base64::Engine;
        use std::os::unix::fs::PermissionsExt;

        let session_id = format!("test-{}", uuid::Uuid::new_v4());
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        // A normal name and a traversal attempt that must collapse to its basename.
        let bootstrap = json!({
            "uploads": [
                { "name": "notes.txt", "content_b64": b64("hello world") },
                { "name": "../../etc/evil", "content_b64": b64("nope") },
            ]
        });

        let paths = stage_uploads(&session_id, &bootstrap).expect("stage ok");
        assert_eq!(paths.len(), 2);
        let dir = std::path::Path::new("/tmp/cctui-uploads").join(&session_id);

        let notes = dir.join("notes.txt");
        assert!(paths.contains(&notes.to_string_lossy().into_owned()));
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "hello world");
        let mode = std::fs::metadata(&notes).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "uploaded file must be 0600");

        // Traversal collapsed to the bare basename inside the staging dir.
        let evil = dir.join("evil");
        assert!(evil.exists(), "traversal name must be reduced to a basename in-dir");
        assert!(!std::path::Path::new("/tmp/cctui-uploads").join("../../etc/evil").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_uploads_null_bootstrap_is_empty() {
        assert!(stage_uploads("sid", &serde_json::Value::Null).unwrap().is_empty());
    }

    #[test]
    fn session_context_block_lists_env_names_not_values() {
        use cctui_proto::adapter::{AdapterId, PermissionMode, SessionSpec};
        let mut env = std::collections::BTreeMap::new();
        env.insert("CCTUI_GITHUB_TOKEN".to_owned(), "super-secret".to_owned());
        env.insert("REGISTRY_USER".to_owned(), "admin".to_owned());
        let spec = SessionSpec {
            service_tier: None,
            adapter_id: AdapterId::new("claude-code"),
            working_dir: Some("/work/cctui".to_owned()),
            prompt: Some("refactor it".to_owned()),
            name: Some("refactor the dispatcher".to_owned()),
            permission_mode: Some(PermissionMode::Auto),
            effort: Some("high".to_owned()),
            model: Some("opus".to_owned()),
            env,
            bootstrap: serde_json::Value::Null,
            parent_local_id: None,
        };
        let block = build_session_context(&spec, "/work/cctui", &["a.rs".to_owned()], None);
        assert!(block.starts_with("<session-context>\n"));
        assert!(block.ends_with("</session-context>"));
        assert!(block.contains("session: refactor the dispatcher"));
        assert!(block.contains("model: opus · effort: high"));
        // acceptEdits/Auto normalizes to `auto`.
        assert!(block.contains("permission-mode: auto"));
        assert!(block.contains("cwd: /work/cctui"));
        assert!(block.contains("env (names only): CCTUI_GITHUB_TOKEN, REGISTRY_USER"));
        assert!(block.contains("  - a.rs"));
        // VALUES must never leak.
        assert!(!block.contains("super-secret"));
        assert!(!block.contains("admin"));
        assert!(!block.contains("CctuiAgent"));
    }

    #[test]
    fn session_context_advertises_the_agent_tool_when_capable() {
        use cctui_proto::adapter::{AdapterId, SessionSpec};
        let spec = SessionSpec {
            service_tier: None,
            adapter_id: AdapterId::new("claude-code"),
            working_dir: Some("/work/cctui".to_owned()),
            prompt: None,
            name: None,
            permission_mode: None,
            effort: None,
            model: None,
            env: std::collections::BTreeMap::new(),
            bootstrap: serde_json::Value::Null,
            parent_local_id: None,
        };
        let cap = cctui_proto::api::SpawnCapability::machine_default();
        let block = build_session_context(&spec, "/work/cctui", &[], Some(&cap));
        assert!(block.contains("mcp__cctui__CctuiAgent"));
        assert!(block.contains("adapters you may spawn: claude-code, codex, opencode"));
        assert!(block.contains("per-child budget ceiling: $20"));
        assert!(block.contains("example: mcp__cctui__CctuiAgent({\"adapter\": \"claude-code\""));
        assert!(block.contains("mcp__cctui__CctuiUsage"), "the limits tool is announced too");
        assert!(block.contains("blocked model burns the whole batch"), "{block}");
        assert!(block.ends_with("</session-context>"));

        let empty = cctui_proto::api::SpawnCapability::default();
        let block = build_session_context(&spec, "/work/cctui", &[], Some(&empty));
        assert!(!block.contains("CctuiAgent"), "an empty capability advertises nothing");
        assert!(!block.contains("CctuiUsage"), "the relay is absent, so neither tool exists");
    }

    #[test]
    fn stage_mid_chat_files_suffixes_name_collisions() {
        use base64::Engine;
        use cctui_proto::adapter::BootstrapFile;

        let session_id = format!("test-{}", uuid::Uuid::new_v4());
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        let dir = std::path::Path::new("/tmp/cctui-uploads").join(&session_id);

        // First upload stages report.pdf.
        let first = stage_mid_chat_files(
            &session_id,
            &[BootstrapFile { name: "report.pdf".into(), content_b64: b64("one") }],
        )
        .expect("stage ok");
        assert_eq!(first, vec![dir.join("report.pdf").to_string_lossy().into_owned()]);

        // A later upload with the same name must NOT overwrite — it gets a suffix.
        let second = stage_mid_chat_files(
            &session_id,
            &[
                BootstrapFile { name: "report.pdf".into(), content_b64: b64("two") },
                BootstrapFile { name: "report.pdf".into(), content_b64: b64("three") },
            ],
        )
        .expect("stage ok");
        assert_eq!(
            second,
            vec![
                dir.join("report-1.pdf").to_string_lossy().into_owned(),
                dir.join("report-2.pdf").to_string_lossy().into_owned(),
            ]
        );
        assert_eq!(std::fs::read_to_string(dir.join("report.pdf")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(dir.join("report-1.pdf")).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(dir.join("report-2.pdf")).unwrap(), "three");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_rm_outcome_distinguishes_gone_from_refused() {
        use super::{ClaudeRmOutcome, classify_claude_rm};
        assert_eq!(classify_claude_rm(Some(0), true, "", ""), ClaudeRmOutcome::Removed);
        assert_eq!(
            classify_claude_rm(Some(1), false, "", "No job matching 'ad162ca8'\n"),
            ClaudeRmOutcome::AlreadyGone
        );
        assert_eq!(
            classify_claude_rm(Some(1), false, "", "worktree has uncommitted changes: /w\n"),
            ClaudeRmOutcome::Refused("exit 1: worktree has uncommitted changes: /w".into())
        );
        assert_eq!(
            classify_claude_rm(None, false, "", ""),
            ClaudeRmOutcome::Refused("exit signal".into())
        );
        // The CLI's own explanation of a refusal is on stdout: it must reach
        // the log, not be dropped for a bare exit code.
        assert_eq!(
            classify_claude_rm(
                Some(1),
                false,
                "kept 229a5a4f — worktree is the working directory of a live session (pid 42)\n  worktree kept at /w\n  exit that session, then run 'claude rm 229a5a4f' again\n",
                ""
            ),
            ClaudeRmOutcome::Refused(
                "exit 1: kept 229a5a4f — worktree is the working directory of a live session (pid 42) worktree kept at /w exit that session, then run 'claude rm 229a5a4f' again".into()
            )
        );
        // stderr first when both speak: the error before the narration.
        assert_eq!(
            classify_claude_rm(Some(1), false, "kept x\n", "EACCES: permission denied\n"),
            ClaudeRmOutcome::Refused("exit 1: EACCES: permission denied kept x".into())
        );
    }

    fn driver() -> (Driver, mpsc::Receiver<AdapterEvent>) {
        let (tx, rx) = mpsc::channel(64);
        let (_cmd_tx, cmd_rx) = mpsc::channel(64);
        let tmp = tempfile::tempdir().unwrap().keep();
        let cfg = DriverConfig {
            poll_interval: Duration::from_millis(50),
            jobs_root: tmp.join("jobs"),
            projects_root: tmp.join("projects"),
            discovery: Discovery::with_base(tmp.join("daemon")),
            offsets_path: Some(tmp.join("offsets.json")),
            backfill_cursor_path: Some(tmp.join("backfill.json")),
            skip_backfill: true,
            claude_bin: "claude".to_string(),
            hook_socket_path: tmp.join("hook.sock"),
        };
        (Driver::new(cfg, tx, cmd_rx, CancellationToken::new()), rx)
    }

    #[tokio::test]
    async fn injected_turn_stamps_every_encoding_with_one_id() {
        let (d, mut rx) = driver();
        let id = uuid::Uuid::new_v4();
        d.note_turn("sess-1", Some(id));
        // The three shapes Claude stores one attachment-carrying turn in.
        for text in [
            "look at this\nAttached file:\n- /tmp/cctui-uploads/sess-1/shot.png",
            "[Image #1][shot.png]look at this",
            "[Image: source: /tmp/cctui-uploads/sess-1/shot.png]",
        ] {
            d.emit(AdapterEvent::Message {
                local_id: "sess-1".into(),
                payload: json!({"role": "user", "text": text, "meta": false}),
                turn_id: None,
            })
            .await;
        }
        for _ in 0..3 {
            let AdapterEvent::Message { turn_id, .. } = rx.recv().await.unwrap() else {
                panic!("want Message")
            };
            assert_eq!(turn_id, Some(id));
        }

        d.emit(AdapterEvent::Message {
            local_id: "sess-1".into(),
            payload: json!({"role": "assistant", "text": "on it"}),
            turn_id: None,
        })
        .await;
        let AdapterEvent::Message { turn_id, .. } = rx.recv().await.unwrap() else {
            panic!("want Message")
        };
        assert_eq!(turn_id, None, "assistant text must not inherit the turn id");

        d.emit(AdapterEvent::Message {
            local_id: "sess-2".into(),
            payload: json!({"role": "user", "text": "hi"}),
            turn_id: None,
        })
        .await;
        let AdapterEvent::Message { turn_id, .. } = rx.recv().await.unwrap() else {
            panic!("want Message")
        };
        assert_eq!(turn_id, None, "another session's turn must not inherit it");
    }

    #[tokio::test]
    async fn a_reply_without_a_turn_id_clears_the_previous_one() {
        let (d, mut rx) = driver();
        d.note_turn("sess-1", Some(uuid::Uuid::new_v4()));
        d.note_turn("sess-1", None);
        d.emit(AdapterEvent::Message {
            local_id: "sess-1".into(),
            payload: json!({"role": "user", "text": "typed in the TUI"}),
            turn_id: None,
        })
        .await;
        let AdapterEvent::Message { turn_id, .. } = rx.recv().await.unwrap() else {
            panic!("want Message")
        };
        assert_eq!(turn_id, None);
    }

    #[test]
    fn a_turn_id_older_than_the_window_is_not_reused() {
        let (d, _rx) = driver();
        let id = uuid::Uuid::new_v4();
        let stale = Instant::now()
            .checked_sub(TURN_ID_WINDOW + Duration::from_secs(1))
            .expect("monotonic clock must already be older than the window");
        d.pending_turns.lock().unwrap().insert("sess-1".into(), PendingTurn { id, at: stale });
        assert_eq!(d.turn_for("sess-1"), None);
        assert!(!d.pending_turns.lock().unwrap().contains_key("sess-1"));
    }

    #[tokio::test]
    async fn snapshot_emits_started_and_status() {
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, AdapterEvent::SessionStarted { .. }));
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, AdapterEvent::Status { .. }));
    }

    #[tokio::test]
    async fn snapshot_filters_spare_and_dying() {
        let (mut d, mut rx) = driver();
        let mut spare = snap("11111111", "working", None);
        spare.source = Some("spare".into());
        let mut dying = snap("22222222", "working", None);
        dying.dying = true;
        d.apply_snapshot(vec![spare, dying]).await;
        assert!(rx.try_recv().is_err(), "filtered jobs should emit nothing");
    }

    #[tokio::test]
    async fn snapshot_emits_ended_when_short_disappears() {
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("aaaa0001", "working", None)]).await;
        // Drain Started + Status.
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        d.apply_snapshot(vec![]).await;
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, AdapterEvent::SessionEnded { .. }));
    }

    #[test]
    fn is_dead_parses_defensive_shapes() {
        // no live known-dead sample, so several plausible shapes.
        let mut s = snap("abcd1234", "working", None);
        assert!(!s.is_dead(), "live working session is not dead");

        s.gone = true;
        assert!(s.is_dead(), "gone flag → dead");
        s.gone = false;

        s.dead = true;
        assert!(s.is_dead(), "dead flag → dead");
        s.dead = false;

        s.alive = Some(false);
        assert!(s.is_dead(), "alive:false → dead");
        s.alive = Some(true);
        assert!(!s.is_dead(), "alive:true → not dead");
        s.alive = None;

        s.status = Some("Exited".into());
        assert!(s.is_dead(), "status:exited (case-insensitive) → dead");
        s.status = Some("process gone".into());
        assert!(s.is_dead(), "status:'process gone' → dead");
        s.status = Some("running".into());
        assert!(!s.is_dead(), "status:running → not dead");
        s.status = None;

        s.state = Some("gone".into());
        assert!(s.is_dead(), "state:gone → dead");
        s.state = Some("working".into());
        assert!(!s.is_dead(), "state:working → not dead");
        s.state = None;

        s.tempo = Some("dead".into());
        assert!(s.is_dead(), "tempo:dead → dead");
        s.tempo = None;

        // the observed live shape — state:"failed", tempo:"idle",
        // detail:"process gone while supervisor was down". The phrase is
        // embedded in a sentence in `detail`, so it must match as a substring.
        s.state = Some("failed".into());
        s.tempo = Some("idle".into());
        s.detail = Some("process gone while supervisor was down".into());
        assert!(s.is_dead(), "detail containing 'process gone' → dead");
        s.detail = Some("Working on the fix".into());
        assert!(!s.is_dead(), "ordinary detail → not dead");
    }

    #[tokio::test]
    async fn dead_in_roster_emits_hibernated_with_state_json() {
        // B2: a still-listed session that claude reports dead emits a
        // hibernated Status (state.json survives → revivable red dot) within
        // one poll, without waiting for roster disappearance.
        let (mut d, mut rx) = driver();
        // Start it live.
        d.apply_snapshot(vec![snap("aaaa0001", "working", None)]).await;
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::SessionStarted { .. }));
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::Status { .. }));

        // Persist on-disk job state so the dead transition picks hibernated.
        let short_dir = d.cfg.jobs_root.join("aaaa0001");
        std::fs::create_dir_all(&short_dir).unwrap();
        std::fs::write(
            short_dir.join("state.json"),
            r#"{"sessionId":"sess-a","cwd":"/tmp","state":"working"}"#,
        )
        .unwrap();

        // Same short, now reported dead but STILL listed.
        let mut dead = snap("aaaa0001", "working", None);
        dead.gone = true;
        d.apply_snapshot(vec![dead]).await;
        let evt = rx.recv().await.unwrap();
        match evt {
            AdapterEvent::Status { tempo, .. } => {
                assert_eq!(tempo.as_deref(), Some("hibernated"));
            }
            other => panic!("expected hibernated Status, got {other:?}"),
        }

        // B3 sticky: a second poll still reporting dead must NOT re-emit
        // (no Status, no SessionEnded) — the dot can't be re-greened.
        let mut dead2 = snap("aaaa0001", "working", None);
        dead2.gone = true;
        d.apply_snapshot(vec![dead2]).await;
        assert!(rx.try_recv().is_err(), "dead-in-roster is sticky: no re-emit");
    }

    #[tokio::test]
    async fn dead_in_roster_emits_ended_without_state_json() {
        // B2: dead-but-listed with no surviving job state → SessionEnded
        // (the server marks the row `ended`, which is sticky).
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("bbbb0002", "working", None)]).await;
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::SessionStarted { .. }));
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::Status { .. }));

        let mut dead = snap("bbbb0002", "working", None);
        dead.status = Some("exited".into());
        d.apply_snapshot(vec![dead]).await;
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, AdapterEvent::SessionEnded { .. }));
    }

    #[tokio::test]
    async fn revive_clears_dead_sticky_and_resumes_status() {
        // if claude reports the short alive again after we marked it
        // dead, the sticky flag clears and live Status flows once more.
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("cccc0003", "working", None)]).await;
        rx.recv().await.unwrap(); // Started
        rx.recv().await.unwrap(); // Status

        let mut dead = snap("cccc0003", "working", None);
        dead.dead = true;
        d.apply_snapshot(vec![dead]).await;
        // SessionEnded (no state.json).
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::SessionEnded { .. }));

        // Revived: alive again → a fresh Status is emitted.
        d.apply_snapshot(vec![snap("cccc0003", "busy", None)]).await;
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::Status { .. }));
    }

    #[tokio::test]
    async fn resolve_short_for_removal_uses_live_map_then_derives() {
        // removal targets completed sessions, which have already
        // dropped out of the live roster. Prefer the live reverse map, but
        // fall back to the session UUID's first group (== the short).
        let (mut d, _rx) = driver();
        let mut s = snap("deadbeef", "working", None);
        s.session_id = Some("deadbeef-1111-2222-3333-444455556666".into());
        d.apply_snapshot(vec![s]).await;
        // Live: resolved from the map.
        assert_eq!(
            d.resolve_short_for_removal("deadbeef-1111-2222-3333-444455556666").unwrap(),
            "deadbeef"
        );
        // Exited (not in the map): derived from the UUID's first group.
        assert_eq!(
            d.resolve_short_for_removal("c0ffee00-9999-8888-7777-666655554444").unwrap(),
            "c0ffee00"
        );
        // Non-hex / malformed first group: refuse rather than guess.
        assert!(d.resolve_short_for_removal("zzzzzzzz-0000").is_err());
    }

    #[tokio::test]
    async fn transcript_repins_when_session_id_changes_on_reset() {
        // An in-process reset (`/clear`, `/compact`) or a
        // resume keeps the same `short` but gets a new `sessionId` (and a new
        // transcript file). We must follow the file to the new id, but keep
        // emitting under the ORIGINAL `local_id` so the post-reset transcript
        // appends to the one session the server already knows (splitting it
        // would let a worker-scoped `claude rm` archive wipe both at once).
        let (mut d, _rx) = driver();
        let mut s1 = snap("deadbeef", "working", None);
        s1.session_id = Some("sess-1".into());
        d.apply_snapshot(vec![s1]).await;
        let loc1 = d.transcript_locations.get("deadbeef").expect("pinned");
        assert_eq!(loc1.offset_key, "sess-1");
        assert_eq!(loc1.local_id, "sess-1");
        let path1 = loc1.path.clone();
        assert_eq!(d.short_by_session.get("sess-1").map(String::as_str), Some("deadbeef"));

        let mut s2 = snap("deadbeef", "working", None);
        s2.session_id = Some("sess-2".into());
        d.apply_snapshot(vec![s2]).await;
        let loc2 = d.transcript_locations.get("deadbeef").expect("re-pinned");
        assert_eq!(loc2.offset_key, "sess-2", "should follow the reset transcript");
        assert_ne!(loc2.path, path1, "transcript path should move to the new session id");
        assert_eq!(loc2.local_id, "sess-1", "local_id stays stable across the reset");
        // Both ids resolve to the worker for command dispatch.
        assert_eq!(d.short_by_session.get("sess-2").map(String::as_str), Some("deadbeef"));
        assert_eq!(d.short_by_session.get("sess-1").map(String::as_str), Some("deadbeef"));
    }

    #[tokio::test]
    async fn reset_emits_boundary_marker_under_original_session() {
        // a reset must not start/end a session — it injects a single
        // `context_reset` marker under the original `local_id` so the cut is
        // visible while the stream stays in one session.
        let (mut d, mut rx) = driver();
        let mut s1 = snap("deadbeef", "working", None);
        s1.session_id = Some("sess-1".into());
        d.apply_snapshot(vec![s1]).await;
        // Drain the first SessionStarted + Status.
        while rx.try_recv().is_ok() {}

        let mut s2 = snap("deadbeef", "working", None);
        s2.session_id = Some("sess-2".into());
        d.apply_snapshot(vec![s2]).await;

        let mut marker: Option<serde_json::Value> = None;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AdapterEvent::SessionStarted { .. } | AdapterEvent::SessionEnded { .. } => {
                    panic!("a reset must not start or end a session");
                }
                AdapterEvent::Message { local_id, payload, .. }
                    if payload.get("role").and_then(|r| r.as_str()) == Some("context_reset") =>
                {
                    assert_eq!(local_id, "sess-1", "marker rides the original session");
                    marker = Some(payload);
                }
                _ => {}
            }
        }
        let payload = marker.expect("a context_reset marker should be emitted");
        // The new session id keys the marker so a second reset isn't deduped.
        assert_eq!(payload.get("session_id").and_then(|s| s.as_str()), Some("sess-2"));
    }

    /// Write a subagent transcript under the parent's `subagents/` dir so a
    /// poll discovers it. Returns the agent file path.
    fn write_subagent(d: &Driver, parent_short: &str, agent_id: &str, lines: &[&str]) {
        use std::io::Write;
        let sess = format!("{parent_short}-uuid");
        let parent_path = transcript::transcript_path(&d.cfg.projects_root, "/tmp", &sess);
        let dir = transcript::subagents_dir(&parent_path);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join(format!("agent-{agent_id}.jsonl"))).unwrap();
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    #[tokio::test]
    async fn subagent_discovered_as_nested_session() {
        let (mut d, mut rx) = driver();
        // Pre-create the subagent transcript so the first poll finds it.
        write_subagent(
            &d,
            "abcd1234",
            "a8412884de5cc5396",
            &[
                r#"{"type":"assistant","isSidechain":true,"agentId":"a8412884de5cc5396","message":{"content":[{"type":"text","text":"sub work"}]}}"#,
            ],
        );
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;

        let mut started_parent = false;
        let mut started_child = false;
        let mut child_text = false;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AdapterEvent::SessionStarted { local_id, meta } => {
                    if local_id == "a8412884de5cc5396" {
                        started_child = true;
                        assert_eq!(
                            meta.parent_local_id.as_deref(),
                            Some("abcd1234-uuid"),
                            "subagent must link to its parent session id"
                        );
                        assert_eq!(meta.working_dir.as_deref(), Some("/tmp"));
                    } else {
                        started_parent = true;
                    }
                }
                AdapterEvent::Message { local_id, .. } if local_id == "a8412884de5cc5396" => {
                    child_text = true;
                }
                _ => {}
            }
        }
        assert!(started_parent, "parent SessionStarted expected");
        assert!(started_child, "subagent SessionStarted expected");
        assert!(child_text, "subagent transcript should stream through");
    }

    #[tokio::test]
    async fn workflow_subagent_carries_workflow_meta() {
        // a Workflow-tool agent under subagents/workflows/<runId>/ is
        // discovered and its SessionStarted meta.extra carries workflow context.
        use std::io::Write;
        let (mut d, mut rx) = driver();
        let sess = "abcd1234-uuid";
        let parent_path = transcript::transcript_path(&d.cfg.projects_root, "/tmp", sess);
        let run_dir = transcript::subagents_dir(&parent_path).join("workflows").join("wf_test123");
        std::fs::create_dir_all(&run_dir).unwrap();
        let mut f = std::fs::File::create(run_dir.join("agent-wfa.jsonl")).unwrap();
        f.write_all(br#"{"type":"assistant","isSidechain":true,"agentId":"wfa","message":{"content":[{"type":"text","text":"wf work"}]}}"#).unwrap();
        f.write_all(b"\n").unwrap();
        std::fs::write(
            run_dir.join("agent-wfa.meta.json"),
            br#"{"agentType":"workflow-subagent"}"#,
        )
        .unwrap();
        // Run-state with name (sibling: <session>/workflows/<runId>.json).
        let wf_state = parent_path.with_extension("").join("workflows");
        std::fs::create_dir_all(&wf_state).unwrap();
        std::fs::write(wf_state.join("wf_test123.json"), br#"{"name":"deep-research"}"#).unwrap();

        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;

        let mut extra = None;
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::SessionStarted { local_id, meta } = evt
                && local_id == "wfa"
            {
                extra = Some(meta.extra);
            }
        }
        let extra = extra.expect("workflow subagent SessionStarted expected");
        assert_eq!(extra.get("subagent").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(extra.get("workflow_run_id").and_then(|v| v.as_str()), Some("wf_test123"));
        assert_eq!(extra.get("workflow_name").and_then(|v| v.as_str()), Some("deep-research"));
        assert_eq!(extra.get("agent_type").and_then(|v| v.as_str()), Some("workflow-subagent"));
    }

    #[test]
    fn subagent_extra_carries_every_sidecar_field() {
        let meta = transcript::SubagentMeta {
            agent_type: Some("general-purpose".to_owned()),
            description: Some("Global competitors research".to_owned()),
            tool_use_id: Some("toolu_018vjjY9mx2Dfmjwv1mQs6nX".to_owned()),
            parent_agent_id: Some("af547156cb073af1e".to_owned()),
            spawn_depth: Some(2),
        };
        let extra = Driver::subagent_extra("a1e5bd", None, Some(&meta));
        assert_eq!(extra.get("subagent").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(extra.get("agent_id").and_then(|v| v.as_str()), Some("a1e5bd"));
        assert_eq!(extra.get("agent_type").and_then(|v| v.as_str()), Some("general-purpose"));
        assert_eq!(
            extra.get("tool_use_id").and_then(|v| v.as_str()),
            Some("toolu_018vjjY9mx2Dfmjwv1mQs6nX")
        );
        assert_eq!(
            extra.get("parent_agent_id").and_then(|v| v.as_str()),
            Some("af547156cb073af1e")
        );
        assert_eq!(extra.get("spawn_depth").and_then(serde_json::Value::as_u64), Some(2));
        // `description` rides the Status name, never `extra`.
        assert_eq!(extra.get("description"), None);
        assert_eq!(extra.get("workflow_run_id"), None);
    }

    #[test]
    fn subagent_extra_without_a_sidecar_is_the_bare_marks() {
        let extra = Driver::subagent_extra("f00dca", None, None);
        assert_eq!(extra.get("subagent").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(extra.get("agent_id").and_then(|v| v.as_str()), Some("f00dca"));
        for absent in ["agent_type", "tool_use_id", "parent_agent_id", "spawn_depth"] {
            assert_eq!(extra.get(absent), None, "{absent} must be absent");
        }
    }

    #[test]
    fn subagent_extra_defaults_the_type_for_a_sidecarless_workflow_agent() {
        let wf = transcript::WorkflowContext {
            run_id: "wf_test123".to_owned(),
            name: Some("deep-research".to_owned()),
        };
        let extra = Driver::subagent_extra("wfa", Some(&wf), None);
        assert_eq!(extra.get("workflow_run_id").and_then(|v| v.as_str()), Some("wf_test123"));
        assert_eq!(extra.get("workflow_name").and_then(|v| v.as_str()), Some("deep-research"));
        assert_eq!(extra.get("agent_type").and_then(|v| v.as_str()), Some("workflow-subagent"));

        // A sidecar on a workflow agent wins over that default.
        let meta = transcript::SubagentMeta {
            agent_type: Some("Explore".to_owned()),
            ..transcript::SubagentMeta::default()
        };
        let typed = Driver::subagent_extra("wfa", Some(&wf), Some(&meta));
        assert_eq!(typed.get("agent_type").and_then(|v| v.as_str()), Some("Explore"));

        // An unnamed run omits the name rather than emitting null.
        let anon = transcript::WorkflowContext { run_id: "wf_x".to_owned(), name: None };
        assert_eq!(Driver::subagent_extra("wfa", Some(&anon), None).get("workflow_name"), None);
    }

    #[test]
    fn subagent_name_status_sets_only_the_name() {
        let evt = Driver::subagent_name_status("a1e5bd", "Global competitors research".to_owned());
        let AdapterEvent::Status { local_id, name, tempo, state, activity, model, effort, .. } =
            evt
        else {
            panic!("expected a Status event");
        };
        assert_eq!(local_id, "a1e5bd");
        assert_eq!(name.as_deref(), Some("Global competitors research"));
        // Nothing else may be asserted by the rename — a Status carrying a
        // tempo or state here would move the subagent between buckets.
        assert!(tempo.is_none() && state.is_none() && activity.is_none());
        assert!(model.is_none() && effort.is_none());
    }

    #[tokio::test]
    async fn flat_task_subagent_is_named_from_its_sidecar() {
        // A Task-tool subagent used to reach the UI as a bare 6-char id hash.
        // Its sidecar names it, and the name rides the ordinary Status path.
        let (mut d, mut rx) = driver();
        write_subagent(
            &d,
            "abcd1234",
            "a1e5bd0f2c3d4e5f6",
            &[
                r#"{"type":"assistant","isSidechain":true,"agentId":"a1e5bd0f2c3d4e5f6","message":{"content":[{"type":"text","text":"sub work"}]}}"#,
            ],
        );
        let parent_path =
            transcript::transcript_path(&d.cfg.projects_root, "/tmp", "abcd1234-uuid");
        std::fs::write(
            transcript::subagents_dir(&parent_path).join("agent-a1e5bd0f2c3d4e5f6.meta.json"),
            br#"{"agentType":"general-purpose","description":"Global competitors research",
                 "toolUseId":"toolu_018vjjY9mx2Dfmjwv1mQs6nX","spawnDepth":1}"#,
        )
        .unwrap();
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;

        let mut extra = None;
        let mut name = None;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AdapterEvent::SessionStarted { local_id, meta }
                    if local_id == "a1e5bd0f2c3d4e5f6" =>
                {
                    extra = Some(meta.extra);
                }
                AdapterEvent::Status { local_id, name: n, .. }
                    if local_id == "a1e5bd0f2c3d4e5f6" =>
                {
                    name = n;
                }
                _ => {}
            }
        }
        assert_eq!(name.as_deref(), Some("Global competitors research"));
        let extra = extra.expect("task subagent SessionStarted expected");
        assert_eq!(extra.get("subagent").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(extra.get("agent_type").and_then(|v| v.as_str()), Some("general-purpose"));
        assert_eq!(
            extra.get("tool_use_id").and_then(|v| v.as_str()),
            Some("toolu_018vjjY9mx2Dfmjwv1mQs6nX")
        );
        assert_eq!(extra.get("spawn_depth").and_then(serde_json::Value::as_u64), Some(1));
        // No workflow run: this is the flat Task layout.
        assert_eq!(extra.get("workflow_run_id"), None);
    }

    #[tokio::test]
    async fn a_sidecarless_subagent_is_announced_unnamed() {
        // The pre-CCT-941 behaviour, preserved: no sidecar means no name and
        // no agent_type, and the UI falls back to the 6-char id.
        let (mut d, mut rx) = driver();
        write_subagent(
            &d,
            "abcd1234",
            "f00dcafe12345678a",
            &[
                r#"{"type":"assistant","isSidechain":true,"agentId":"f00dcafe12345678a","message":{"content":[{"type":"text","text":"anon"}]}}"#,
            ],
        );
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;

        let mut extra = None;
        let mut named = false;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                AdapterEvent::SessionStarted { local_id, meta }
                    if local_id == "f00dcafe12345678a" =>
                {
                    extra = Some(meta.extra);
                }
                AdapterEvent::Status { local_id, name, .. } if local_id == "f00dcafe12345678a" => {
                    named |= name.is_some();
                }
                _ => {}
            }
        }
        let extra = extra.expect("subagent SessionStarted expected");
        assert!(!named, "a sidecarless subagent must not be given a name");
        assert_eq!(extra.get("subagent").and_then(serde_json::Value::as_bool), Some(true));
        assert_eq!(extra.get("agent_type"), None);
        assert_eq!(extra.get("tool_use_id"), None);
    }

    #[tokio::test]
    async fn subagent_announced_once_then_ends_on_quiescence() {
        let (mut d, mut rx) = driver();
        write_subagent(
            &d,
            "abcd1234",
            "deadbeefcafe00001",
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#],
        );
        // First poll: discover + announce.
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        // Drain.
        while rx.try_recv().is_ok() {}

        // Subsequent idle polls must NOT re-announce, and after the idle
        // threshold the subagent ends exactly once.
        let mut started_again = 0;
        let mut ended = 0;
        for _ in 0..(SUBAGENT_IDLE_TICKS_TO_END + 2) {
            d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
            while let Ok(evt) = rx.try_recv() {
                match evt {
                    AdapterEvent::SessionStarted { local_id, .. }
                        if local_id == "deadbeefcafe00001" =>
                    {
                        started_again += 1;
                    }
                    AdapterEvent::SessionEnded { local_id, .. }
                        if local_id == "deadbeefcafe00001" =>
                    {
                        ended += 1;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(started_again, 0, "quiescent subagent must not be re-announced");
        assert_eq!(ended, 1, "subagent should end exactly once on quiescence");
    }

    /// Write (or rewrite) a parent session's own transcript. Byte offsets
    /// make a rewrite with the same prefix behave as an append.
    fn write_parent(d: &Driver, parent_short: &str, lines: &[&str]) {
        use std::io::Write;
        let sess = format!("{parent_short}-uuid");
        let path = transcript::transcript_path(&d.cfg.projects_root, "/tmp", &sess);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    const SUB_TOOL_USE: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_inner","name":"Bash","input":{}}]}}"#;
    const SUB_TOOL_RESULT: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_inner","content":"ok"}]}}"#;
    const SUB_FINAL_TEXT: &str =
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#;

    #[tokio::test]
    async fn a_subagent_waiting_on_the_model_is_not_ended_by_quiescence() {
        // Ticket claudo/inbox#210. A subagent whose last transcript line is a
        // `tool_result` owes the model a turn: it writes NOTHING while the
        // model thinks, and 30s of extended thinking is routine. Ending it on
        // bare quiescence marked live subagents `completed` about 30s after
        // their last tool, and `ended_subagents` then dropped every later
        // line. Measured on the 2026-09-17 sessions: last line at
        // 13:26:20.965Z, cctui ended the session at 13:26:51.415Z, and the
        // subagent went on to write 516 more lines until 14:14:59Z.
        let (mut d, mut rx) = driver();
        write_subagent(&d, "abcd1234", "deadbeefcafe00002", &[SUB_TOOL_USE, SUB_TOOL_RESULT]);
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        while rx.try_recv().is_ok() {}

        for _ in 0..(SUBAGENT_IDLE_TICKS_TO_END + 5) {
            d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        }
        while let Ok(evt) = rx.try_recv() {
            assert!(
                !matches!(
                    &evt,
                    AdapterEvent::SessionEnded { local_id, .. }
                        if local_id == "deadbeefcafe00002"
                ),
                "a subagent mid-turn must stay alive however long its transcript stays quiet"
            );
        }

        // Its answer finally lands: the tail now reads finished, so the idle
        // window ends it for real.
        write_subagent(
            &d,
            "abcd1234",
            "deadbeefcafe00002",
            &[SUB_TOOL_USE, SUB_TOOL_RESULT, SUB_FINAL_TEXT],
        );
        let mut ended = 0;
        for _ in 0..(SUBAGENT_IDLE_TICKS_TO_END + 2) {
            d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
            while let Ok(evt) = rx.try_recv() {
                if matches!(
                    &evt,
                    AdapterEvent::SessionEnded { local_id, .. }
                        if local_id == "deadbeefcafe00002"
                ) {
                    ended += 1;
                }
            }
        }
        assert_eq!(ended, 1, "a finished subagent still ends exactly once");
    }

    #[tokio::test]
    async fn a_subagent_ends_when_the_parent_records_its_tool_result() {
        // The authoritative end: the parent's `Task` call returned. It closes
        // the subagent on the next poll — no idle window to wait out — even
        // though the subagent's own tail stopped mid-turn.
        let (mut d, mut rx) = driver();
        write_parent(
            &d,
            "abcd1234",
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_task1","name":"Task","input":{}}]}}"#,
            ],
        );
        write_subagent(&d, "abcd1234", "deadbeefcafe00003", &[SUB_TOOL_USE, SUB_TOOL_RESULT]);
        let parent_path =
            transcript::transcript_path(&d.cfg.projects_root, "/tmp", "abcd1234-uuid");
        std::fs::write(
            transcript::subagents_dir(&parent_path).join("agent-deadbeefcafe00003.meta.json"),
            br#"{"agentType":"general-purpose","description":"child","toolUseId":"toolu_task1"}"#,
        )
        .unwrap();
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        while rx.try_recv().is_ok() {}

        // Still mid-turn, still alive.
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        while let Ok(evt) = rx.try_recv() {
            assert!(
                !matches!(
                    &evt,
                    AdapterEvent::SessionEnded { local_id, .. }
                        if local_id == "deadbeefcafe00003"
                ),
                "the Task call has not returned yet"
            );
        }

        // The parent records the Task's result → the child is over.
        write_parent(
            &d,
            "abcd1234",
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_task1","name":"Task","input":{}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_task1","content":"report"}]}}"#,
            ],
        );
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let mut ended = 0;
        while let Ok(evt) = rx.try_recv() {
            if matches!(
                &evt,
                AdapterEvent::SessionEnded { local_id, .. } if local_id == "deadbeefcafe00003"
            ) {
                ended += 1;
            }
        }
        assert_eq!(ended, 1, "the parent's tool_result ends the subagent at once");
    }

    #[tokio::test]
    async fn subagent_ends_when_parent_leaves_roster() {
        let (mut d, mut rx) = driver();
        write_subagent(
            &d,
            "abcd1234",
            "facefeed00001111a",
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#],
        );
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        while rx.try_recv().is_ok() {}
        // Parent disappears → its subagent must end too.
        d.apply_snapshot(vec![]).await;
        let mut child_ended = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(&evt, AdapterEvent::SessionEnded { local_id, .. } if local_id == "facefeed00001111a")
            {
                child_ended = true;
            }
        }
        assert!(child_ended, "subagent should end when its parent leaves the roster");
    }

    #[tokio::test]
    async fn blocked_state_does_not_emit_phantom_ask_question() {
        // Status never drives AskQuestion; the real prompt arrives via the
        // PreToolUse hook. A blocked snapshot must emit Status, never
        // AskQuestion.
        let (mut d, mut rx) = driver();
        let mut blocked = snap("abcd1234", "blocked", None);
        blocked.detail = Some("needs go-ahead to build & ship".into());
        d.apply_snapshot(vec![blocked]).await;

        while let Ok(evt) = rx.try_recv() {
            assert!(
                !matches!(evt, AdapterEvent::AskQuestion { .. } | AdapterEvent::AskResolved { .. }),
                "blocked status must not synthesize an Ask event"
            );
        }
    }

    #[test]
    fn parse_permission_needs_splits_tool_and_detail() {
        assert_eq!(
            parse_permission_needs("approve Bash: touch /tmp/x"),
            ("Bash".to_owned(), "touch /tmp/x".to_owned())
        );
        // No "approve " prefix, no separator → whole string as both.
        assert_eq!(parse_permission_needs("Edit"), ("Edit".to_owned(), "Edit".to_owned()));
        // Prefix but no separator.
        assert_eq!(
            parse_permission_needs("approve WebFetch"),
            ("WebFetch".to_owned(), "WebFetch".to_owned())
        );
    }

    #[tokio::test]
    async fn blocked_approve_emits_permission_request_then_resolves() {
        // a `tempo:"blocked"` snapshot whose `needs` reads
        // "approve <Tool>: <detail>" surfaces a PermissionRequest; clearing the
        // block (next poll) emits PermissionResolved exactly once.
        let (mut d, mut rx) = driver();
        let mut blocked = snap("abcd1234", "running", None);
        blocked.tempo = Some("blocked".into());
        blocked.needs = Some("approve Bash: touch /tmp/x".into());
        d.apply_snapshot(vec![blocked]).await;

        let mut request: Option<(String, String, String)> = None;
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::PermissionRequest { local_id, request_id, tool, input } = evt {
                assert_eq!(tool, "Bash");
                assert_eq!(input.get("description").and_then(|d| d.as_str()), Some("touch /tmp/x"));
                request = Some((local_id, request_id, tool));
            }
        }
        let (_, request_id, _) = request.expect("PermissionRequest expected for blocked+approve");

        // Re-poll while still blocked on the SAME prompt: no duplicate emit.
        let mut still = snap("abcd1234", "running", None);
        still.tempo = Some("blocked".into());
        still.needs = Some("approve Bash: touch /tmp/x".into());
        d.apply_snapshot(vec![still]).await;
        while let Ok(evt) = rx.try_recv() {
            assert!(
                !matches!(evt, AdapterEvent::PermissionRequest { .. }),
                "an unchanged prompt must not re-emit PermissionRequest"
            );
        }

        // Prompt clears (answered / tempo back to active) → resolve once.
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let mut resolved = 0;
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::PermissionResolved { request_id: rid, .. } = evt {
                assert_eq!(rid, request_id, "resolved id matches the emitted request");
                resolved += 1;
            }
        }
        assert_eq!(resolved, 1, "clearing the prompt resolves it exactly once");
    }

    #[tokio::test]
    async fn permission_resolved_when_session_ends_while_blocked() {
        // A worker that disappears mid-prompt must not leave a stale card.
        let (mut d, mut rx) = driver();
        let mut blocked = snap("abcd1234", "running", None);
        blocked.tempo = Some("blocked".into());
        blocked.needs = Some("approve Bash: rm -rf /tmp/x".into());
        d.apply_snapshot(vec![blocked]).await;
        while rx.try_recv().is_ok() {}
        d.apply_snapshot(vec![]).await;
        let mut resolved = false;
        while let Ok(evt) = rx.try_recv() {
            if matches!(evt, AdapterEvent::PermissionResolved { .. }) {
                resolved = true;
            }
        }
        assert!(resolved, "a vanished blocked session emits PermissionResolved");
    }

    #[tokio::test]
    async fn pinning_records_session_to_local_map() {
        // the ask-hook listener resolves a hook's live `session_id`
        // through this map. First pin maps the id to itself; a `/clear`
        // rotation maps the NEW id to the stable original `local_id`.
        let (mut d, _rx) = driver();
        let mut s1 = snap("deadbeef", "working", None);
        s1.session_id = Some("sess-1".into());
        d.apply_snapshot(vec![s1]).await;
        assert_eq!(
            d.session_map().lock().unwrap().get("sess-1").map(String::as_str),
            Some("sess-1")
        );

        let mut s2 = snap("deadbeef", "working", None);
        s2.session_id = Some("sess-2".into());
        d.apply_snapshot(vec![s2]).await;
        assert_eq!(
            d.session_map().lock().unwrap().get("sess-2").map(String::as_str),
            Some("sess-1"),
            "rotated id resolves to the stable local_id"
        );
    }

    #[tokio::test]
    async fn status_dedup_skips_unchanged_polls() {
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("c0ffee00", "working", Some("ours"))]).await;
        rx.recv().await.unwrap(); // started
        rx.recv().await.unwrap(); // status
        d.apply_snapshot(vec![snap("c0ffee00", "working", Some("ours"))]).await;
        assert!(rx.try_recv().is_err(), "identical poll should emit nothing");
    }

    /// Drain events until the Diagnose reply for `request_id` arrives.
    async fn recv_diagnose(
        rx: &mut mpsc::Receiver<AdapterEvent>,
        request_id: uuid::Uuid,
    ) -> SessionDiagnose {
        loop {
            match rx.recv().await.expect("event stream open") {
                AdapterEvent::Diagnose { request_id: rid, report, .. } if rid == request_id => {
                    return *report;
                }
                _ => {}
            }
        }
    }

    /// the diagnose assembly aggregates the driver's live state —
    /// resolved short, activity-sourced verdict with an observation timestamp,
    /// pinned transcript, and honest `missing` facts for signals not present.
    #[tokio::test]
    async fn diagnose_assembles_dated_facts_for_live_session() {
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("abcd1234", "working", Some("ours"))]).await;

        let request_id = uuid::Uuid::new_v4();
        d.handle_diagnose("abcd1234-uuid", request_id).await.unwrap();
        let report = recv_diagnose(&mut rx, request_id).await;

        assert_eq!(report.local_id, "abcd1234-uuid");
        assert_eq!(report.short.as_deref(), Some("abcd1234"));
        assert_eq!(report.adapter, "claude-code");
        assert!(report.generated_at_ms > 0);

        // Effective state: derived from the poll snapshot (activity source),
        // dated by the poll observation.
        let es = &report.effective_state;
        assert_eq!(es.source, "activity");
        let v = es.value.as_ref().expect("effective state present");
        assert_eq!(v.verdict, "active/working");
        assert_eq!(v.tempo.as_deref(), Some("active"));
        assert!(es.observed_at_ms.is_some());
        assert!(es.age_ms.is_some_and(|a| a >= 0));

        // Transcript was pinned by the snapshot; the file doesn't exist yet so
        // the fact is present-but-undated with offset 0.
        let t = report.transcript.value.as_ref().expect("transcript pinned");
        assert!(t.path.ends_with("abcd1234-uuid.jsonl"), "{}", t.path);
        assert_eq!(t.tail_offset, 0);
        assert_eq!(t.mtime_ms, None);

        // No socket in the temp discovery base.
        let sock = report.claude_socket.value.as_ref().unwrap();
        assert!(!sock.live);
        assert!(sock.path.is_none());

        // Nothing pending, nothing recorded → honest missing/false facts.
        let p = report.prompts.value.as_ref().unwrap();
        assert!(!p.pending_ask && !p.parked_perm_hook);
        // No held-attach task in this unit driver, so no PTY output observed.
        assert!(report.pty_output.value.is_none(), "no attach → no PTY output");
        assert!(report.pty_output.missing_reason.as_deref().unwrap().contains("PTY output"));
        assert!(report.dispatch.value.is_none(), "not a dispatched session");
        assert!(report.permission_mode.value.is_none(), "posture never recorded");
        assert!(report.last_hook_event.value.is_none());
        assert!(!report.gateway.value.as_ref().unwrap().server_configured);
    }

    /// a pending ask (hook signal) wins the arbitration and surfaces
    /// in both the verdict and the prompts fact; an unknown session still
    /// produces a fail-soft report rather than an error.
    #[tokio::test]
    async fn diagnose_hook_signal_wins_and_unknown_session_fails_soft() {
        let (mut d, mut rx) = driver();
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        d.pending_asks.lock().unwrap().insert("abcd1234-uuid".into(), None);

        let request_id = uuid::Uuid::new_v4();
        d.handle_diagnose("abcd1234-uuid", request_id).await.unwrap();
        let report = recv_diagnose(&mut rx, request_id).await;
        assert_eq!(report.effective_state.source, "hook");
        assert!(report.effective_state.value.unwrap().verdict.contains("ask"));
        assert!(report.prompts.value.unwrap().pending_ask);

        // Unknown session: no short resolvable → missing facts, not an Err.
        let request_id = uuid::Uuid::new_v4();
        d.handle_diagnose("not-a-known-session", request_id).await.unwrap();
        let report = recv_diagnose(&mut rx, request_id).await;
        assert!(report.short.is_none());
        assert!(report.effective_state.value.is_none());
        assert!(
            report.effective_state.missing_reason.as_deref().unwrap().contains("unknown session")
        );
    }

    /// Write `lines` to a live session's main transcript at the path
    /// `apply_snapshot` resolves for `snap(short, …)`, returning its `local_id`.
    fn write_main_transcript(d: &Driver, short: &str, lines: &[&str]) -> String {
        use std::io::Write;
        let sess = format!("{short}-uuid");
        let path = transcript::transcript_path(&d.cfg.projects_root, "/tmp", &sess);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        sess
    }

    fn text_line(t: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{t}"}}]}}}}"#
        )
    }

    fn drain_messages(rx: &mut mpsc::Receiver<AdapterEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let AdapterEvent::Message { payload, .. } = evt
                && let Some(t) = payload.get("text").and_then(|v| v.as_str())
            {
                out.push(t.to_owned());
            }
        }
        out
    }

    #[tokio::test]
    async fn resume_mark_clamps_cursor_forward_and_skips_replay() {
        // a server mark ahead of the (cold-start empty) local offset
        // fast-forwards the tail cursor, so the bytes the server already has are
        // never re-emitted.
        let (mut d, mut rx) = driver();
        let l0 = text_line("first");
        let sess = write_main_transcript(&d, "abcd1234", &[&l0]);
        // First poll establishes the location and tails "first".
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let seen = drain_messages(&mut rx);
        assert!(seen.contains(&"first".to_owned()));
        let mark = d.offsets.get(&sess);
        assert!(mark > 0);

        // Append two more lines, then wipe the local offset to simulate a
        // daemon restart (in prod offsets are in-memory only).
        let l1 = text_line("second");
        let l2 = text_line("third");
        write_main_transcript(&d, "abcd1234", &[&l1, &l2]);
        d.offsets.set(sess.clone(), 0);

        // The server hands back its stored mark (end of "first").
        d.apply_resume_marks(vec![(sess.clone(), mark)]).await;
        assert_eq!(d.offsets.get(&sess), mark, "cursor clamps forward to the mark");

        // Next poll resumes from the mark: only the two new lines, never "first".
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let seen = drain_messages(&mut rx);
        assert!(seen.contains(&"second".to_owned()) && seen.contains(&"third".to_owned()));
        assert!(!seen.contains(&"first".to_owned()), "clamped bytes must not replay");
    }

    #[tokio::test]
    async fn absent_mark_triggers_one_bounded_resend_then_idle_is_silent() {
        // acceptance: a session we already tail with NO server mark gets
        // exactly one bounded re-send window; once the offsets agree, repeated
        // periodic passes at idle emit nothing.
        let (mut d, mut rx) = driver();
        let l0 = text_line("alpha");
        let l1 = text_line("beta");
        write_main_transcript(&d, "abcd1234", &[&l0, &l1]);
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let _ = drain_messages(&mut rx);

        // No mark for this session → one bounded window re-send (server dedups).
        d.apply_resume_marks(vec![]).await;
        let resent = drain_messages(&mut rx);
        assert!(resent.contains(&"alpha".to_owned()) && resent.contains(&"beta".to_owned()));

        // Now offsets and the recorded server mark agree: several periodic
        // reconcile passes must emit ZERO frames (the whole point of the ticket).
        for _ in 0..5 {
            d.reconcile_tail(false).await;
        }
        assert!(rx.try_recv().is_err(), "idle periodic reconcile must emit nothing");
    }

    #[tokio::test]
    async fn divergent_mark_behind_local_triggers_resend() {
        // the server's mark is BEHIND our persisted offset (a send
        // dropped before reconnect) — heal the gap with one bounded window.
        let (mut d, mut rx) = driver();
        let l0 = text_line("one");
        let l1 = text_line("two");
        let sess = write_main_transcript(&d, "abcd1234", &[&l0, &l1]);
        d.apply_snapshot(vec![snap("abcd1234", "working", None)]).await;
        let _ = drain_messages(&mut rx);
        let local = d.offsets.get(&sess);
        assert!(local > 1);

        d.apply_resume_marks(vec![(sess.clone(), 1)]).await;
        let resent = drain_messages(&mut rx);
        assert!(!resent.is_empty(), "a mark behind the local offset must re-send the window");
        assert_eq!(d.offsets.get(&sess), local, "the persisted offset is never rewound");
    }

    /// Stand-in claude daemon: answers every request `{ok:true}` and hands
    /// the requests it saw back through the returned receiver.
    fn recording_socket() -> (PathBuf, mpsc::UnboundedReceiver<serde_json::Value>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = std::env::temp_dir().join(format!("cctui-sleep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(r).read_line(&mut line).await.is_ok() {
                    let _ = tx.send(serde_json::from_str(&line).unwrap_or_default());
                    let _ = w.write_all(b"{\"ok\":true}\n").await;
                }
            }
        });
        (path, rx)
    }

    fn quiet_transcript(d: &mut Driver, short: &str, mins_ago: u64) {
        let path = d.cfg.projects_root.join(format!("{short}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(&path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(mins_ago * 60)).unwrap();
        d.transcript_locations.insert(
            short.to_owned(),
            TranscriptLocation {
                path,
                local_id: format!("{short}-local"),
                cwd: "/tmp".into(),
                offset_key: short.to_owned(),
            },
        );
    }

    #[tokio::test]
    async fn memory_pressure_sleeps_the_longest_idle_free_session_only() {
        let (mut d, _rx) = driver();
        // Threshold 100 %: any real host is "under pressure", no grace.
        d.sleep_policy =
            Some(crate::mempressure::Policy { threshold_pct: 100.0, min_idle: Duration::ZERO });
        quiet_transcript(&mut d, "aaaa0001", 5);
        quiet_transcript(&mut d, "aaaa0002", 60);
        quiet_transcript(&mut d, "aaaa0003", 600);
        // The oldest one still has a live subagent: not a candidate.
        d.subagents.insert(
            "agent-x".into(),
            SubagentState {
                parent_local_id: "aaaa0003-local".into(),
                idle_ticks: 0,
                tool_use_id: None,
                parent_resolved: false,
                tail_pending: true,
            },
        );
        let (sock, mut seen) = recording_socket();
        let finished = ["aaaa0001".to_owned(), "aaaa0002".to_owned(), "aaaa0003".to_owned()];
        d.sleep_under_memory_pressure(&sock, &finished).await;
        let req = seen.try_recv().expect("one worker is stopped");
        assert_eq!(req["op"], "kill");
        assert_eq!(req["short"], "aaaa0002", "the longest idle session without live work");
        // Cooldown: the next poll does not stop another one right away.
        d.sleep_under_memory_pressure(&sock, &finished).await;
        assert!(seen.try_recv().is_err(), "one worker per step");
    }

    #[tokio::test]
    async fn no_pressure_no_sleep() {
        let (mut d, _rx) = driver();
        d.sleep_policy =
            Some(crate::mempressure::Policy { threshold_pct: 0.0, min_idle: Duration::ZERO });
        quiet_transcript(&mut d, "aaaa0001", 600);
        let (sock, mut seen) = recording_socket();
        d.sleep_under_memory_pressure(&sock, &["aaaa0001".to_owned()]).await;
        assert!(seen.try_recv().is_err(), "memory is plentiful: nothing is stopped");
    }
}
