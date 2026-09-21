//! Codex log-tail adapter.
//!
//! Watches `~/.codex/sessions/` for new log files. For each new file:
//!
//! 1. Emit `SessionStarted` with `local_id` = file basename (without
//!    `.jsonl` / `.log` suffix) and `working_dir` from the
//!    `cwd`/`working_dir` field in the first parseable JSON line that
//!    carries one (if any).
//! 2. Tail subsequent lines: if the line parses as JSON it becomes a
//!    `Message` payload as-is; otherwise it's wrapped as
//!    `{role: "assistant", text: <line>}`. Tool-call payloads are
//!    recognised heuristically by the presence of a `"tool"` or
//!    `"function_call"` field.
//! 3. After `quiesce_secs` of no new bytes on a tracked file, emit a
//!    `hibernated` `Status`: an idle rollout is not a finished one, so the
//!    session stays tracked and resumes streaming when it grows again.
//!    `SessionEnded` is reserved for the rollout file disappearing.
//!
//! The exact Codex log schema isn't documented here — this is an
//! opt-in scaffold that will need refinement once we have concrete
//! fixtures. The line parser is intentionally permissive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cctui_proto::adapter::{AdapterEvent, EndReason, SessionMeta};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct LogTailConfig {
    pub sessions_root: PathBuf,
    pub poll_interval: Duration,
    pub quiesce: Duration,
    pub offsets_path: Option<PathBuf>,
}

impl Default for LogTailConfig {
    fn default() -> Self {
        Self {
            sessions_root: default_sessions_root(),
            poll_interval: Duration::from_secs(2),
            quiesce: Duration::from_mins(1),
            offsets_path: dirs::config_dir().map(|d| d.join("cctui").join("codex-offsets.json")),
        }
    }
}

impl LogTailConfig {
    pub fn from_value(v: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(p) = v.get("sessions_root").and_then(Value::as_str) {
            cfg.sessions_root = PathBuf::from(p);
        }
        if let Some(ms) = v.get("poll_interval_ms").and_then(Value::as_u64) {
            cfg.poll_interval = Duration::from_millis(ms);
        }
        if let Some(s) = v.get("quiesce_secs").and_then(Value::as_u64) {
            cfg.quiesce = Duration::from_secs(s);
        }
        cfg
    }
}

#[must_use]
pub fn default_sessions_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(".codex").join("sessions")
}

#[derive(Debug)]
struct TrackedSession {
    local_id: String,
    offset: u64,
    last_activity: Instant,
    hibernated: bool,
}

pub struct LogTail {
    cfg: LogTailConfig,
    events: mpsc::Sender<AdapterEvent>,
    shutdown: CancellationToken,
    sessions: HashMap<PathBuf, TrackedSession>,
    /// Sessions driven by the app-server. Their rollout files are
    /// skipped here so we don't double-ingest. `local_id` is the rollout
    /// `UUIDv7`, which is a suffix of the rollout filename stem.
    owned: Option<super::app_server::SessionRegistry>,
    /// Threads whose transcript was served structurally via
    /// `thread/read` + `thread/turns/list`. Their rollout files are skipped
    /// for the same no-double-ingest reason as `owned`.
    served: Option<super::thread_read::ServedIds>,
    /// Rollout-path → byte offset, persisted so restarts and quiesce
    /// evictions never re-read (re-upload) historical rollouts.
    offsets: crate::offsets::OffsetStore,
    offsets_dirty: bool,
    /// `local_id` → offset the server last persisted. A mark behind our own
    /// offset means events were emitted but never stored, so the reconcile
    /// pass replays from the mark instead.
    marks: ResumeMarks,
    reconciled: HashSet<PathBuf>,
}

/// The server's per-session transcript marks, written by the codex command
/// pump on a `ResumeMarks` frame and read by the tail when it adopts a rollout.
pub type ResumeMarks = Arc<Mutex<HashMap<String, u64>>>;

/// How far behind the persisted offset the reconcile pass backs up before
/// re-reading, mirroring the claude-code transcript tailer. The server's
/// `(session_id, event_type, content_hash, turn_id)` dedup drops every replayed
/// duplicate, so the window can be generous.
pub const RECONCILE_BACKUP_BYTES: u64 = 64 * 1024;

impl LogTail {
    pub fn new(
        cfg: LogTailConfig,
        events: mpsc::Sender<AdapterEvent>,
        shutdown: CancellationToken,
    ) -> Self {
        let offsets = crate::offsets::OffsetStore::open(cfg.offsets_path.clone());
        Self {
            cfg,
            events,
            shutdown,
            sessions: HashMap::new(),
            owned: None,
            served: None,
            offsets,
            offsets_dirty: false,
            marks: ResumeMarks::default(),
            reconciled: HashSet::new(),
        }
    }

    /// Share the store the command pump writes server transcript marks into.
    pub fn set_resume_marks(&mut self, marks: ResumeMarks) {
        self.marks = marks;
    }

    /// Share the app-server session registry so app-server-owned rollout
    /// files are skipped (no double-ingest of the same session).
    pub fn set_owned(&mut self, registry: super::app_server::SessionRegistry) {
        self.owned = Some(registry);
    }

