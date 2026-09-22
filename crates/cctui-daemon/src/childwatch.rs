//! Live tracking of `CctuiAgent`-spawned child sessions.
//!
//! Completion means the child's TURN ended, not its process: the shared
//! [`cctui_proto::classifier`] decides (claude writes `activity: "success"`
//! to `state.json`; the codex driver emits a done `Status` on
//! `turn/completed` for spawned children; opencode one-shots end outright).
//! The finished/still-running policy lives in [`ChildSnapshot::assess`].
//!
//! A child is matched only by its pre-minted session id or by the
//! `spawn_key` an adapter that mints its own local id echoes into
//! `SessionMeta::extra`. Never bind by `parent_local_id`: observed sessions
//! (codex log-tail rollouts, claude Task subagents) also name the caller as
//! parent and would steal the watch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cctui_proto::adapter::{AdapterEvent, EndReason};
use cctui_proto::classifier::{Bucket, ClassifyInput, classify};
use tokio::sync::Notify;

/// After a done-classified status, how long to keep waiting for the final
/// assistant text to land (claude's transcript tail can lag the status poll).
pub const DONE_TEXT_GRACE: Duration = Duration::from_secs(20);
/// How long assistant text must stand as a child's last output before the turn
/// counts as over.
///
/// The claude driver emits a `Status` only when the polled snapshot *changes*,
/// so a follow-up turn running between two identical idle readings produces no
/// done status and the text is the only turn-end evidence there is. Text and
/// the tool call that follows it belong to one assistant response and arrive
/// together, so a gap this long is real.
pub const TEXT_QUIET_GRACE: Duration = Duration::from_secs(10);
/// Trust window for early done readings.
///
/// A done classification observed before the child was ever seen working is
/// distrusted until the watch is at least this old (a freshly dispatched
/// worker can briefly read as idle before its first status poll).
pub const QUIET_DONE_MIN_AGE: Duration = Duration::from_mins(1);

/// How a finished child is reported to the parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOutcome {
    /// The child's last assistant message, when it produced one.
    pub final_text: Option<String>,
    /// Failure text when the child never started or ended badly.
    pub error: Option<String>,
    /// The session id the child actually registered as (codex mints its own
    /// thread id). This is the id a follow-up message targets.
    pub local_id: Option<String>,
    /// The turn's last content was a thinking block: the model planned more
    /// work and stopped, so `final_text` is stale mid-turn narration.
    pub tail_is_thinking: bool,
}

/// Which event set `blocked`: a resolution clears only its own kind, so a
/// resolved permission cannot clear an ask/plan/status block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Status,
    Ask,
    Plan,
    Permission,
}

/// Everything observed about a watched child so far.
#[derive(Debug, Clone, Default)]
struct WatchState {
    local_id: Option<String>,
    final_text: Option<String>,
    last_tool: Option<String>,
    status_line: Option<String>,
    blocked: Option<String>,
    blocked_kind: Option<BlockKind>,
    saw_working: bool,
    done_since: Option<Instant>,
    ended: bool,
    error: Option<String>,
    tail_is_thinking: bool,
    /// When assistant text became the child's last output; cleared by anything
    /// proving the turn carried on.
    text_tail_at: Option<Instant>,
    tool_errors: u32,
    tokens: u64,
}

struct Watch {
    state: WatchState,
    registered_at: Instant,
    notify: Arc<Notify>,
}

/// A point-in-time copy of a watch, plus its age. All policy questions are
/// answered off this via [`ChildSnapshot::assess`].
#[derive(Debug, Clone)]
pub struct ChildSnapshot {
    pub local_id: Option<String>,
    pub final_text: Option<String>,
    pub last_tool: Option<String>,
    pub status_line: Option<String>,
    pub blocked: Option<String>,
    saw_working: bool,
    done_since: Option<Instant>,
    ended: bool,
    error: Option<String>,
    registered_at: Instant,
    tail_is_thinking: bool,
    text_tail_at: Option<Instant>,
    tool_errors: u32,
    tokens: u64,
}

/// What the tool handler should do with the current snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// The child finished (turn done or session over) — reply with this.
    Finished(ChildOutcome),
    /// Still running; the string is a human-readable progress line.
    Running(String),
}

impl ChildSnapshot {
    fn outcome(&self) -> ChildOutcome {
        ChildOutcome {
            final_text: self.final_text.clone(),
            error: self.error.clone(),
            local_id: self.local_id.clone(),
            tail_is_thinking: self.tail_is_thinking,
        }
    }