    /// Share the set of threads the structured history reader has already
    /// served, so the heuristic tail stays a fallback rather than a duplicate.
    pub fn set_served(&mut self, served: super::thread_read::ServedIds) {
        self.served = Some(served);
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(self.cfg.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    self.scan_once().await;
                }
            }
        }
    }

    async fn scan_once(&mut self) {
        // App-server-owned session ids (rollout UUIDv7). Files whose stem
        // ends with one of these are driven directly via app-server and must
        // not be tailed here.
        let mut owned: Vec<String> = match &self.owned {
            Some(reg) => reg.lock().await.keys().cloned().collect(),
            None => Vec::new(),
        };
        if let Some(served) = &self.served {
            owned.extend(served.lock().await.iter().cloned());
        }
        // Ids merely surfaced by the `thread/list` inventory are NOT skipped:
        // the inventory alone seeds only a preview, and suppressing the tail
        // left discovered CLI sessions with an empty conversation. Only
        // threads cctui drives live (`owned`) or whose real transcript came
        // back from `thread/turns/list` (`served`) are skipped.
        let mut alive: HashSet<PathBuf> = HashSet::new();
        // real rollouts live under YYYY/MM/DD subdirectories, not
        // directly under the sessions root, so the scan recurses.
        let mut files = Vec::new();
        collect_rollout_files(&self.cfg.sessions_root, 0, &mut files);
        for path in files {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if owned.iter().any(|id| stem.ends_with(id.as_str())) {
                continue;
            }
            alive.insert(path.clone());
            self.tail_file(path).await;
        }
        // The rollout is gone: the only evidence of an end this adapter has.
        let ended: Vec<PathBuf> =
            self.sessions.keys().filter(|p| !alive.contains(*p)).cloned().collect();
        for path in ended {
            if let Some(s) = self.sessions.remove(&path) {
                let _ = self
                    .events
                    .send(AdapterEvent::SessionEnded {
                        local_id: s.local_id,
                        reason: EndReason::Completed,
                    })
                    .await;
            }
        }
        let now = Instant::now();
        let idle: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                !s.hibernated && now.duration_since(s.last_activity) > self.cfg.quiesce
            })
            .map(|(p, _)| p.clone())
            .collect();
        for path in idle {
            let Some(s) = self.sessions.get_mut(&path) else { continue };
            s.hibernated = true;
            let local_id = s.local_id.clone();
            let _ = self.events.send(hibernated_status(local_id)).await;
        }
        if !alive.is_empty() {
            let keep: HashSet<String> =
                alive.iter().map(|p| p.to_string_lossy().into_owned()).collect();
            if self.offsets.retain(|k| keep.contains(k)) {
                self.offsets_dirty = true;
            }
        }
        if self.offsets_dirty {
            self.offsets.flush();
            self.offsets_dirty = false;
        }
    }

    async fn tail_file(&mut self, path: PathBuf) {
        let meta = std::fs::metadata(&path).ok();
        let len = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let key = path.to_string_lossy().into_owned();

        let is_new = !self.sessions.contains_key(&path);
        if is_new {
            // Quiet rollout with nothing beyond the persisted offset: leave it
            // untracked so it stays invisible (no Started/Ended churn) unless
            // the server still holds a mark for it — then the gap behind that
            // mark is exactly what the reconcile pass must replay.
            if len <= self.offsets.get(&key) && !self.needs_quiet_reconcile(&path, &key) {
                return;
            }
            let local_id = derive_local_id(&path);
            let observed_at = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
            let link = rollout_link(&path);
            let mut extra = json!({"source": "codex-log-tail", "observed_at": observed_at});
            let mut parent_local_id = None;
            if link.source.as_deref().is_some_and(|s| s.starts_with("subAgent")) {
                // A subagent rollout that can't resolve a parent can never nest;
                // skip it so this path matches the inventory's orphan skip.
                let Some(parent) = link.subagent_parent else { return };
                parent_local_id =
                    Some(crate::dispatch_codex::dispatch_session_for(&parent).unwrap_or(parent));
                extra["subagent"] = json!(true);
            } else if let Some(parent) = link.launcher_parent
                // A self-parent would make the server's recursive heartbeat CTE
                // walk a cycle, so refuse it.
                && parent != local_id
            {
                parent_local_id = Some(parent);
                extra["subagent"] = json!(true);
            }
            let _ = self
                .events
                .send(AdapterEvent::SessionStarted {
                    local_id: local_id.clone(),
                    meta: SessionMeta { working_dir: None, parent_local_id, extra },
                })
                .await;
            self.sessions.insert(
                path.clone(),
                TrackedSession {
                    local_id,
                    offset: self.offsets.get(&key).min(len),
                    last_activity: Instant::now(),
                    hibernated: false,
                },
            );
        }

        self.reconcile_once(&path, &key).await;

        let session = self.sessions.get(&path).expect("inserted above");
        let (offset, local_id) = (session.offset, session.local_id.clone());
        if len <= offset {
            return; // no new bytes
        }
        let (events, new_offset) = match read_new_lines(&path, offset, &local_id) {
            Ok(res) => res,
            Err(err) => {
                tracing::debug!(%err, ?path, "codex log read failed");
                return;
            }
        };
        if events.is_empty() {
            let session = self.sessions.get_mut(&path).expect("inserted above");
            session.offset = new_offset;
            if new_offset > offset {
                self.offsets.set(key, new_offset);
                self.offsets_dirty = true;
            }
            return;
        }
        // The offset may only advance over events the receiver actually took:
        // a dropped send is a hole the next scan has to re-read, and a
        // persisted offset past it would make that hole permanent.
        for evt in events {
            if self.events.send(evt).await.is_err() {
                return;
            }
        }
        let session = self.sessions.get_mut(&path).expect("inserted above");
        session.offset = new_offset;
        session.last_activity = Instant::now();
        session.hibernated = false;
        self.offsets.set(key, new_offset);
        self.offsets_dirty = true;
        let _ =
            self.events.send(AdapterEvent::TranscriptMark { local_id, offset: new_offset }).await;
    }

    /// A quiet rollout still worth adopting: one we have tailed before and the
    /// server holds a mark for, i.e. a candidate for a gap that opened while
    /// its events were going nowhere.
    fn needs_quiet_reconcile(&self, path: &Path, key: &str) -> bool {
        if self.reconciled.contains(path) || self.offsets.get(key) == 0 {
            return false;
        }
        let local_id = derive_local_id(path);
        self.marks.lock().is_ok_and(|m| m.contains_key(&local_id))
    }

    /// Bounded re-read behind the persisted offset, once per rollout per
    /// process. Emits without advancing any offset: it deliberately re-reads
    /// seen lines and leans on the server's content-hash dedup.
    async fn reconcile_once(&mut self, path: &Path, key: &str) {
        if !self.reconciled.insert(path.to_path_buf()) {
            return;
        }
        let persisted = self.offsets.get(key);
        if persisted == 0 {
            return;
        }
        let Some(session) = self.sessions.get(path) else { return };
        let local_id = session.local_id.clone();
        let mark = self.marks.lock().ok().and_then(|m| m.get(&local_id).copied());
        let anchor = mark.map_or(persisted, |m| m.min(persisted));
        let events = match reconcile_tail(path, &local_id, anchor) {
            Ok(events) => events,
            Err(err) => {
                tracing::debug!(%err, ?path, "codex reconcile read failed");
                return;
            }
        };
        if events.is_empty() {
            return;
        }
        tracing::info!(%local_id, anchor, count = events.len(), "codex: reconciling rollout tail");
        for evt in events {
            if self.events.send(evt).await.is_err() {
                return;
            }
        }
    }
}