    /// Decide whether the child counts as finished right now.
    ///
    /// Finished when the session ended (or its spawn failed), or when a
    /// done-classified status has settled: immediately once the final text is
    /// in, after [`DONE_TEXT_GRACE`] without one, and never off a quiet early
    /// done reading (see [`QUIET_DONE_MIN_AGE`]) unless text already proves
    /// the turn ran.
    ///
    /// Also finished when assistant text has stood as the child's last output
    /// for [`TEXT_QUIET_GRACE`], which is all a turn between two identical
    /// deduped status polls ever produces.
    #[must_use]
    pub fn assess(&self, now: Instant) -> Assessment {
        if self.ended || self.error.is_some() {
            return Assessment::Finished(self.outcome());
        }
        // claude's control socket reports done while an AskUserQuestion /
        // ExitPlanMode prompt is pending: a blocked child is never done.
        if self.blocked.is_none()
            && let Some(done_at) = self.done_since
        {
            let trusted = self.saw_working
                || self.final_text.is_some()
                || now.duration_since(self.registered_at) >= QUIET_DONE_MIN_AGE;
            if trusted
                && (self.final_text.is_some() || now.duration_since(done_at) >= DONE_TEXT_GRACE)
            {
                return Assessment::Finished(self.outcome());
            }
        }
        if self.blocked.is_none()
            && !self.tail_is_thinking
            && let Some(text_at) = self.text_tail_at
            && now.duration_since(text_at) >= TEXT_QUIET_GRACE
        {
            return Assessment::Finished(self.outcome());
        }
        Assessment::Running(self.progress_line())
    }

    /// One line describing what the child is doing, streamed to the parent.
    fn progress_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(blocked) = &self.blocked {
            parts.push(format!("child needs input: {blocked}"));
        } else if let Some(status) = &self.status_line {
            parts.push(status.clone());
        } else if self.local_id.is_none() {
            parts.push("child starting".to_owned());
        } else {
            parts.push("child working".to_owned());
        }
        if let Some(tool) = &self.last_tool {
            parts.push(format!("tool: {tool}"));
        }
        match self.tool_errors {
            0 => {}
            1 => parts.push("last tool errored".to_owned()),
            n => parts.push(format!("{n} consecutive tool errors")),
        }
        if let Some(text) = &self.final_text {
            parts.push(format!("last message: {}", snippet(text, 160)));
        }
        if self.tokens > 0 {
            parts.push(format!("tokens: {}", humanize_tokens(self.tokens)));
        }
        parts.join(" · ")
    }
}

#[allow(clippy::cast_precision_loss)]
fn humanize_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// A registered watch: snapshot on demand, woken on every observed change.
pub struct WatchHandle {
    key: String,
    watch: Arc<ChildWatch>,
    notify: Arc<Notify>,
}

impl WatchHandle {
    #[must_use]
    pub fn snapshot(&self) -> Option<ChildSnapshot> {
        self.watch.snapshot(&self.key)
    }

    /// Wait for the next observed change, or until `timeout` elapses.
    pub async fn changed(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.notify.notified()).await;
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.watch.remove(&self.key);
    }
}

#[derive(Default)]
pub struct ChildWatch {
    watches: Mutex<HashMap<String, Watch>>,
}

static GLOBAL: OnceLock<Arc<ChildWatch>> = OnceLock::new();

/// The process-wide watcher: written by the supervisor's event pump, read by the
/// agent-tool handler.
pub fn global() -> Arc<ChildWatch> {
    GLOBAL.get_or_init(|| Arc::new(ChildWatch::default())).clone()
}

impl ChildWatch {
    /// Start watching the child `key` (its pre-minted session id).
    pub fn register(self: &Arc<Self>, key: &str) -> WatchHandle {
        self.insert(key, WatchState::default())
    }

    /// Watch an already-registered child (follow-up message): the local id is
    /// known up front, so events bind immediately and a stale done reading
    /// from before the follow-up cannot count as this turn's completion.
    pub fn register_bound(self: &Arc<Self>, key: &str) -> WatchHandle {
        self.insert(key, WatchState { local_id: Some(key.to_owned()), ..WatchState::default() })
    }

    fn insert(self: &Arc<Self>, key: &str, state: WatchState) -> WatchHandle {
        let notify = Arc::new(Notify::new());
        let watch = Watch { state, registered_at: Instant::now(), notify: notify.clone() };
        self.watches.lock().unwrap().insert(key.to_owned(), watch);
        WatchHandle { key: key.to_owned(), watch: self.clone(), notify }
    }

    fn remove(&self, key: &str) {
        self.watches.lock().unwrap().remove(key);
    }

    #[allow(clippy::significant_drop_tightening)]
    fn snapshot(&self, key: &str) -> Option<ChildSnapshot> {
        let guard = self.watches.lock().ok()?;
        let w = guard.get(key)?;
        Some(ChildSnapshot {
            local_id: w.state.local_id.clone(),
            final_text: w.state.final_text.clone(),
            last_tool: w.state.last_tool.clone(),
            status_line: w.state.status_line.clone(),
            blocked: w.state.blocked.clone(),
            saw_working: w.state.saw_working,
            done_since: w.state.done_since,
            ended: w.state.ended,
            error: w.state.error.clone(),
            registered_at: w.registered_at,
            tail_is_thinking: w.state.tail_is_thinking,
            text_tail_at: w.state.text_tail_at,
            tool_errors: w.state.tool_errors,
            tokens: w.state.tokens,
        })
    }