fn hibernated_status(local_id: String) -> AdapterEvent {
    AdapterEvent::Status {
        local_id,
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
    }
}

/// Recursively collect rollout files under `dir`. Codex nests sessions as
/// `sessions/YYYY/MM/DD/rollout-*.jsonl`; depth is capped so a symlink cycle or
/// unexpected tree can't spin the scan forever. Flat files directly under the
/// root (used by tests and older layouts) are still picked up at depth 0.
fn collect_rollout_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, depth + 1, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Canonical thread id for a rollout file. The identity is the
/// `session_meta` payload `id` (a UUID), not the filename — the app-server
/// registry and `thread/list` inventory key off the same UUID, so deriving it
/// from the file avoids a second, filename-shaped local id for one thread.
/// Falls back to a UUID embedded in the filename, then the bare stem.
fn derive_local_id(path: &Path) -> String {
    if let Some(id) = session_meta_id(path) {
        return id;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    uuid_from_stem(stem).unwrap_or_else(|| stem.to_owned())
}

/// Read the `session_meta` line (first line of a well-formed rollout) and return
/// its lowercased payload `id`. Scans a bounded prefix in case the meta isn't
/// strictly first; returns `None` for files that carry no `session_meta`.
fn session_meta_id(path: &Path) -> Option<String> {
    session_meta_payload(path)
        .as_ref()
        .and_then(|p| p.get("id").and_then(Value::as_str).map(str::to_ascii_lowercase))
}

/// The `session_meta` line's `payload` object, or `None` for a rollout that
/// carries no `session_meta`. Scans a bounded prefix in case the meta isn't
/// strictly first.
fn session_meta_payload(path: &Path) -> Option<Value> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(64).map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else { continue };
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            return value.get("payload").cloned();
        }
    }
    None
}

/// Set by `deploy/codex-run.sh` through `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, which
/// codex copies verbatim into `session_meta.originator`.
const LAUNCHER_ORIGINATOR_PREFIX: &str = "cctui-parent.";

struct RolloutLink {
    source: Option<String>,
    subagent_parent: Option<String>,
    launcher_parent: Option<String>,
}

/// Reduce a rollout's `session_meta` to how it links to a parent. Mirrors the
/// `thread/list` inventory extraction so both discovery paths agree on which
/// rollouts are subagents and who their parent is. Codex calls a plain `codex
/// exec` a root thread, so the stamped originator is the only thing tying one
/// back to the cctui session that launched it.
fn rollout_link(path: &Path) -> RolloutLink {
    let Some(payload) = session_meta_payload(path) else {
        return RolloutLink { source: None, subagent_parent: None, launcher_parent: None };
    };
    let source = payload.get("source").and_then(super::thread_list::parse_source);
    let subagent_parent = super::thread_list::parse_parent(&payload);
    let launcher_parent = payload
        .get("originator")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix(LAUNCHER_ORIGINATOR_PREFIX))
        .filter(|s| !s.is_empty())
        .map(super::thread_list::canonical_id);
    RolloutLink { source, subagent_parent, launcher_parent }
}

/// Extract a canonical 8-4-4-4-12 hex UUID from a rollout filename stem such as
/// `rollout-2026-07-12T01-25-55-019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90` by scanning
/// hyphen-separated segments for the five-group UUID window.
fn uuid_from_stem(stem: &str) -> Option<String> {
    let segs: Vec<&str> = stem.split('-').collect();
    let is_hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    for w in segs.windows(5) {
        if [8, 4, 4, 4, 12] == [w[0].len(), w[1].len(), w[2].len(), w[3].len(), w[4].len()]
            && w.iter().all(|s| is_hex(s))
        {
            return Some(w.join("-").to_ascii_lowercase());
        }
    }
    None
}

/// Parse from `offset` to the last complete line, returning the events and the
/// offset that line ends at. A truncated trailing line never advances the
/// offset, so the next scan re-reads it whole.
fn read_new_lines(
    path: &Path,
    offset: u64,
    local_id: &str,
) -> std::io::Result<(Vec<AdapterEvent>, u64)> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len <= offset {
        return Ok((vec![], offset));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut new_offset = offset;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            break;
        }
        new_offset += n as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(parse_line(local_id, trimmed));
    }
    Ok((out, new_offset))
}

/// Re-read `path` from a window BEHIND `anchor`, realigned to a line boundary
/// so parsing never starts mid-line. The caller must not persist any offset
/// from this: it re-reads already-seen lines and relies on the server's
/// content-hash dedup to drop the duplicates and surface only real gaps.
fn reconcile_tail(path: &Path, local_id: &str, anchor: u64) -> std::io::Result<Vec<AdapterEvent>> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(vec![]);
    }
    let start = anchor.min(len).saturating_sub(RECONCILE_BACKUP_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    if start > 0 {
        let mut partial = String::new();
        reader.read_line(&mut partial)?;
        if !partial.ends_with('\n') {
            return Ok(vec![]);
        }
    }
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || !line.ends_with('\n') {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(parse_line(local_id, trimmed));
    }
    Ok(out)
}

fn parse_line(local_id: &str, line: &str) -> AdapterEvent {
    if let Ok(value) = serde_json::from_str::<Value>(line) {
        // `turn_context` rollout lines carry the model + reasoning effort the
        // session runs on. Surface them as a Status so discovered
        // (log-tailed) codex sessions render model/effort in the list, instead
        // of letting the line fall through as a meaningless "message".
        if value.get("type").and_then(Value::as_str) == Some("turn_context")
            && let Some(status) = turn_context_status(local_id, &value)
        {
            return status;
        }
        if let Some(usage) = token_usage_event(local_id, &value) {
            return usage;
        }
        // Heuristic: lines that look like tool calls.
        if value.get("tool").is_some()
            || value.get("function_call").is_some()
            || value.get("type").and_then(Value::as_str) == Some("tool_use")
        {
            return AdapterEvent::ToolUse { local_id: local_id.to_owned(), payload: value };
        }
        return AdapterEvent::Message {
            local_id: local_id.to_owned(),
            payload: value,
            turn_id: None,
        };
    }
    AdapterEvent::Message {
        local_id: local_id.to_owned(),
        payload: json!({"role": "assistant", "text": line}),
        turn_id: None,
    }
}

/// Extract model + reasoning effort from a `turn_context` rollout line and build
/// a `Status` event. The model lives at `payload.model`; effort at
/// `payload.collaboration_mode.settings.reasoning_effort` (newer codex) or a
/// top-level `payload.reasoning_effort` fallback — both may be null. Returns
/// `None` when neither is present so we don't emit an empty Status.
fn turn_context_status(local_id: &str, value: &Value) -> Option<AdapterEvent> {
    let p = value.get("payload")?;
    let str_at = |v: &Value, ptr: &str| {
        v.pointer(ptr).and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty())
    };
    let model = str_at(p, "/model");
    let effort = str_at(p, "/collaboration_mode/settings/reasoning_effort")
        .or_else(|| str_at(p, "/reasoning_effort"));
    if model.is_none() && effort.is_none() {
        return None;
    }
    Some(AdapterEvent::Status {
        local_id: local_id.to_owned(),
        tempo: None,
        state: None,
        detail: None,
        activity: None,
        name: None,
        intent: None,
        model,
        effort,
        permission_mode: None,
        children: vec![],
    })
}