    /// Feed one adapter event. Cheap no-op while nothing is being watched.
    #[allow(clippy::cognitive_complexity, clippy::match_same_arms)]
    pub fn observe(&self, event: &AdapterEvent) {
        let Ok(mut guard) = self.watches.lock() else { return };
        if guard.is_empty() {
            return;
        }
        let watches = &mut *guard;
        match event {
            AdapterEvent::SessionStarted { local_id, meta } => {
                bind(watches, local_id, &meta.extra);
            }
            AdapterEvent::Message { local_id, payload, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    apply_message(w, payload);
                }
            }
            AdapterEvent::ToolUse { local_id, payload } => {
                if let Some(w) = find_mut(watches, local_id) {
                    apply_tool_use(w, payload);
                }
            }
            AdapterEvent::TokenUsage { local_id, input_tokens, output_tokens, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    w.state.tokens += input_tokens + output_tokens;
                }
            }
            AdapterEvent::Status { local_id, tempo, state, detail, activity, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    apply_status(
                        w,
                        tempo.as_deref(),
                        state.as_deref(),
                        detail.as_deref(),
                        activity.as_deref(),
                    );
                    w.notify.notify_waiters();
                }
            }
            AdapterEvent::AskQuestion { local_id, question, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    set_blocked(w, BlockKind::Ask, snippet(question, 200));
                }
            }
            AdapterEvent::PlanRequest { local_id, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    set_blocked(w, BlockKind::Plan, "plan pending approval".to_owned());
                }
            }
            AdapterEvent::PermissionRequest { local_id, tool, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    set_blocked(w, BlockKind::Permission, format!("permission: {tool}"));
                }
            }
            AdapterEvent::AskResolved { local_id } => {
                if let Some(w) = find_mut(watches, local_id) {
                    clear_blocked(w, BlockKind::Ask);
                }
            }
            AdapterEvent::PlanResolved { local_id } => {
                if let Some(w) = find_mut(watches, local_id) {
                    clear_blocked(w, BlockKind::Plan);
                }
            }
            AdapterEvent::PermissionResolved { local_id, .. } => {
                if let Some(w) = find_mut(watches, local_id) {
                    clear_blocked(w, BlockKind::Permission);
                }
            }
            AdapterEvent::SessionEnded { local_id, reason } => {
                if let Some(w) = find_mut(watches, local_id) {
                    w.state.ended = true;
                    match reason {
                        EndReason::Killed => {
                            w.state.error = Some("child session was killed".to_owned());
                        }
                        EndReason::Crashed { detail } | EndReason::Other { detail } => {
                            w.state.error = Some(detail.clone());
                        }
                        _ => {}
                    }
                    w.notify.notify_waiters();
                }
            }
            AdapterEvent::CommandResult { command_id, ok, error } if !ok => {
                let key = command_id.to_string();
                if let Some(w) = watches.get_mut(&key) {
                    w.state.ended = true;
                    w.state.error =
                        Some(error.clone().unwrap_or_else(|| "spawn failed".to_owned()));
                    w.notify.notify_waiters();
                }
            }
            // Listed to record that each was reviewed and is deliberately inert
            // for the watch; the wildcard below is forced by #[non_exhaustive].
            AdapterEvent::CommandResult { .. }
            | AdapterEvent::SessionModel { .. }
            | AdapterEvent::PrLink { .. }
            | AdapterEvent::Diagnose { .. }
            | AdapterEvent::CodexModels { .. }
            | AdapterEvent::PtyChunk { .. }
            | AdapterEvent::TranscriptMark { .. } => {}
            _ => {}
        }
    }
}

/// Fold a status update into the watch: classify it with the shared bucket
/// classifier so every adapter's "my turn is over" spelling lands the same way.
fn apply_status(
    w: &mut Watch,
    tempo: Option<&str>,
    state: Option<&str>,
    detail: Option<&str>,
    activity: Option<&str>,
) {
    let input = ClassifyInput { tempo, state, activity, ..ClassifyInput::default() };
    match classify(&input, &HashMap::new()) {
        Bucket::Working => {
            w.state.saw_working = true;
            w.state.done_since = None;
            w.state.text_tail_at = None;
            clear_blocked(w, BlockKind::Status);
        }
        Bucket::Blocked => {
            set_blocked(w, BlockKind::Status, detail.unwrap_or("waiting for input").to_owned());
        }
        Bucket::Done | Bucket::Review => {
            clear_blocked(w, BlockKind::Status);
            if w.state.done_since.is_none() {
                w.state.done_since = Some(Instant::now());
            }
        }
    }
    // Hibernation is terminal for a child: the worker is gone, nothing more
    // will stream. Whatever text we hold is the result.
    if tempo == Some("hibernated") || tempo == Some("dead") {
        w.state.ended = true;
    }
    w.state.status_line = detail
        .or(activity)
        .or(tempo)
        .or(state)
        .map(str::to_owned)
        .or_else(|| w.state.status_line.clone());
}