/// Map a codex `event_msg`/`token_count` rollout line → [`AdapterEvent::TokenUsage`].
/// Codex writes one after every model response with
/// `info.last_token_usage` = that response's delta and `info.total_token_usage`
/// = the running session total. We emit the `last` delta so the server's
/// per-message SUM reconstructs the total, exactly like the app-server driver's
/// [`super::app_server`] `thread/tokenUsage/updated` mapping (`inputTokens`
/// includes the cached count, so subtract it for the non-cached/cached split
/// the claude + app-server adapters use).
///
/// `message_id` is derived from the line's own content — the timestamp plus the
/// strictly-monotonic cumulative total — so re-tailing the same rollout file
/// after a daemon restart re-emits identical ids and the server's
/// `ON CONFLICT (session_id, message_id) DO NOTHING` upsert refuses to
/// double-count. Returns `None` for non-token lines so `parse_line` continues.
fn token_usage_event(local_id: &str, value: &Value) -> Option<AdapterEvent> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let last = payload.pointer("/info/last_token_usage");
    let g = |k: &str| last.and_then(|l| l.get(k)).and_then(Value::as_u64).unwrap_or(0);
    let cached = g("cached_input_tokens");
    let cumulative = payload
        .pointer("/info/total_token_usage/total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let ts = value.get("timestamp").and_then(Value::as_str).unwrap_or("");
    Some(AdapterEvent::TokenUsage {
        local_id: local_id.to_owned(),
        message_id: format!("codex-tokens-{ts}-{cumulative}"),
        input_tokens: g("input_tokens").saturating_sub(cached),
        output_tokens: g("output_tokens"),
        cache_read_tokens: cached,
        cache_creation_tokens: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const ROLLOUT: &str = "rollout-2026-09-07T01-00-00-019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90";
    const ROLLOUT_ID: &str = "019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90";

    fn write_turns(path: &Path, range: std::ops::Range<usize>) {
        let mut f =
            std::fs::OpenOptions::new().create(true).append(true).open(path).expect("open rollout");
        for i in range {
            writeln!(f, r#"{{"role":"assistant","text":"turn {i}"}}"#).unwrap();
        }
    }

    fn texts(events: &[AdapterEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Message { payload, .. } => {
                    payload.get("text").and_then(Value::as_str).map(str::to_owned)
                }
                _ => None,
            })
            .collect()
    }

    fn drain(rx: &mut mpsc::Receiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut out = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            out.push(evt);
        }
        out
    }

    fn tail_with(
        sessions: &Path,
        offsets_path: Option<PathBuf>,
        tx: mpsc::Sender<AdapterEvent>,
    ) -> LogTail {
        LogTail::new(
            LogTailConfig {
                sessions_root: sessions.to_path_buf(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path,
            },
            tx,
            CancellationToken::new(),
        )
    }

    /// A daemon that kept tailing while its events went nowhere leaves the
    /// persisted offset past turns the server never stored. On restart the
    /// server's resume mark is the only evidence of where its copy stops, so
    /// the reconcile pass must replay from there — even though the rollout has
    /// not grown since.
    #[tokio::test]
    async fn resume_mark_replays_the_gap_a_dropped_connection_left() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let path = sessions.join(format!("{ROLLOUT}.jsonl"));
        let offsets_path = tmp.path().join("offsets.json");

        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = tail_with(&sessions, Some(offsets_path.clone()), tx);
        write_turns(&path, 0..2);
        tail.scan_once().await;
        let stored = drain(&mut rx);
        assert_eq!(texts(&stored), ["turn 0", "turn 1"]);
        let mark = match stored.last().expect("events") {
            AdapterEvent::TranscriptMark { offset, .. } => *offset,
            other => panic!("expected a transcript mark, got {other:?}"),
        };

        // the WS is down: these turns are tailed but never reach the server,
        // and the offset is flushed past them anyway.
        write_turns(&path, 2..4);
        tail.scan_once().await;
        drop(tail);
        drop(rx);

        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = tail_with(&sessions, Some(offsets_path), tx);
        tail.set_resume_marks(Arc::new(Mutex::new(HashMap::from([(ROLLOUT_ID.to_owned(), mark)]))));
        tail.scan_once().await;
        let healed = texts(&drain(&mut rx));
        assert!(
            healed.contains(&"turn 2".to_owned()) && healed.contains(&"turn 3".to_owned()),
            "the gap behind the mark must be replayed, got {healed:?}"
        );
    }

    /// The offset is a promise that everything before it was handed off. A
    /// failed send must leave it where it was so the next scan re-reads.
    #[tokio::test]
    async fn offset_does_not_advance_past_events_that_were_not_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let path = sessions.join(format!("{ROLLOUT}.jsonl"));
        let offsets_path = tmp.path().join("offsets.json");
        write_turns(&path, 0..3);

        let (tx, rx) = mpsc::channel(64);
        drop(rx);
        let mut tail = tail_with(&sessions, Some(offsets_path), tx);
        tail.scan_once().await;
        let key = path.to_string_lossy().into_owned();
        assert_eq!(tail.offsets.get(&key), 0, "a dropped send must not advance the offset");

        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = tail_with(&sessions, tail.cfg.offsets_path.clone(), tx);
        tail.scan_once().await;
        assert_eq!(texts(&drain(&mut rx)), ["turn 0", "turn 1", "turn 2"]);
    }

    #[tokio::test]
    async fn a_partial_trailing_line_is_re_read_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let path = sessions.join(format!("{ROLLOUT}.jsonl"));
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = tail_with(&sessions, None, tx);

        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"turn 0\"}\n{\"role\":\"assis")
            .unwrap();
        tail.scan_once().await;
        assert_eq!(texts(&drain(&mut rx)), ["turn 0"]);

        std::fs::write(
            &path,
            "{\"role\":\"assistant\",\"text\":\"turn 0\"}\n{\"role\":\"assistant\",\"text\":\"turn 1\"}\n",
        )
        .unwrap();
        tail.scan_once().await;
        assert_eq!(texts(&drain(&mut rx)), ["turn 1"]);
    }

    #[tokio::test]
    async fn detects_new_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let path = sessions.join("session-abc.jsonl");
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"hi\"}\n").unwrap();
        tail.scan_once().await;
        // Started + Message.
        let evt1 = rx.recv().await.unwrap();
        let evt2 = rx.recv().await.unwrap();
        assert!(matches!(evt1, AdapterEvent::SessionStarted { .. }));
        assert!(matches!(evt2, AdapterEvent::Message { .. }));
    }

    #[tokio::test]
    async fn quiesce_emits_hibernated_status() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_millis(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let path = sessions.join("s1.jsonl");
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"hi\"}\n").unwrap();
        tail.scan_once().await;
        drain(&mut rx);
        tokio::time::sleep(Duration::from_millis(20)).await;
        tail.scan_once().await;
        match rx.recv().await.unwrap() {
            AdapterEvent::Status { tempo, .. } => assert_eq!(tempo.as_deref(), Some("hibernated")),
            other => panic!("expected hibernated status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_lines_emit_tool_use() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let path = sessions.join("s1.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"tool":"shell","args":["ls"]}}"#).unwrap();
        tail.scan_once().await;
        rx.recv().await.unwrap(); // started
        let evt = rx.recv().await.unwrap();
        assert!(matches!(evt, AdapterEvent::ToolUse { .. }));
    }

    #[tokio::test]
    async fn inventory_discovered_session_still_tails_transcript() {
        // regression: a session whose rollout id was discovered by the
        // thread/list inventory must still get its real JSONL transcript tailed
        // here. Before the fix the log-tail skipped files whose stem matched an
        // inventory id, leaving the conversation empty ("No events yet").
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        // Rollout filename whose stem ends with the inventory-discovered id.
        let id = "019ea66a-cf6e-73b1";
        let path = sessions.join(format!("rollout-2026-{id}.jsonl"));
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"real transcript\"}\n").unwrap();
        tail.scan_once().await;
        let evt1 = rx.recv().await.unwrap();
        let evt2 = rx.recv().await.unwrap();
        assert!(matches!(evt1, AdapterEvent::SessionStarted { .. }));
        assert!(matches!(evt2, AdapterEvent::Message { .. }), "transcript must be tailed");
    }

    #[tokio::test]
    async fn app_server_owned_session_is_skipped() {
        // The app-server `owned` set is still honored — cctui drives those live.
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let registry = super::super::app_server::SessionRegistry::default();
        let id = "owned-019ea66a";
        registry.lock().await.insert(
            id.to_owned(),
            super::super::app_server::SessionRecord {
                cfg: super::super::app_server::AppServerConfig::default(),
                cwd: "/w".into(),
                name: None,
                env: std::collections::BTreeMap::new(),
                spawn_relay: false,
            },
        );
        tail.set_owned(registry);
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"x\"}\n").unwrap();
        tail.scan_once().await;
        assert!(rx.try_recv().is_err(), "owned rollout file must not be tailed");
    }

    #[tokio::test]
    async fn subagent_rollout_nests_under_dispatched_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let exec = "019f832c-6301-7053-8000-0000000000e1";
        crate::dispatch_codex::register_dispatch_thread(exec, "DISPATCH-LT-1");
        let child = "019f832c-6301-7053-8000-0000000000e2";
        let path = sessions.join(format!("rollout-{child}.jsonl"));
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{child}","cwd":"/workspace","source":{{"subAgent":{{"thread_spawn":{{"parent_thread_id":"{exec}"}}}}}}}}}}"#
            ),
        )
        .unwrap();
        tail.scan_once().await;
        let AdapterEvent::SessionStarted { local_id, meta } = rx.recv().await.unwrap() else {
            panic!("expected SessionStarted")
        };
        assert_eq!(local_id, child);
        assert_eq!(meta.parent_local_id.as_deref(), Some("DISPATCH-LT-1"));
        assert_eq!(meta.extra["subagent"], json!(true));
    }

    #[tokio::test]
    async fn snake_case_subagent_rollout_nests_under_its_parent() {
        // codex 0.153 writes `source.subagent` (snake_case) + top-level
        // `parent_thread_id`; the parent is a plain app-server session, so the
        // child must nest under it directly.
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let parent = "01a09a6a-a692-72e1-bdef-be54a77174b2";
        let child = "01a09a80-8a73-7051-82d5-68e3c0b97770";
        let path = sessions.join(format!("rollout-2026-09-13T13-21-50-{child}.jsonl"));
        std::fs::write(
            &path,
            format!(
                r#"{{"timestamp":"2026-09-13T11:21:50.456Z","type":"session_meta","payload":{{"session_id":"{parent}","id":"{child}","forked_from_id":"{parent}","parent_thread_id":"{parent}","cwd":"/workspace","originator":"cctui","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}","depth":1,"agent_path":"/root/noms_fondateur","agent_nickname":"Ohm","agent_role":null}}}}}},"thread_source":"subagent","agent_nickname":"Ohm"}}}}"#
            ),
        )
        .unwrap();
        tail.scan_once().await;
        let AdapterEvent::SessionStarted { local_id, meta } = rx.recv().await.unwrap() else {
            panic!("expected SessionStarted")
        };
        assert_eq!(local_id, child);
        assert_eq!(meta.parent_local_id.as_deref(), Some(parent));
        assert_eq!(meta.extra["subagent"], json!(true));
    }

    #[tokio::test]
    async fn orphan_subagent_rollout_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let child = "019f832c-6301-7053-8000-0000000000e3";
        let path = sessions.join(format!("rollout-{child}.jsonl"));
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{child}","source":{{"subAgent":"review"}}}}}}"#
            ),
        )
        .unwrap();
        tail.scan_once().await;
        assert!(rx.try_recv().is_err(), "orphan subagent rollout must be skipped");
    }

    async fn started_meta_for_exec(originator: &str, id: &str) -> SessionMeta {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"/workspace","source":"exec","originator":"{originator}"}}}}"#
            ),
        )
        .unwrap();
        tail.scan_once().await;
        let AdapterEvent::SessionStarted { meta, .. } = rx.recv().await.unwrap() else {
            panic!("expected SessionStarted")
        };
        meta
    }

    #[tokio::test]
    async fn exec_rollout_nests_under_stamped_launcher() {
        let id = "019f832c-6301-7053-8000-0000000000f1";
        let launcher = "356d4dde-659c-47c7-8a3c-aa4e5c44b50a";
        let meta = started_meta_for_exec(&format!("cctui-parent.{launcher}"), id).await;
        assert_eq!(meta.parent_local_id.as_deref(), Some(launcher));
        assert_eq!(meta.extra["subagent"], json!(true));
    }

    #[tokio::test]
    async fn unstamped_exec_rollout_stays_parentless() {
        let id = "019f832c-6301-7053-8000-0000000000f2";
        let meta = started_meta_for_exec("codex_exec", id).await;
        assert_eq!(meta.parent_local_id, None);
        assert_eq!(meta.extra.get("subagent"), None);
    }

    #[tokio::test]
    async fn self_referential_stamp_is_refused() {
        let id = "019f832c-6301-7053-8000-0000000000f3";
        let meta = started_meta_for_exec(&format!("cctui-parent.{id}"), id).await;
        assert_eq!(meta.parent_local_id, None, "a self-parent would cycle the heartbeat CTE");
    }

    #[tokio::test]
    async fn quiesced_rollout_is_not_replayed_on_rediscovery() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_millis(1),
                offsets_path: Some(tmp.path().join("offsets.json")),
            },
            tx,
            CancellationToken::new(),
        );
        let path = sessions.join("s1.jsonl");
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"hi\"}\n").unwrap();
        tail.scan_once().await;
        drain(&mut rx); // Started + Message + mark
        tokio::time::sleep(Duration::from_millis(20)).await;
        tail.scan_once().await;
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::Status { .. }));
        tail.scan_once().await;
        tail.scan_once().await;
        assert!(rx.try_recv().is_err(), "quiesced rollout must stay silent");
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{\"role\":\"assistant\",\"text\":\"more\"}}").unwrap();
        tail.scan_once().await;
        match rx.recv().await.unwrap() {
            AdapterEvent::Message { payload, .. } => {
                assert_eq!(payload["text"], json!("more"));
            }
            other => panic!("expected only the appended line, got {other:?}"),
        }
        assert!(texts(&drain(&mut rx)).is_empty());
    }

    #[tokio::test]
    async fn idle_rollout_hibernates_and_resumes_without_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions.clone(),
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_millis(1),
                offsets_path: Some(tmp.path().join("offsets.json")),
            },
            tx,
            CancellationToken::new(),
        );
        let path = sessions.join("s1.jsonl");
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"hi\"}\n").unwrap();
        tail.scan_once().await;
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::SessionStarted { .. }));
        assert!(matches!(rx.recv().await.unwrap(), AdapterEvent::Message { .. }));
        drain(&mut rx);

        tokio::time::sleep(Duration::from_millis(20)).await;
        tail.scan_once().await;
        match rx.recv().await.unwrap() {
            AdapterEvent::Status { tempo, .. } => assert_eq!(tempo.as_deref(), Some("hibernated")),
            other => panic!("an idle rollout must hibernate, got {other:?}"),
        }
        tail.scan_once().await;
        assert!(rx.try_recv().is_err(), "hibernation must be emitted once");

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{{\"role\":\"assistant\",\"text\":\"more\"}}").unwrap();
        tail.scan_once().await;
        match rx.recv().await.unwrap() {
            AdapterEvent::Message { payload, .. } => assert_eq!(payload["text"], json!("more")),
            other => panic!("expected only the appended line, got {other:?}"),
        }
        assert!(texts(&drain(&mut rx)).is_empty());

        std::fs::remove_file(&path).unwrap();
        tail.scan_once().await;
        assert!(
            matches!(rx.recv().await.unwrap(), AdapterEvent::SessionEnded { .. }),
            "a removed rollout is the one real end"
        );
    }

    #[tokio::test]
    async fn offsets_survive_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let offsets_path = Some(tmp.path().join("offsets.json"));
        let cfg = LogTailConfig {
            sessions_root: sessions.clone(),
            poll_interval: Duration::from_millis(10),
            quiesce: Duration::from_hours(1),
            offsets_path,
        };
        let path = sessions.join("s1.jsonl");
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"hi\"}\n").unwrap();
        {
            let (tx, mut rx) = mpsc::channel(64);
            let mut tail = LogTail::new(cfg.clone(), tx, CancellationToken::new());
            tail.scan_once().await;
            rx.recv().await.unwrap();
            rx.recv().await.unwrap();
        }
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(cfg, tx, CancellationToken::new());
        tail.scan_once().await;
        assert!(rx.try_recv().is_err(), "restart must not replay the rollout");
    }

    #[test]
    fn parse_line_handles_plain_text() {
        let evt = parse_line("s1", "hello world");
        assert!(matches!(evt, AdapterEvent::Message { .. }));
    }

    #[test]
    fn turn_context_line_emits_status_with_model_and_effort() {
        let line = r#"{"type":"turn_context","payload":{"model":"gpt-5.5","collaboration_mode":{"settings":{"reasoning_effort":"high"}}}}"#;
        match parse_line("s1", line) {
            AdapterEvent::Status { model, effort, .. } => {
                assert_eq!(model.as_deref(), Some("gpt-5.5"));
                assert_eq!(effort.as_deref(), Some("high"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn turn_context_with_null_effort_still_surfaces_model() {
        let line = r#"{"type":"turn_context","payload":{"model":"gpt-5.5","collaboration_mode":{"settings":{"reasoning_effort":null}}}}"#;
        match parse_line("s1", line) {
            AdapterEvent::Status { model, effort, .. } => {
                assert_eq!(model.as_deref(), Some("gpt-5.5"));
                assert_eq!(effort, None);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn turn_context_without_model_or_effort_falls_through_to_message() {
        let line = r#"{"type":"turn_context","payload":{"cwd":"/w"}}"#;
        assert!(matches!(parse_line("s1", line), AdapterEvent::Message { .. }));
    }

    const ROLLOUT_FIXTURE: &str = include_str!("fixtures/rollout_token_usage.jsonl");
    const HISTORY_FIXTURE: &str = include_str!("fixtures/rollout_history.jsonl");

    #[test]
    fn uuid_from_stem_extracts_canonical_uuid() {
        let stem = "rollout-2026-07-12T01-25-55-019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90";
        assert_eq!(uuid_from_stem(stem).as_deref(), Some("019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90"));
        assert_eq!(uuid_from_stem("no-uuid-here"), None);
    }

    #[test]
    fn derive_local_id_prefers_session_meta_over_filename() {
        let tmp = tempfile::tempdir().unwrap();
        // Filename UUID differs from the session_meta id to prove meta wins.
        let path = tmp
            .path()
            .join("rollout-2026-07-12T01-25-55-ffffffff-0000-7000-8000-000000000000.jsonl");
        std::fs::write(&path, HISTORY_FIXTURE).unwrap();
        assert_eq!(derive_local_id(&path), "019f5200-aaaa-7bbb-8ccc-000000000001");
    }

    #[test]
    fn derive_local_id_falls_back_to_filename_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("rollout-2026-07-12T01-25-55-019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90.jsonl");
        std::fs::write(&path, "{\"role\":\"assistant\",\"text\":\"no meta\"}\n").unwrap();
        assert_eq!(derive_local_id(&path), "019f51ff-f19f-7ed2-bf2a-bbb0d5cc5b90");
    }

    #[tokio::test]
    async fn recursive_scan_finds_nested_rollout_with_canonical_id() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        let nested = sessions.join("2026").join("07").join("12");
        std::fs::create_dir_all(&nested).unwrap();
        let path =
            nested.join("rollout-2026-07-12T01-25-55-ffffffff-0000-7000-8000-000000000000.jsonl");
        std::fs::write(&path, HISTORY_FIXTURE).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let mut tail = LogTail::new(
            LogTailConfig {
                sessions_root: sessions,
                poll_interval: Duration::from_millis(10),
                quiesce: Duration::from_hours(1),
                offsets_path: None,
            },
            tx,
            CancellationToken::new(),
        );
        tail.scan_once().await;
        let started = rx.recv().await.unwrap();
        match started {
            AdapterEvent::SessionStarted { local_id, .. } => {
                assert_eq!(local_id, "019f5200-aaaa-7bbb-8ccc-000000000001");
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        // Every subsequent event must carry the canonical id, not the filename.
        let mut saw_message = false;
        while let Ok(evt) = rx.try_recv() {
            let id = match &evt {
                AdapterEvent::Message { local_id, .. }
                | AdapterEvent::ToolUse { local_id, .. }
                | AdapterEvent::Status { local_id, .. }
                | AdapterEvent::TokenUsage { local_id, .. }
                | AdapterEvent::TranscriptMark { local_id, .. } => local_id.clone(),
                other => panic!("unexpected event {other:?}"),
            };
            assert_eq!(id, "019f5200-aaaa-7bbb-8ccc-000000000001");
            if matches!(evt, AdapterEvent::Message { .. }) {
                saw_message = true;
            }
        }
        assert!(saw_message, "nested rollout transcript must be tailed");
    }

    #[test]
    fn history_fixture_response_and_event_envelopes_parse() {
        let events: Vec<AdapterEvent> = HISTORY_FIXTURE
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| parse_line("hist", l.trim()))
            .collect();
        // token_count → TokenUsage, turn_context → Status, the rest → Message.
        assert_eq!(
            events.iter().filter(|e| matches!(e, AdapterEvent::TokenUsage { .. })).count(),
            1
        );
        assert_eq!(events.iter().filter(|e| matches!(e, AdapterEvent::Status { .. })).count(), 1);
        // The response_item / event_msg envelopes are preserved verbatim as
        // Message payloads so the server-side normalizer can unwrap them.
        let has_envelope = |t: &str| {
            events.iter().any(|e| {
                matches!(e,
                AdapterEvent::Message { payload, .. }
                    if payload.get("type").and_then(Value::as_str) == Some(t))
            })
        };
        assert!(has_envelope("response_item"));
        assert!(has_envelope("event_msg"));
    }

    #[test]
    fn token_count_line_emits_token_usage() {
        let line = r#"{"timestamp":"2026-05-30T07:37:04.869Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":23695},"last_token_usage":{"input_tokens":11860,"cached_input_tokens":9600,"output_tokens":214,"reasoning_output_tokens":117,"total_tokens":12074}}}}"#;
        match parse_line("sess", line) {
            AdapterEvent::TokenUsage {
                local_id,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                ..
            } => {
                assert_eq!(local_id, "sess");
                assert_eq!(input_tokens, 11860 - 9600);
                assert_eq!(output_tokens, 214);
                assert_eq!(cache_read_tokens, 9600);
                assert_eq!(cache_creation_tokens, 0);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn token_usage_message_id_is_stable_and_distinct_per_line() {
        let a = r#"{"timestamp":"2026-05-30T07:36:59.740Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":11621},"last_token_usage":{"input_tokens":11111,"cached_input_tokens":9600,"output_tokens":510,"total_tokens":11621}}}}"#;
        let b = r#"{"timestamp":"2026-05-30T07:37:04.869Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":23695},"last_token_usage":{"input_tokens":11860,"cached_input_tokens":9600,"output_tokens":214,"total_tokens":12074}}}}"#;
        let id = |line: &str| match parse_line("s", line) {
            AdapterEvent::TokenUsage { message_id, .. } => message_id,
            other => panic!("expected TokenUsage, got {other:?}"),
        };
        assert_eq!(id(a), id(a));
        assert_ne!(id(a), id(b));
    }

    #[test]
    fn fixture_rollout_accumulates_per_turn_token_usage() {
        let events: Vec<AdapterEvent> = ROLLOUT_FIXTURE
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| parse_line("fixture", l.trim()))
            .collect();
        let usages: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::TokenUsage {
                    message_id,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    ..
                } => Some((message_id.clone(), *input_tokens, *output_tokens, *cache_read_tokens)),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 3);
        let ids: HashSet<&String> = usages.iter().map(|(id, ..)| id).collect();
        assert_eq!(ids.len(), 3, "message ids must be unique per token_count line");
        let sum_in: u64 = usages.iter().map(|(_, i, ..)| i).sum();
        let sum_out: u64 = usages.iter().map(|(_, _, o, _)| o).sum();
        let sum_cache: u64 = usages.iter().map(|(.., c)| c).sum();
        assert_eq!(sum_in, (11111 - 9600) + (11860 - 9600) + (12134 - 10624));
        assert_eq!(sum_out, 510 + 214 + 61);
        assert_eq!(sum_cache, 9600 + 9600 + 10624);
        // token_count lines must NOT also surface as transcript messages.
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AdapterEvent::Message { payload, .. }
                    if payload.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
            )),
            "token_count lines must map to TokenUsage, not Message"
        );
    }
}