fn apply_message(w: &mut Watch, payload: &serde_json::Value) {
    if let Some(text) = assistant_text(payload) {
        w.state.final_text = Some(text);
        w.state.tail_is_thinking = false;
        w.state.text_tail_at = Some(Instant::now());
        w.notify.notify_waiters();
    } else if is_thinking_message(payload) {
        w.state.tail_is_thinking = true;
        w.state.text_tail_at = None;
        w.notify.notify_waiters();
    } else if is_turn_summary(payload) {
        apply_summary(w, payload);
    } else if is_user_message(payload) {
        // A new prompt (follow-up) starts a new turn: the previous final text
        // and done reading are stale.
        w.state.final_text = None;
        w.state.done_since = None;
        w.state.tail_is_thinking = false;
        w.state.text_tail_at = None;
        w.state.tool_errors = 0;
    }
}

fn apply_tool_use(w: &mut Watch, payload: &serde_json::Value) {
    if let Some(tool) = payload.get("tool").and_then(serde_json::Value::as_str) {
        w.state.last_tool = Some(tool.to_owned());
    }
    if let Some("tool_result" | "server_tool_result") =
        payload.get("kind").and_then(serde_json::Value::as_str)
    {
        if payload.get("is_error").and_then(serde_json::Value::as_bool).unwrap_or(false) {
            w.state.tool_errors += 1;
        } else {
            w.state.tool_errors = 0;
        }
    }
    // Tool traffic is proof of an in-flight turn.
    w.state.saw_working = true;
    w.state.done_since = None;
    w.state.text_tail_at = None;
    w.notify.notify_waiters();
}

fn set_blocked(w: &mut Watch, kind: BlockKind, reason: String) {
    w.state.blocked = Some(reason);
    w.state.blocked_kind = Some(kind);
    w.state.done_since = None;
    w.state.text_tail_at = None;
    w.notify.notify_waiters();
}

/// A done reading or text tail captured while the block was pending is stale,
/// so both clocks reset too.
fn clear_blocked(w: &mut Watch, kind: BlockKind) {
    if w.state.blocked_kind == Some(kind) {
        w.state.blocked = None;
        w.state.blocked_kind = None;
        w.state.done_since = None;
        w.state.text_tail_at = None;
        w.notify.notify_waiters();
    }
}

/// Bind `local_id` to its watch by exact key, else by the echoed `spawn_key` —
/// both pre-minted, so a bind can never land on a session the tool did not spawn.
fn bind(watches: &mut HashMap<String, Watch>, local_id: &str, extra: &serde_json::Value) {
    if let Some(w) = watches.get_mut(local_id) {
        w.state.local_id = Some(local_id.to_owned());
        w.notify.notify_waiters();
        return;
    }
    let Some(spawn_key) =
        extra.get("spawn_key").and_then(serde_json::Value::as_str).filter(|k| !k.is_empty())
    else {
        return;
    };
    if let Some(w) = watches.get_mut(spawn_key).filter(|w| w.state.local_id.is_none()) {
        w.state.local_id = Some(local_id.to_owned());
        w.notify.notify_waiters();
    }
}

fn find_mut<'a>(watches: &'a mut HashMap<String, Watch>, local_id: &str) -> Option<&'a mut Watch> {
    let key = watches
        .iter()
        .find(|(k, w)| w.state.local_id.as_deref() == Some(local_id) || k.as_str() == local_id)
        .map(|(k, _)| k.clone())?;
    watches.get_mut(&key)
}

/// The text of an assistant message, ignoring thinking blocks and user turns.
fn assistant_text(payload: &serde_json::Value) -> Option<String> {
    let assistant = payload.get("role").and_then(serde_json::Value::as_str).map_or_else(
        || {
            matches!(
                payload.get("type").and_then(serde_json::Value::as_str),
                Some("agentMessage" | "agent_message")
            )
        },
        |role| role == "assistant",
    );
    if !assistant {
        return None;
    }
    let text = payload.get("text").and_then(serde_json::Value::as_str)?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// A human-authored message (a follow-up prompt landing in the child).
/// Excludes `meta:true` user turns (system-reminders, task-notifications,
/// hook feedback): injected content is not a new human turn.
fn is_user_message(payload: &serde_json::Value) -> bool {
    if payload.get("meta").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        return false;
    }
    payload.get("role").and_then(serde_json::Value::as_str) == Some("user")
        || matches!(
            payload.get("type").and_then(serde_json::Value::as_str),
            Some("userMessage" | "user_message")
        )
}

fn is_thinking_message(payload: &serde_json::Value) -> bool {
    matches!(
        payload.get("role").and_then(serde_json::Value::as_str),
        Some("assistant_thinking" | "assistant_redacted_thinking")
    )
}

fn is_turn_summary(payload: &serde_json::Value) -> bool {
    payload.get("role").and_then(serde_json::Value::as_str) == Some("summary")
}

/// Fold a `post_turn_summary` in: `needs_action` (bool or non-empty text)
/// blocks the child; otherwise the summary is a turn-end hint.
fn apply_summary(w: &mut Watch, payload: &serde_json::Value) {
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let needs_action = match payload.get("needs_action") {
        Some(serde_json::Value::Bool(true)) => {
            Some(text("status_detail").unwrap_or_else(|| "needs action".to_owned()))
        }
        Some(serde_json::Value::String(_)) => text("needs_action"),
        _ => None,
    };
    if let Some(reason) = needs_action {
        set_blocked(w, BlockKind::Status, reason);
    } else if w.state.done_since.is_none() {
        w.state.done_since = Some(Instant::now());
        w.notify.notify_waiters();
    }
}

/// First `max` characters of `text` on one line, ellipsised.
#[must_use]
pub fn snippet(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cctui_proto::adapter::SessionMeta;
    use serde_json::json;

    fn started(local_id: &str, parent: Option<&str>) -> AdapterEvent {
        AdapterEvent::SessionStarted {
            local_id: local_id.to_owned(),
            meta: SessionMeta {
                parent_local_id: parent.map(str::to_owned),
                ..SessionMeta::default()
            },
        }
    }

    fn started_with_spawn_key(local_id: &str, spawn_key: &str) -> AdapterEvent {
        AdapterEvent::SessionStarted {
            local_id: local_id.to_owned(),
            meta: SessionMeta {
                extra: json!({ "spawn_key": spawn_key }),
                ..SessionMeta::default()
            },
        }
    }

    fn msg(local_id: &str, role: &str, text: &str) -> AdapterEvent {
        AdapterEvent::Message {
            local_id: local_id.to_owned(),
            payload: json!({ "role": role, "text": text }),
            turn_id: None,
        }
    }

    fn status(
        local_id: &str,
        tempo: Option<&str>,
        state: Option<&str>,
        activity: Option<&str>,
    ) -> AdapterEvent {
        AdapterEvent::Status {
            local_id: local_id.to_owned(),
            tempo: tempo.map(str::to_owned),
            state: state.map(str::to_owned),
            detail: None,
            activity: activity.map(str::to_owned),
            name: None,
            intent: None,
            model: None,
            effort: None,
            permission_mode: None,
            children: Vec::new(),
        }
    }

    fn ended(local_id: &str) -> AdapterEvent {
        AdapterEvent::SessionEnded { local_id: local_id.to_owned(), reason: EndReason::Completed }
    }

    fn finished(handle: &WatchHandle) -> Option<ChildOutcome> {
        match handle.snapshot().unwrap().assess(Instant::now()) {
            Assessment::Finished(o) => Some(o),
            Assessment::Running(_) => None,
        }
    }

    #[test]
    fn session_end_returns_the_last_assistant_text() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", Some("parent-1")));
        watch.observe(&msg("child-1", "assistant", "first pass"));
        watch.observe(&msg("child-1", "assistant", "VERDICT: approve"));
        watch.observe(&ended("child-1"));
        let out = finished(&h).unwrap();
        assert_eq!(out.final_text.as_deref(), Some("VERDICT: approve"));
        assert!(out.error.is_none());
        assert_eq!(out.local_id.as_deref(), Some("child-1"));
    }

    #[test]
    fn turn_done_with_text_finishes_without_session_end() {
        // The claude fleet shape: the worker finishes its prompt and idles.
        // state.json flips activity to success; the session never ends.
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), Some("working"), None));
        watch.observe(&msg("child-1", "assistant", "report: all good"));
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        let out = finished(&h).expect("done status + text must finish the watch");
        assert_eq!(out.final_text.as_deref(), Some("report: all good"));
    }

    #[test]
    fn quiet_done_before_any_work_is_distrusted() {
        // A freshly dispatched worker can read idle/done before its first
        // real status. Without text or working-evidence that must NOT finish.
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", None, Some("done"), None));
        assert!(finished(&h).is_none(), "early quiet done must not finish");
    }

    #[test]
    fn done_without_text_finishes_after_the_grace_window() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), None, None));
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        assert!(finished(&h).is_none(), "inside the grace window");
        let snap = h.snapshot().unwrap();
        let later = Instant::now() + DONE_TEXT_GRACE + Duration::from_secs(1);
        assert!(matches!(snap.assess(later), Assessment::Finished(_)));
    }

    #[test]
    fn renewed_work_clears_a_stale_done_reading() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), None, None));
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        watch.observe(&status("child-1", Some("active"), Some("working"), None));
        let snap = h.snapshot().unwrap();
        let later = Instant::now() + DONE_TEXT_GRACE * 2;
        assert!(
            matches!(snap.assess(later), Assessment::Running(_)),
            "renewed activity must clear the done reading"
        );
    }

    #[test]
    fn follow_up_user_message_resets_text_and_done() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register_bound("child-1");
        watch.observe(&status("child-1", Some("active"), None, None));
        watch.observe(&msg("child-1", "assistant", "turn one answer"));
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        // Follow-up prompt lands: previous answer is stale.
        watch.observe(&msg("child-1", "user", "and now do this"));
        assert!(finished(&h).is_none(), "new turn must reopen the watch");
        watch.observe(&status("child-1", Some("active"), None, None));
        watch.observe(&msg("child-1", "assistant", "turn two answer"));
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        assert_eq!(finished(&h).unwrap().final_text.as_deref(), Some("turn two answer"));
    }

    #[test]
    fn a_follow_up_turn_ends_on_text_then_silence_with_no_status_event_at_all() {
        // The claude driver dedupes an unchanged status poll: a child idle
        // before the follow-up and idle after it emits no done status for the
        // turn in between, so the text is the only evidence there is.
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register_bound("child-1");
        watch.observe(&msg("child-1", "user", "Reply with exactly the word: pong"));
        watch.observe(&msg("child-1", "assistant", "pong"));
        assert!(finished(&h).is_none(), "the turn stays open inside the quiet grace");
        let snap = h.snapshot().unwrap();
        let later = Instant::now() + TEXT_QUIET_GRACE + Duration::from_secs(1);
        let Assessment::Finished(out) = snap.assess(later) else {
            panic!("silence after the answer must end the turn")
        };
        assert_eq!(out.final_text.as_deref(), Some("pong"));
        assert!(out.error.is_none());
    }

    #[test]
    fn a_turn_landing_as_the_watch_attaches_is_not_finished_by_the_previous_done_reading() {
        // The stale done status from the turn before the follow-up arrives
        // just after register_bound; only the new turn's text may finish it.
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register_bound("child-1");
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        let snap = h.snapshot().unwrap();
        let later = Instant::now() + TEXT_QUIET_GRACE + DONE_TEXT_GRACE;
        assert!(
            matches!(snap.assess(later), Assessment::Running(_)),
            "a done reading predating the follow-up must not answer it"
        );
        watch.observe(&msg("child-1", "user", "now say pong"));
        watch.observe(&msg("child-1", "assistant", "pong"));
        let snap = h.snapshot().unwrap();
        let later = Instant::now() + TEXT_QUIET_GRACE + Duration::from_secs(1);
        let Assessment::Finished(out) = snap.assess(later) else { panic!("the new turn ended") };
        assert_eq!(out.final_text.as_deref(), Some("pong"));
    }

    #[test]
    fn text_that_is_not_the_tail_never_ends_a_turn_on_silence() {
        let watch = Arc::new(ChildWatch::default());
        let later = || Instant::now() + TEXT_QUIET_GRACE * 3;

        let tooling = watch.register_bound("child-1");
        watch.observe(&msg("child-1", "assistant", "let me check the tests"));
        watch.observe(&AdapterEvent::ToolUse {
            local_id: "child-1".into(),
            payload: json!({ "tool": "Bash" }),
        });
        assert!(
            matches!(tooling.snapshot().unwrap().assess(later()), Assessment::Running(_)),
            "a tool call after the text proves the turn carried on"
        );

        let thinking = watch.register_bound("child-2");
        watch.observe(&msg("child-2", "assistant", "let me check the tests"));
        watch.observe(&msg("child-2", "assistant_thinking", "which test first?"));
        assert!(
            matches!(thinking.snapshot().unwrap().assess(later()), Assessment::Running(_)),
            "a thinking tail is mid-turn narration, not an answer"
        );

        let working = watch.register_bound("child-3");
        watch.observe(&msg("child-3", "assistant", "let me check the tests"));
        watch.observe(&status("child-3", Some("active"), Some("working"), None));
        assert!(
            matches!(working.snapshot().unwrap().assess(later()), Assessment::Running(_)),
            "a working status after the text reopens the turn"
        );

        let asked = watch.register_bound("child-4");
        watch.observe(&msg("child-4", "assistant", "Before I continue:"));
        watch.observe(&AdapterEvent::AskQuestion {
            local_id: "child-4".into(),
            question: "which database?".into(),
            questions: None,
            preamble: None,
        });
        assert!(
            matches!(asked.snapshot().unwrap().assess(later()), Assessment::Running(_)),
            "a pending question is not a finished turn"
        );
    }

    #[test]
    fn hibernated_child_is_terminal_with_whatever_text_arrived() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), None, None));
        watch.observe(&msg("child-1", "assistant", "partial findings"));
        watch.observe(&status("child-1", Some("hibernated"), None, None));
        let out = finished(&h).unwrap();
        assert_eq!(out.final_text.as_deref(), Some("partial findings"));
    }

    #[test]
    fn blocked_child_reports_needs_input_in_progress() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("blocked"), None, None));
        match h.snapshot().unwrap().assess(Instant::now()) {
            Assessment::Running(line) => assert!(line.contains("needs input"), "{line}"),
            Assessment::Finished(_) => panic!("blocked is not finished"),
        }
    }

    fn running_line(handle: &WatchHandle) -> String {
        match handle.snapshot().unwrap().assess(Instant::now()) {
            Assessment::Running(line) => line,
            Assessment::Finished(o) => panic!("expected running, finished with {o:?}"),
        }
    }

    #[test]
    fn ask_question_blocks_even_while_status_reads_done() {
        // claude's control socket reports state:"done" while AskUserQuestion
        // is pending — the question must keep the watch open, not finish it
        // with the preamble prose as the answer.
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), Some("working"), None));
        watch.observe(&msg("child-1", "assistant", "I checked the repo. Before I continue:"));
        watch.observe(&AdapterEvent::AskQuestion {
            local_id: "child-1".into(),
            question: "Which database should the migration target?".into(),
            questions: None,
            preamble: None,
        });
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        let snap = h.snapshot().unwrap();
        let later = Instant::now() + DONE_TEXT_GRACE * 2;
        match snap.assess(later) {
            Assessment::Running(line) => {
                assert!(line.contains("child needs input"), "{line}");
                assert!(line.contains("Which database"), "{line}");
            }
            Assessment::Finished(o) => panic!("pending question must not finish: {o:?}"),
        }
    }

    #[test]
    fn long_questions_are_snippeted_in_the_progress_line() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&AdapterEvent::AskQuestion {
            local_id: "child-1".into(),
            question: "x".repeat(500),
            questions: None,
            preamble: None,
        });
        let line = running_line(&h);
        assert!(line.contains('…'), "{line}");
        assert!(line.chars().count() < 300, "{line}");
    }

    #[test]
    fn plan_and_permission_requests_block_until_resolved() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&AdapterEvent::PlanRequest {
            local_id: "child-1".into(),
            plan: "1. do the thing".into(),
            preamble: None,
        });
        assert!(running_line(&h).contains("plan pending approval"));
        watch.observe(&AdapterEvent::PlanResolved { local_id: "child-1".into() });
        assert!(h.snapshot().unwrap().blocked.is_none());

        watch.observe(&AdapterEvent::PermissionRequest {
            local_id: "child-1".into(),
            request_id: "req-1".into(),
            tool: "Bash".into(),
            input: json!(null),
        });
        assert!(running_line(&h).contains("permission: Bash"));
        watch.observe(&AdapterEvent::PermissionResolved {
            local_id: "child-1".into(),
            request_id: "req-1".into(),
        });
        assert!(h.snapshot().unwrap().blocked.is_none());
    }

    #[test]
    fn a_resolution_only_clears_a_block_of_its_own_kind() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&AdapterEvent::AskQuestion {
            local_id: "child-1".into(),
            question: "pick one".into(),
            questions: None,
            preamble: None,
        });
        watch.observe(&AdapterEvent::PermissionResolved {
            local_id: "child-1".into(),
            request_id: "req-1".into(),
        });
        assert!(running_line(&h).contains("pick one"));
        watch.observe(&AdapterEvent::AskResolved { local_id: "child-1".into() });
        assert!(h.snapshot().unwrap().blocked.is_none());
    }

    #[test]
    fn resolving_a_question_discards_the_done_reading_seen_while_pending() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), Some("working"), None));
        watch.observe(&msg("child-1", "assistant", "preamble prose"));
        watch.observe(&AdapterEvent::AskQuestion {
            local_id: "child-1".into(),
            question: "pick one".into(),
            questions: None,
            preamble: None,
        });
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        watch.observe(&AdapterEvent::AskResolved { local_id: "child-1".into() });
        assert!(finished(&h).is_none(), "the pending-window done reading is stale");
        watch.observe(&msg("child-1", "assistant", "real answer"));
        watch.observe(&status("child-1", None, Some("done"), Some("success")));
        assert_eq!(finished(&h).unwrap().final_text.as_deref(), Some("real answer"));
    }

    #[test]
    fn killed_child_finishes_with_a_killed_error() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&msg("child-1", "assistant", "half-done text"));
        watch.observe(&AdapterEvent::SessionEnded {
            local_id: "child-1".into(),
            reason: EndReason::Killed,
        });
        let out = finished(&h).unwrap();
        assert!(out.error.as_deref().unwrap().contains("killed"), "{out:?}");
    }

    #[test]
    fn child_with_its_own_local_id_is_matched_by_spawn_key() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-key");
        watch.observe(&started_with_spawn_key("ses_abc", "child-key"));
        watch.observe(&msg("ses_abc", "assistant", "done"));
        watch.observe(&ended("ses_abc"));
        let out = finished(&h).unwrap();
        assert_eq!(out.final_text.as_deref(), Some("done"));
        assert_eq!(out.local_id.as_deref(), Some("ses_abc"), "reply must carry the real id");
    }

    #[test]
    fn observed_session_naming_the_same_parent_cannot_steal_the_watch() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-key");
        watch.observe(&started("019fca47-rollout", Some("parent-1")));
        watch.observe(&ended("019fca47-rollout"));
        assert!(finished(&h).is_none(), "watch must survive the observed session's end");

        watch.observe(&started_with_spawn_key("ses_real", "child-key"));
        watch.observe(&msg("ses_real", "assistant", "VERDICT: approve"));
        watch.observe(&ended("ses_real"));
        assert_eq!(finished(&h).unwrap().final_text.as_deref(), Some("VERDICT: approve"));
    }

    #[test]
    fn a_bound_watch_ignores_later_spawn_key_rebinds() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-key");
        watch.observe(&started_with_spawn_key("ses_first", "child-key"));
        watch.observe(&started_with_spawn_key("ses_second", "child-key"));
        watch.observe(&msg("ses_first", "assistant", "from first"));
        watch.observe(&ended("ses_first"));
        assert_eq!(finished(&h).unwrap().final_text.as_deref(), Some("from first"));
    }

    #[test]
    fn unrelated_sessions_never_finish_a_watch() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("someone-else", Some("other-parent")));
        watch.observe(&msg("someone-else", "assistant", "not mine"));
        watch.observe(&ended("someone-else"));
        assert!(finished(&h).is_none(), "watch must still be pending");
    }

    #[test]
    fn crashed_child_reports_the_failure_detail() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&AdapterEvent::SessionEnded {
            local_id: "child-1".into(),
            reason: EndReason::Crashed { detail: "binary missing".into() },
        });
        assert_eq!(finished(&h).unwrap().error.as_deref(), Some("binary missing"));
    }

    #[test]
    fn failed_spawn_command_finishes_immediately() {
        let watch = Arc::new(ChildWatch::default());
        let id = uuid::Uuid::new_v4();
        let h = watch.register(&id.to_string());
        watch.observe(&AdapterEvent::CommandResult {
            command_id: id,
            ok: false,
            error: Some("adapter offline".into()),
        });
        assert_eq!(finished(&h).unwrap().error.as_deref(), Some("adapter offline"));
    }

    #[test]
    fn dropping_the_handle_unregisters_the_watch() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        drop(h);
        assert!(watch.snapshot("child-1").is_none());
    }

    #[test]
    fn progress_line_carries_tool_and_snippet() {
        let watch = Arc::new(ChildWatch::default());
        let h = watch.register("child-1");
        watch.observe(&started("child-1", None));
        watch.observe(&status("child-1", Some("active"), None, None));
        watch.observe(&AdapterEvent::ToolUse {
            local_id: "child-1".into(),
            payload: json!({ "tool": "Bash" }),
        });
        watch.observe(&msg("child-1", "assistant", "looking at the diff now"));
        match h.snapshot().unwrap().assess(Instant::now()) {
            Assessment::Running(line) => {
                assert!(line.contains("tool: Bash"), "{line}");
                assert!(line.contains("looking at the diff"), "{line}");
            }
            Assessment::Finished(_) => panic!("still running"),
        }
    }

    #[test]
    fn thinking_and_user_messages_are_not_the_final_text() {
        assert!(assistant_text(&json!({ "role": "assistant_thinking", "text": "hmm" })).is_none());
        assert!(assistant_text(&json!({ "role": "user", "text": "go" })).is_none());
        assert!(assistant_text(&json!({ "role": "assistant", "text": "  " })).is_none());
        assert_eq!(
            assistant_text(&json!({ "role": "assistant", "text": " ok " })).as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn codex_native_agent_messages_are_final_text() {
        assert_eq!(
            assistant_text(&json!({ "type": "agentMessage", "text": "done" })).as_deref(),
            Some("done")
        );
        assert!(assistant_text(&json!({ "type": "userMessage", "text": "go" })).is_none());
        assert!(assistant_text(&json!({ "type": "reasoning", "text": "hmm" })).is_none());
    }

    #[test]
    fn snippet_folds_whitespace_and_truncates() {
        assert_eq!(snippet("a  b\n\nc", 100), "a b c");
        assert_eq!(snippet("abcdef", 3), "abc…");
    }
}
