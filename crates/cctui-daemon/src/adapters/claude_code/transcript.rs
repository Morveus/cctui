//! Tail the standard Claude transcript at
//! `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`.
//!
//! The `subscribe` op (deferred) and the `timeline.jsonl` daemon log
//! both lack tool-call detail (§6.2 of the protocol doc). The transcript
//! here is the canonical source for `tool_use` blocks, agent text, and
//! `post_turn_summary` events.
//!
//! Design choice: rather than running a separate `notify` watcher per
//! session, we tail incrementally on the driver's 2s poll tick using a
//! persisted byte offset. The transcript file grows monotonically (only
//! `/clear` truncates), so seek-and-read-new is sufficient and avoids
//! the rename / debounce complications a watcher would carry.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use cctui_proto::adapter::AdapterEvent;
use serde_json::{Value, json};

/// Encode a working directory into the path-segment Claude uses under
/// `~/.claude/projects/`. Per protocol §6.1: replace every `/` AND every
/// `.` with `-`. Empirically `/.` collapses to `--`.
///
/// Claude normalizes the cwd before deriving this segment: a trailing slash
/// is dropped, so `/a/b/` and `/a/b` map to the SAME `<encoded>` dir. We must
/// match that. A cctui-dispatched session whose `working_dir` carried a
/// trailing slash (the UI sends `/home/you/proj/`) otherwise encodes to
/// `-home-you-proj-` (an extra trailing dash) — a directory that never exists.
/// `tail_once` then treats the missing file as silent success (`NotFound` →
/// empty), so the session shows live status but "No events yet" forever.
#[must_use]
pub fn encode_cwd(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .chars()
        .map(|c| match c {
            '/' | '.' => '-',
            other => other,
        })
        .collect()
}

/// Resolve the transcript path for a session.
#[must_use]
pub fn transcript_path(projects_root: &Path, cwd: &str, session_id: &str) -> PathBuf {
    projects_root.join(encode_cwd(cwd)).join(format!("{session_id}.jsonl"))
}

/// Newest `<session_id>.jsonl` across every project dir under `projects_root`.
/// `EnterWorktree` moves cwd, so claude relocates the session's transcript to a
/// new project-slug dir and the launch-cwd path stops existing; resolve the live
/// file by session id across dirs and pick the most recently modified so the
/// daemon can follow the move instead of tailing a dead path.
#[must_use]
pub fn newest_transcript_for_session(projects_root: &Path, session_id: &str) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(projects_root).ok()?.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let candidate = entry.path().join(&file_name);
        let Ok(meta) = std::fs::metadata(&candidate) else { continue };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(best_mtime, _)| mtime > *best_mtime) {
            best = Some((mtime, candidate));
        }
    }
    best.map(|(_, path)| path)
}

/// Directory holding per-subagent (Task-tool) transcripts for a parent
/// session: `<encoded-cwd>/<parent-session-id>/subagents/`. Derived from
/// the parent's own transcript path `<encoded-cwd>/<parent-session-id>.jsonl`
/// by stripping the `.jsonl` extension and descending into `subagents/`.
#[must_use]
pub fn subagents_dir(parent_transcript: &Path) -> PathBuf {
    parent_transcript.with_extension("").join("subagents")
}

/// Workflow-tool context for a subagent transcript discovered under
/// `subagents/workflows/<runId>/`. The Task tool's flat
/// `subagents/agent-*.jsonl` layout has no workflow, so this is `None` there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowContext {
    /// The workflow run id (e.g. `wf_fab6efd5-4bf`), = the `<runId>` dir name.
    pub run_id: String,
    /// Human workflow name (e.g. `deep-research`) from `workflows/<runId>.json`,
    /// if resolvable.
    pub name: Option<String>,
}

/// The `.meta.json` sidecar Claude writes next to every subagent transcript —
/// flat Task-tool agents included. `description` is the human label the Task
/// call was given; `tool_use_id` correlates the agent back to the parent's
/// `Task` tool call exactly, with no positional guesswork.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentMeta {
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub tool_use_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub spawn_depth: Option<u64>,
}

/// A discovered subagent transcript: its agent id, transcript path, and (for
/// Workflow-tool agents) the enclosing workflow run context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentEntry {
    pub agent_id: String,
    pub path: PathBuf,
    pub workflow: Option<WorkflowContext>,
    /// Sidecar meta, when the agent has a readable `.meta.json`.
    pub meta: Option<SubagentMeta>,
}

/// Discover every subagent transcript reachable from a parent session's
/// `subagents/` dir. Covers two layouts:
///
/// 1. Task tool — flat `subagents/agent-<agentId>.jsonl`.
/// 2. Workflow tool — nested `subagents/workflows/<runId>/agent-<agentId>.jsonl`,
///    with per-agent `.meta.json` (`agentType`) and a run-state
///    `workflows/<runId>.json` one level up under the session dir carrying the
///    workflow name. Nested `workflow()` calls reuse the same dir shape, so the
///    single-level glob below covers them too.
///
/// A missing directory (the common case — most sessions spawn no subagents)
/// yields an empty vec. The `agentId` is the stable subagent identifier Claude
/// assigns (also the id the Agent tool returns).
#[must_use]
pub fn discover_subagents(dir: &Path) -> Vec<SubagentEntry> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some(agent_id) = name.strip_prefix("agent-").and_then(strip_jsonl) {
            let meta = subagent_meta(dir, agent_id);
            out.push(SubagentEntry { agent_id: agent_id.to_owned(), path, workflow: None, meta });
        } else if name == "workflows" && path.is_dir() {
            discover_workflow_subagents(&path, &mut out);
        }
    }
    out
}

fn strip_jsonl(name: &str) -> Option<&str> {
    name.strip_suffix(".jsonl")
}

/// Scan `subagents/workflows/` — one `<runId>/` dir per workflow run, each
/// holding `agent-<id>.jsonl` transcripts (+ `.meta.json` sidecars).
fn discover_workflow_subagents(workflows_dir: &Path, out: &mut Vec<SubagentEntry>) {
    let Ok(runs) = std::fs::read_dir(workflows_dir) else {
        return;
    };
    for run in runs.flatten() {
        let run_dir = run.path();
        if !run_dir.is_dir() {
            continue;
        }
        let Some(run_id) = run_dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let name = workflow_name(workflows_dir, &run_id);
        let Ok(agents) = std::fs::read_dir(&run_dir) else {
            continue;
        };
        for agent in agents.flatten() {
            let path = agent.path();
            let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(agent_id) = fname.strip_prefix("agent-").and_then(strip_jsonl) else {
                continue;
            };
            let meta = subagent_meta(&run_dir, agent_id);
            out.push(SubagentEntry {
                agent_id: agent_id.to_owned(),
                path: path.clone(),
                workflow: Some(WorkflowContext { run_id: run_id.clone(), name: name.clone() }),
                meta,
            });
        }
    }
}

/// Read the workflow name from the run-state file
/// `<session-dir>/workflows/<runId>.json` (one level up from the subagents
/// `workflows/` dir). The run state lives under `<session>/workflows/`, while
/// transcripts live under `<session>/subagents/workflows/` — siblings of the
/// session dir.
fn workflow_name(subagents_workflows_dir: &Path, run_id: &str) -> Option<String> {
    // subagents_workflows_dir = <session>/subagents/workflows
    // run state              = <session>/workflows/<runId>.json
    let session_dir = subagents_workflows_dir.parent()?.parent()?;
    let run_state = session_dir.join("workflows").join(format!("{run_id}.json"));
    let bytes = std::fs::read(run_state).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/script/name").and_then(Value::as_str))
        .or_else(|| value.pointer("/script/meta/name").and_then(Value::as_str))
        .map(str::to_owned)
}

/// Read an agent's `.meta.json` sidecar (`<dir>/agent-<id>.meta.json`).
/// `Some` means the sidecar told us something usable; everything else —
/// missing, unreadable, malformed, not an object, or an object holding none
/// of these fields at a type we can use — is `None`. Discovery never fails
/// on a bad sidecar.
fn subagent_meta(dir: &Path, agent_id: &str) -> Option<SubagentMeta> {
    let bytes = std::fs::read(dir.join(format!("agent-{agent_id}.meta.json"))).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let meta = SubagentMeta {
        agent_type: text("agentType"),
        description: text("description"),
        tool_use_id: text("toolUseId"),
        parent_agent_id: text("parentAgentId"),
        spawn_depth: value.get("spawnDepth").and_then(Value::as_u64),
    };
    (meta != SubagentMeta::default()).then_some(meta)
}

/// Read new lines from `path` starting at `offset`. Returns the parsed
/// events plus the new offset. If the file is shorter than `offset`
/// (rotation / `/clear`), the offset is reset to 0 and the file is
/// re-read from the beginning.
pub fn tail_once(
    path: &Path,
    local_id: &str,
    mut offset: u64,
) -> std::io::Result<(Vec<AdapterEvent>, u64)> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((vec![], offset));
        }
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len < offset {
        // Rotation: start over.
        offset = 0;
    }
    if len == offset {
        return Ok((vec![], offset));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut new_offset = offset;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Only advance the persisted offset over complete lines (ending
        // in '\n'); a truncated trailing line will be re-read next tick.
        if !line.ends_with('\n') {
            break;
        }
        new_offset += n as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            tracing::debug!(?trimmed, "ignoring non-JSON transcript line");
            continue;
        };
        parse_line(local_id, &value, &mut events);
    }
    Ok((events, new_offset))
}

/// How far behind the persisted offset a reconciliation re-tail backs up
/// before re-reading. Large enough to recover several missed
/// turns' worth of transcript, small enough that the re-emitted volume
/// stays cheap (the server's content-hash dedup drops every dup).
pub const RECONCILE_BACKUP_BYTES: u64 = 64 * 1024;

/// Re-tail `path` from a checkpoint a fixed window BEHIND `persisted_offset`
/// to self-heal any gap left when an event was emitted but never persisted
/// server-side (a send dropped while the offset advanced) or when roster
/// churn re-homed the tail. Returns the parsed events; the caller
/// MUST NOT advance/persist any offset from this — it deliberately re-reads
/// already-seen lines, relying on the server's `ON CONFLICT … DO NOTHING`
/// dedup to drop the dups and surface only real gaps.
///
/// The checkpoint is realigned to a JSONL line boundary so parsing never
/// starts mid-line: `persisted_offset` always sits just after a `\n` (only
/// complete lines advance it in `tail_once`), but `persisted_offset -
/// backup` lands in the middle of an earlier line. We scan forward from the
/// backed-up position to the next `\n` and resume after it, so the first
/// line read is always whole. When the backup reaches the start of the file
/// (offset 0) we read from 0 directly — byte 0 is already a line boundary.
pub fn reconcile_tail(
    path: &Path,
    local_id: &str,
    persisted_offset: u64,
) -> std::io::Result<Vec<AdapterEvent>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(vec![]);
    }
    // Clamp the offset to the current length (the file may have rotated /
    // been truncated since the offset was taken) so the backup math stays
    // in bounds, then back up the window.
    let anchor = persisted_offset.min(len);
    let start = anchor.saturating_sub(RECONCILE_BACKUP_BYTES);

    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);

    // Realign to a line boundary: when we backed up into the middle of a
    // line (start > 0), discard the partial line by reading up to and
    // including the next '\n'. At start == 0 the position is already a
    // boundary, so skip the realignment.
    if start > 0 {
        let mut partial = String::new();
        // Advance the reader past the partial line; we don't need its byte
        // count, only the realignment side effect.
        reader.read_line(&mut partial)?;
        // If that "line" had no terminating '\n' it ran to EOF with no
        // complete line after the checkpoint — nothing to reconcile.
        if !partial.ends_with('\n') {
            return Ok(vec![]);
        }
    }

    let mut events = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Only parse complete lines; a truncated trailing line is left for
        // the regular tail to pick up once it's whole.
        if !line.ends_with('\n') {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        parse_line(local_id, &value, &mut events);
    }
    Ok(events)
}

/// Process-local tally of transcript shapes that produce no event, keyed by a
/// namespaced label (`unknown:<type>`, `unknown-assistant-block:<t>`,
/// `unknown-user-block:<t>`, `ignored:<kind>`). The daemon has no metrics
/// exporter, so counts live here rather than in a registry.
static DROP_TALLY: LazyLock<Mutex<BTreeMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Increment `label` and return its new count (0 if the lock is poisoned —
/// observability must never take the parser down).
fn tally(label: &str) -> u64 {
    let Ok(mut counts) = DROP_TALLY.lock() else { return 0 };
    let count = counts.entry(label.to_owned()).or_insert(0);
    *count += 1;
    *count
}

/// A shape we do not handle and did not expect: count it, and warn once.
fn record_unknown(namespace: &str, kind: &str) {
    let label = format!("{namespace}:{kind}");
    if tally(&label) == 1 {
        tracing::warn!(
            line_type = %label,
            "unhandled claude transcript shape — content is being dropped"
        );
    }
}

/// A shape we handle by deliberately producing nothing. Counted, never warned.
fn record_ignored(kind: &str) {
    tally(&format!("ignored:{kind}"));
}

/// Snapshot of [`DROP_TALLY`] for the session-diagnose report.
// The diagnose aggregation that reads this lives in `control.rs` and is owned by
// another change; without it nothing in the binary calls this yet.
#[allow(dead_code)]
#[must_use]
pub fn transcript_drop_tally() -> Vec<(String, u64)> {
    let Ok(counts) = DROP_TALLY.lock() else { return Vec::new() };
    counts.iter().map(|(k, v)| (k.clone(), *v)).collect()
}

/// First non-empty string among `keys`. Claude has renamed fields between
/// releases on several of these records, and this parser cannot see the schema.
fn first_str<'a>(line: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|k| line.get(*k))
        .filter_map(Value::as_str)
        .find(|s| !s.trim().is_empty())
}

const EXCERPT_CHARS: usize = 120;

/// One-line, length-capped excerpt for a marker label.
fn excerpt(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    if line.chars().count() <= EXCERPT_CHARS {
        return line.to_owned();
    }
    let head: String = line.chars().take(EXCERPT_CHARS).collect();
    format!("{head}…")
}

/// Map one transcript / stream-json JSON line (`type:"assistant"|"user"|…`)
/// to zero or more [`AdapterEvent`]s. Shared with the stream-json drivers:
/// the CLI's `--output-format stream-json` `assistant`/`user`
/// frames carry the same `message.content` shape as transcript lines, so the
/// same normalization applies.
pub(super) fn parse_line(local_id: &str, line: &Value, out: &mut Vec<AdapterEvent>) {
    let kind = line.get("type").and_then(Value::as_str).unwrap_or_default();
    // `/compact` appends an `isCompactSummary` line (a `type:"user"`
    // entry whose content is the auto-generated summary) into the SAME
    // transcript — no session-id rotation, so the rotation marker never
    // fires. Surface it as a dedicated compact event instead of letting it
    // render as a giant user-typed bubble.
    if line.get("isCompactSummary").and_then(Value::as_bool).unwrap_or(false) {
        out.push(AdapterEvent::Message {
            local_id: local_id.to_owned(),
            payload: json!({ "role": "compact_summary", "text": compact_summary_text(line) }),
        });
        return;
    }
    match kind {
        "assistant" => parse_assistant(local_id, line, out),
        "user" => parse_user(local_id, line, out),
        "post_turn_summary" => {
            out.push(AdapterEvent::Message {
                local_id: local_id.to_owned(),
                payload: json!({
                    "role": "summary",
                    "status_category": line.get("status_category"),
                    "status_detail": line.get("status_detail"),
                    "needs_action": line.get("needs_action"),
                }),
            });
        }
        "pr-link" => {
            if let Some(child) = pr_link_child(line) {
                out.push(AdapterEvent::PrLink {
                    local_id: local_id.to_owned(),
                    children: vec![child],
                });
            }
        }
        "attachment" | "permission-mode" | "worktree-state" | "ai-title" | "custom-title"
        | "agent-name" | "agent-setting" | "queue-operation" => {
            out.push(AdapterEvent::Message {
                local_id: local_id.to_owned(),
                payload: system_marker_payload(kind, line),
            });
        }
        "system" => parse_system(local_id, line, out),
        // `last-prompt` is a composer-restore pointer and `atis-latch` internal
        // latch state: no user meaning, and `last-prompt` embeds `leafUuid` so
        // every one survives payload dedup as its own bubble.
        "last-prompt" | "atis-latch" => record_ignored(kind),
        _ => record_unknown("unknown", kind),
    }
}

/// `type:"system"` covers both genuine timeline events and per-turn bookkeeping
/// (`turn_duration`, `stop_hook_summary`) that belongs on the owning turn rather
/// than in the timeline; the latter is counted, not emitted.
fn parse_system(local_id: &str, line: &Value, out: &mut Vec<AdapterEvent>) {
    let subtype = line.get("subtype").and_then(Value::as_str).unwrap_or_default();
    let body = || {
        first_str(line, &["summary", "content", "message", "text", "command"])
            .map(excerpt)
            .unwrap_or_default()
    };
    let text = match subtype {
        "away_summary" => format!("away summary: {}", body()),
        "scheduled_task_fire" => format!("scheduled task fired: {}", body()),
        "model_refusal_fallback" => format!("model refusal fallback: {}", body()),
        "informational" => body(),
        "local_command" => format!("local command: {}", body()),
        "turn_duration" | "stop_hook_summary" => {
            record_ignored(&format!("system/{subtype}"));
            return;
        }
        other => {
            record_unknown("unknown", &format!("system/{other}"));
            return;
        }
    };
    let text = if text.trim().is_empty() { subtype.to_owned() } else { text };
    out.push(AdapterEvent::Message {
        local_id: local_id.to_owned(),
        payload: json!({
            "role": "system_marker",
            "marker": format!("system/{subtype}"),
            "text": text,
        }),
    });
}

/// Never embeds the raw line — attachment bodies can be huge; only the marker,
/// a few useful fields, and a short `text` survive.
fn system_marker_payload(marker: &str, line: &Value) -> Value {
    let str_field = |k: &str| line.get(k).and_then(Value::as_str).unwrap_or_default();
    match marker {
        "attachment" => {
            let att = line
                .get("attachment")
                .and_then(|a| a.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            json!({
                "role": "system_marker",
                "marker": marker,
                "attachment_type": att,
                "text": format!("[attachment: {att}]"),
            })
        }
        "permission-mode" => {
            let mode = str_field("permissionMode");
            json!({
                "role": "system_marker",
                "marker": marker,
                "permission_mode": mode,
                "text": format!("permission mode: {mode}"),
            })
        }
        "worktree-state" => {
            let state = line.get("worktreeState").cloned().unwrap_or(Value::Null);
            let text = state.as_str().map_or_else(
                || "worktree state updated".to_owned(),
                |s| format!("worktree state: {s}"),
            );
            json!({
                "role": "system_marker",
                "marker": marker,
                "worktree_state": state,
                "text": text,
            })
        }
        "ai-title" => {
            let title = str_field("aiTitle");
            json!({
                "role": "system_marker",
                "marker": marker,
                "title": title,
                "text": format!("title: {title}"),
            })
        }
        "agent-name" => {
            let name = str_field("agentName");
            json!({
                "role": "system_marker",
                "marker": marker,
                "agent_name": name,
                "text": format!("agent name: {name}"),
            })
        }
        "agent-setting" => {
            let setting = str_field("agentSetting");
            json!({
                "role": "system_marker",
                "marker": marker,
                "agent_setting": setting,
                "text": format!("agent setting: {setting}"),
            })
        }
        "custom-title" => {
            let title = first_str(line, &["customTitle", "title"]).unwrap_or_default();
            json!({
                "role": "system_marker",
                "marker": marker,
                "title": title,
                "text": format!("title: {title}"),
            })
        }
        "queue-operation" => {
            let op = first_str(line, &["operation", "op", "action"]).unwrap_or("queue");
            let verb = match op {
                "enqueue" | "add" | "queued" => "queued",
                "dequeue" | "remove" | "dequeued" => "dequeued",
                other => other,
            };
            let body = first_str(line, &["prompt", "text", "content", "value"])
                .map(excerpt)
                .unwrap_or_default();
            let text = if body.is_empty() { verb.to_owned() } else { format!("{verb}: {body}") };
            json!({
                "role": "system_marker",
                "marker": marker,
                "operation": verb,
                "text": text,
            })
        }
        _ => json!({ "role": "system_marker", "marker": marker, "text": marker }),
    }
}

/// Extract a linked-PR [`SessionChild`] from a transcript `pr-link` line. The
/// carrier is tolerant of shape drift: the PR fields may sit at the top level
/// or nested under `pr`/`child`. A line with no resolvable `href` is dropped.
fn pr_link_child(line: &Value) -> Option<cctui_proto::adapter::SessionChild> {
    let obj = line.get("pr").or_else(|| line.get("child")).unwrap_or(line);
    let href = obj
        .get("href")
        .or_else(|| obj.get("url"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_owned();
    let id = obj
        .get("id")
        .and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string())))
        .or_else(|| obj.get("number").and_then(|v| v.as_i64().map(|n| n.to_string())))
        .unwrap_or_else(|| href.clone());
    let kind = obj.get("kind").and_then(Value::as_str).unwrap_or("pr").to_owned();
    Some(cctui_proto::adapter::SessionChild { id, href, kind })
}

fn parse_assistant(local_id: &str, line: &Value, out: &mut Vec<AdapterEvent>) {
    let message = line.get("message");
    let message_id = message.and_then(|m| m.get("id")).and_then(Value::as_str);
    // Token usage — one row per assistant message, idempotent on
    // `(session_id, message_id)`. Skip when either the usage block or
    // the message id is missing (older transcripts).
    if let (Some(usage), Some(msg_id)) = (
        message.and_then(|m| m.get("usage")),
        message.and_then(|m| m.get("id")).and_then(Value::as_str),
    ) {
        let pick = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        let input = pick("input_tokens");
        let output = pick("output_tokens");
        let cache_read = pick("cache_read_input_tokens");
        let cache_creation = pick("cache_creation_input_tokens");
        if input | output | cache_read | cache_creation > 0 {
            out.push(AdapterEvent::TokenUsage {
                local_id: local_id.to_owned(),
                message_id: msg_id.to_owned(),
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_creation_tokens: cache_creation,
            });
        }
    }
    // Model id of what actually ran (`message.model`, e.g. "claude-opus-4-8").
    // Surfaces the model for sessions started without an explicit `--model`
    // flag; the server writes it only when `sessions.model` is still unset, so
    // an explicit `--model` alias keeps priority. Subagent transcripts carry
    // the parent's model too, so each gets labelled.
    if let Some(model) = message
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        out.push(AdapterEvent::SessionModel {
            local_id: local_id.to_owned(),
            model: model.to_owned(),
        });
    }
    let Some(content) = message.and_then(|m| m.get("content")).and_then(Value::as_array) else {
        return;
    };
    for block in content {
        parse_assistant_block(local_id, message_id, block, out);
    }
}

fn parse_assistant_block(
    local_id: &str,
    message_id: Option<&str>,
    block: &Value,
    out: &mut Vec<AdapterEvent>,
) {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            out.push(AdapterEvent::Message {
                local_id: local_id.to_owned(),
                payload: json!({
                    "role": "assistant",
                    "text": block.get("text"),
                    "message_id": message_id,
                }),
            });
        }
        Some("thinking") => {
            out.push(AdapterEvent::Message {
                local_id: local_id.to_owned(),
                payload: json!({
                    "role": "assistant_thinking",
                    "text": block.get("thinking").or_else(|| block.get("text")),
                    "message_id": message_id,
                }),
            });
        }
        Some("tool_use") => {
            out.push(AdapterEvent::ToolUse {
                local_id: local_id.to_owned(),
                payload: json!({
                    "id": block.get("id"),
                    "tool": block.get("name"),
                    "input": block.get("input"),
                }),
            });
        }
        Some("redacted_thinking") => {
            out.push(AdapterEvent::Message {
                local_id: local_id.to_owned(),
                payload: json!({
                    "role": "assistant_redacted_thinking",
                    "text": "[redacted thinking]",
                    "message_id": message_id,
                }),
            });
        }
        Some("image") => {
            out.push(AdapterEvent::Message {
                local_id: local_id.to_owned(),
                payload: json!({
                    "role": "assistant_attachment",
                    "text": "[image attachment]",
                    "message_id": message_id,
                }),
            });
        }
        Some("server_tool_use") => {
            out.push(AdapterEvent::ToolUse {
                local_id: local_id.to_owned(),
                payload: json!({
                    "kind": "server_tool_use",
                    "id": block.get("id"),
                    "tool": block.get("name"),
                    "input": block.get("input"),
                }),
            });
        }
        Some(t) if t.ends_with("_tool_result") => {
            out.push(AdapterEvent::ToolUse {
                local_id: local_id.to_owned(),
                payload: json!({
                    "kind": "server_tool_result",
                    "tool_use_id": block.get("tool_use_id"),
                    "content": server_result_snippet(block.get("content")),
                }),
            });
        }
        other => {
            record_unknown("unknown-assistant-block", other.unwrap_or("<none>"));
        }
    }
}

const SERVER_RESULT_SNIPPET_CHARS: usize = 2000;

/// Web-search results can run to tens of KB; keep only a leading snippet.
fn server_result_snippet(content: Option<&Value>) -> Value {
    let Some(v) = content else { return Value::Null };
    let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
    if s.chars().count() <= SERVER_RESULT_SNIPPET_CHARS {
        json!(s)
    } else {
        json!(s.chars().take(SERVER_RESULT_SNIPPET_CHARS).collect::<String>())
    }
}

/// Markers that prefix content injected *to* the agent (not typed by the
/// human): background-task notifications, slash-command expansions, bash
/// passthrough, injected reminders, skill preambles, hook feedback. These are
/// fixed strings Claude Code / the cctui harness emit, so the match is exact,
/// not a fuzzy guess.
///
/// We deliberately do NOT trust Claude's top-level `isMeta` flag:
/// cctui delivers a human's composer reply through Claude's control-socket
/// `reply` op, and Claude records that non-interactively-injected turn with
/// `isMeta:true` even though it is genuine human input. Trusting `isMeta`
/// reclassified those turns from `user` to `system` on reload, so they appeared
/// to vanish. Classifying purely by these structural markers keeps real human
/// prose visible while still hiding machine-injected content.
const META_MARKERS: [&str; 12] = [
    "<task-notification",
    "<system-reminder",
    "<command-name",
    "<command-message",
    "<local-command",
    "<bash-input",
    "<bash-stdout",
    "<bash-stderr",
    "[SYSTEM NOTIFICATION",
    "Base directory for this skill:",
    "Stop hook feedback:",
    "# Autonomous loop",
];

/// Claude echoes an ingested image back as a *user* turn of pure bookkeeping.
/// The wording changes between releases (`[Image: source: …]`,
/// `[Image: original 1440x3120, displayed at 923x2000. …]`, `[Image #2]`), so
/// match the family by its bracket shape rather than any one literal.
fn is_image_notice_line(line: &str) -> bool {
    let t = line.trim();
    let Some(rest) = t.strip_prefix("[Image") else { return false };
    rest.ends_with(']') && rest.starts_with([']', ':', ' ', '#'])
}

/// A turn made of nothing but image-notice lines carries no human content.
fn is_image_notice_turn(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty()).peekable();
    lines.peek().is_some() && lines.all(is_image_notice_line)
}

/// Whether a user-role transcript message is really a system/agent-directed
/// message rather than human input, decided solely from the message body
/// (`text`). See [`META_MARKERS`] for why Claude's `isMeta` flag is ignored.
///
/// Markers are matched at the start of ANY line, not only the start of the
/// turn: the harness routinely prefixes its own sentence before the wrapper it
/// injects, which a prefix-only test never sees. Line-anchored rather than a
/// bare substring scan so a human quoting `<system-reminder>` inside a sentence
/// stays a human turn.
fn user_text_is_meta(text: &str) -> bool {
    if is_image_notice_turn(text) {
        return true;
    }
    text.lines().any(|line| {
        let t = line.trim_start();
        META_MARKERS.iter().any(|m| t.starts_with(m))
    })
}

/// Extract the summary text from a `/compact` line. The content lives under
/// `message.content`, which is either a plain string or an array of `text`
/// blocks (same shape as a normal user message) — join the latter.
fn compact_summary_text(line: &Value) -> String {
    let Some(content) = line.get("message").and_then(|m| m.get("content")) else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_owned();
    }
    let Some(blocks) = content.as_array() else { return String::new() };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Claude writes this user line when ESC (or ESC ESC) aborts a turn. It also
/// means the TUI composer likely holds the restored previous prompt.
const INTERRUPTED_MARKER: &str = "[Request interrupted by user";

fn interrupted_marker_payload(text: &str) -> Option<Value> {
    text.trim_start()
        .starts_with(INTERRUPTED_MARKER)
        .then(|| json!({ "role": "system_marker", "marker": "interrupted", "text": text.trim() }))
}

/// Stamp a user-line payload with the transcript line's identity.
///
/// The server dedupes on `digest(payload)`, so a user turn — the only payload
/// made purely of content — must carry a discriminator or the same prose sent
/// twice is dropped. It has to come from the line itself (`uuid`, or
/// `timestamp` for stream-json frames that have none), never be generated at
/// emit time, so a replay of the same line still hashes identically.
fn with_line_id(mut payload: Value, line: &Value) -> Value {
    let id = line
        .get("uuid")
        .or_else(|| line.get("timestamp"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if let (Some(id), Some(obj)) = (id, payload.as_object_mut()) {
        obj.insert("line_id".to_owned(), Value::String(id.to_owned()));
    }
    payload
}

fn parse_user(local_id: &str, line: &Value, out: &mut Vec<AdapterEvent>) {
    // User lines can be plain text or carry tool_result blocks.
    let Some(content) = line.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    if let Some(text) = content.as_str() {
        let payload = interrupted_marker_payload(text).unwrap_or_else(
            || json!({"role": "user", "text": text, "meta": user_text_is_meta(text)}),
        );
        out.push(AdapterEvent::Message {
            local_id: local_id.to_owned(),
            payload: with_line_id(payload, line),
        });
        return;
    }
    let Some(blocks) = content.as_array() else { return };
    // A user turn stored as a block array can interleave several `text` blocks
    // with `image` blocks — Claude expands attached files into their own
    // synthetic `[Image #N]` / `[Image: source: …]` text blocks plus raw
    // `image` blocks. Emitting one `Message` per block fans a single turn out
    // into 3+ duplicate bubbles in the webui and drops the `image` blocks
    // entirely. Instead JOIN the text blocks into ONE `Message` for the turn
    // (preserving order) and note any attachments with a single indicator so
    // an attachment-only turn isn't lost. `tool_result` blocks stay
    // as their own events.
    let mut texts: Vec<&str> = Vec::new();
    let mut has_attachment = false;
    let mut tool_results: Vec<AdapterEvent> = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                texts.push(block.get("text").and_then(Value::as_str).unwrap_or_default());
            }
            Some("image") => {
                has_attachment = true;
            }
            Some("tool_result") => {
                tool_results.push(AdapterEvent::ToolUse {
                    local_id: local_id.to_owned(),
                    payload: json!({
                        "kind": "tool_result",
                        "tool_use_id": block.get("tool_use_id"),
                        "content": block.get("content"),
                        "is_error": block.get("is_error"),
                    }),
                });
            }
            other => record_unknown("unknown-user-block", other.unwrap_or("<none>")),
        }
    }
    if !texts.is_empty() || has_attachment {
        let mut joined = texts.join("\n");
        if has_attachment && joined.trim().is_empty() {
            "[image attachment]".clone_into(&mut joined);
        }
        let payload = interrupted_marker_payload(&joined).unwrap_or_else(
            || json!({"role": "user", "text": joined, "meta": user_text_is_meta(&joined)}),
        );
        out.push(AdapterEvent::Message {
            local_id: local_id.to_owned(),
            payload: with_line_id(payload, line),
        });
    }
    out.extend(tool_results);
}

pub use crate::offsets::OffsetStore;

#[must_use]
pub fn default_projects_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(".claude").join("projects")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn encode_cwd_replaces_slash_and_dot() {
        assert_eq!(encode_cwd("/Users/me/.claude/feedbacks"), "-Users-me--claude-feedbacks");
        assert_eq!(encode_cwd("/tmp/test"), "-tmp-test");
        assert_eq!(encode_cwd("/a.b/c"), "-a-b-c");
    }

    #[test]
    fn encode_cwd_drops_trailing_slash() {
        // a dispatched session's working_dir often carries a trailing
        // slash (`/home/you/proj/`). Claude normalizes it away before deriving
        // the projects-dir segment, so we must too — otherwise the encoded dir
        // gets a spurious trailing dash and the transcript is never found.
        assert_eq!(encode_cwd("/home/you/proj/"), "-home-you-proj");
        assert_eq!(encode_cwd("/home/you/proj"), "-home-you-proj");
        // Multiple trailing slashes collapse the same way.
        assert_eq!(encode_cwd("/tmp/test//"), "-tmp-test");
    }

    #[test]
    fn transcript_path_is_built_correctly() {
        let p = transcript_path(Path::new("/projects"), "/Users/me", "abc-123");
        assert_eq!(p, PathBuf::from("/projects/-Users-me/abc-123.jsonl"));
    }

    #[test]
    fn newest_transcript_follows_a_worktree_move() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sess = "sess-1";
        let repo = root.join(encode_cwd("/workspace/repo"));
        let worktree = root.join(encode_cwd("/workspace/repo/.claude/worktrees/wt"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();

        // No transcript yet anywhere.
        assert_eq!(newest_transcript_for_session(root, sess), None);

        // Session starts in the repo cwd.
        let repo_file = repo.join(format!("{sess}.jsonl"));
        std::fs::write(&repo_file, b"{}\n").unwrap();
        assert_eq!(newest_transcript_for_session(root, sess), Some(repo_file));

        // EnterWorktree relocates the file; the newest wins even if the stale
        // one lingers on disk.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let worktree_file = worktree.join(format!("{sess}.jsonl"));
        std::fs::write(&worktree_file, b"{}\n{}\n").unwrap();
        assert_eq!(newest_transcript_for_session(root, sess), Some(worktree_file));

        // A different session id is never matched.
        assert_eq!(newest_transcript_for_session(root, "other"), None);
    }

    #[test]
    fn subagents_dir_descends_from_parent_transcript() {
        let parent = transcript_path(Path::new("/projects"), "/Users/me", "sess-1");
        assert_eq!(subagents_dir(&parent), PathBuf::from("/projects/-Users-me/sess-1/subagents"));
    }

    #[test]
    fn discover_subagents_lists_agent_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("agent-a8412884de5cc5396.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.join("agent-b0c27d990208c793.jsonl"), b"{}\n").unwrap();
        // Non-matching entries are ignored.
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("agent-partial.json"), b"x").unwrap();
        let mut found = discover_subagents(dir);
        found.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        assert_eq!(
            found,
            vec![
                SubagentEntry {
                    agent_id: "a8412884de5cc5396".to_owned(),
                    path: dir.join("agent-a8412884de5cc5396.jsonl"),
                    workflow: None,
                    meta: None,
                },
                SubagentEntry {
                    agent_id: "b0c27d990208c793".to_owned(),
                    path: dir.join("agent-b0c27d990208c793.jsonl"),
                    workflow: None,
                    meta: None,
                },
            ]
        );
    }

    #[test]
    fn discover_subagents_finds_nested_workflow_agents() {
        // Workflow-tool agents live under subagents/workflows/<runId>/.
        let tmp = tempfile::tempdir().unwrap();
        // Lay out a realistic <session>/ tree.
        let session = tmp.path().join("-home-user").join("bea6c407");
        let subagents = session.join("subagents");
        let run_dir = subagents.join("workflows").join("wf_fab6efd5-4bf");
        std::fs::create_dir_all(&run_dir).unwrap();
        // Two workflow agents, one with a meta sidecar.
        std::fs::write(run_dir.join("agent-aaa.jsonl"), b"{}\n").unwrap();
        std::fs::write(run_dir.join("agent-bbb.jsonl"), b"{}\n").unwrap();
        std::fs::write(
            run_dir.join("agent-aaa.meta.json"),
            br#"{"agentType":"workflow-subagent"}"#,
        )
        .unwrap();
        // Run-state file carries the workflow name.
        let wf_state = session.join("workflows");
        std::fs::create_dir_all(&wf_state).unwrap();
        std::fs::write(wf_state.join("wf_fab6efd5-4bf.json"), br#"{"name":"deep-research"}"#)
            .unwrap();
        // Also a flat Task-tool agent alongside.
        std::fs::write(subagents.join("agent-flat.jsonl"), b"{}\n").unwrap();

        let mut found = discover_subagents(&subagents);
        found.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
        assert_eq!(found.len(), 3);

        let aaa = found.iter().find(|e| e.agent_id == "aaa").unwrap();
        assert_eq!(
            aaa.workflow,
            Some(WorkflowContext {
                run_id: "wf_fab6efd5-4bf".to_owned(),
                name: Some("deep-research".to_owned()),
            })
        );
        assert_eq!(
            aaa.meta.as_ref().and_then(|m| m.agent_type.as_deref()),
            Some("workflow-subagent")
        );
        let bbb = found.iter().find(|e| e.agent_id == "bbb").unwrap();
        assert_eq!(
            bbb.workflow,
            Some(WorkflowContext {
                run_id: "wf_fab6efd5-4bf".to_owned(),
                name: Some("deep-research".to_owned()),
            })
        );
        assert_eq!(bbb.meta, None); // no meta sidecar
        let flat = found.iter().find(|e| e.agent_id == "flat").unwrap();
        assert_eq!(flat.workflow, None);
    }

    #[test]
    fn workflow_name_falls_back_to_script_name() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("sess");
        let run_dir = session.join("subagents").join("workflows").join("wf_x");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("agent-z.jsonl"), b"{}\n").unwrap();
        let wf_state = session.join("workflows");
        std::fs::create_dir_all(&wf_state).unwrap();
        // No top-level `name`, but script.name present.
        std::fs::write(wf_state.join("wf_x.json"), br#"{"script":{"name":"scripted"}}"#).unwrap();

        let found = discover_subagents(&session.join("subagents"));
        let z = found.iter().find(|e| e.agent_id == "z").unwrap();
        assert_eq!(z.workflow.as_ref().unwrap().name.as_deref(), Some("scripted"));
    }

    #[test]
    fn flat_task_subagents_carry_their_sidecar_meta() {
        // The shape Claude writes next to a flat Task-tool transcript.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("agent-a1e5bd0f2c3d4e5f6.jsonl"), b"{}\n").unwrap();
        std::fs::write(
            dir.join("agent-a1e5bd0f2c3d4e5f6.meta.json"),
            br#"{"agentType":"general-purpose","description":"Global competitors research",
                 "toolUseId":"toolu_018vjjY9mx2Dfmjwv1mQs6nX","spawnDepth":1}"#,
        )
        .unwrap();
        // A depth-2 spawn also names its parent agent.
        std::fs::write(dir.join("agent-bb.jsonl"), b"{}\n").unwrap();
        std::fs::write(
            dir.join("agent-bb.meta.json"),
            br#"{"agentType":"Explore","description":"Research site-selection vendors",
                 "toolUseId":"toolu_01BXjN1arwk6Zzu6pbPN7kth",
                 "parentAgentId":"af547156cb073af1e","spawnDepth":2}"#,
        )
        .unwrap();

        let found = discover_subagents(dir);
        let first = found.iter().find(|e| e.agent_id == "a1e5bd0f2c3d4e5f6").unwrap();
        assert_eq!(first.workflow, None);
        assert_eq!(
            first.meta,
            Some(SubagentMeta {
                agent_type: Some("general-purpose".to_owned()),
                description: Some("Global competitors research".to_owned()),
                tool_use_id: Some("toolu_018vjjY9mx2Dfmjwv1mQs6nX".to_owned()),
                parent_agent_id: None,
                spawn_depth: Some(1),
            })
        );
        let nested = found.iter().find(|e| e.agent_id == "bb").unwrap();
        assert_eq!(
            nested.meta,
            Some(SubagentMeta {
                agent_type: Some("Explore".to_owned()),
                description: Some("Research site-selection vendors".to_owned()),
                tool_use_id: Some("toolu_01BXjN1arwk6Zzu6pbPN7kth".to_owned()),
                parent_agent_id: Some("af547156cb073af1e".to_owned()),
                spawn_depth: Some(2),
            })
        );
    }

    #[test]
    fn a_missing_or_malformed_sidecar_degrades_to_no_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // No sidecar at all.
        std::fs::write(dir.join("agent-none.jsonl"), b"{}\n").unwrap();
        // Truncated JSON.
        std::fs::write(dir.join("agent-broken.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.join("agent-broken.meta.json"), b"{\"agentType\": ").unwrap();
        // Valid JSON, but not an object.
        std::fs::write(dir.join("agent-array.jsonl"), b"{}\n").unwrap();
        std::fs::write(dir.join("agent-array.meta.json"), b"[1,2,3]").unwrap();
        // An object whose fields are the wrong types, and one with none of
        // the fields we read.
        std::fs::write(dir.join("agent-typed.jsonl"), b"{}\n").unwrap();
        std::fs::write(
            dir.join("agent-typed.meta.json"),
            br#"{"agentType":7,"description":null,"toolUseId":{},"spawnDepth":"deep"}"#,
        )
        .unwrap();

        let found = discover_subagents(dir);
        assert_eq!(found.len(), 4);
        let meta_of = |id: &str| {
            found.iter().find(|e| e.agent_id == id).unwrap_or_else(|| panic!("{id}")).meta.clone()
        };
        // Every transcript is still discovered, and nothing panics. A sidecar
        // that parses but yields no usable field is `None` like the rest: an
        // all-empty `Some` would claim the sidecar said something.
        for id in ["none", "broken", "array", "typed"] {
            assert_eq!(meta_of(id), None, "{id} must degrade to no meta");
        }
    }

    #[test]
    fn one_usable_sidecar_field_is_enough_to_be_some() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("agent-partialmeta.jsonl"), b"{}\n").unwrap();
        // Unknown keys alongside, and every field we read but one unusable.
        std::fs::write(
            dir.join("agent-partialmeta.meta.json"),
            br#"{"description":"Research vendors","agentType":null,"spawnDepth":"deep","extra":1}"#,
        )
        .unwrap();
        let found = discover_subagents(dir);
        assert_eq!(
            found[0].meta,
            Some(SubagentMeta {
                description: Some("Research vendors".to_owned()),
                ..SubagentMeta::default()
            })
        );
    }

    #[test]
    fn discover_subagents_missing_dir_is_empty() {
        assert!(discover_subagents(Path::new("/no/such/dir")).is_empty());
    }

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        for l in lines {
            f.write_all(l.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }

    #[test]
    fn pr_link_line_emits_pr_child() {
        let mut out = Vec::new();
        parse_line(
            "sess1",
            &json!({
                "type": "pr-link",
                "id": "1972",
                "href": "https://github.com/o/r/pull/1972",
                "kind": "pr"
            }),
            &mut out,
        );
        match out.as_slice() {
            [AdapterEvent::PrLink { local_id, children }] => {
                assert_eq!(local_id, "sess1");
                assert_eq!(children.len(), 1);
                assert_eq!(children[0].href, "https://github.com/o/r/pull/1972");
                assert_eq!(children[0].kind, "pr");
            }
            other => panic!("expected one PrLink event, got {other:?}"),
        }
    }

    #[test]
    fn pr_link_nested_url_and_number_shape() {
        let mut out = Vec::new();
        parse_line(
            "sess1",
            &json!({
                "type": "pr-link",
                "pr": { "number": 42, "url": "https://github.com/o/r/pull/42" }
            }),
            &mut out,
        );
        match out.as_slice() {
            [AdapterEvent::PrLink { children, .. }] => {
                assert_eq!(children[0].id, "42");
                assert_eq!(children[0].href, "https://github.com/o/r/pull/42");
                assert_eq!(children[0].kind, "pr");
            }
            other => panic!("expected one PrLink event, got {other:?}"),
        }
    }

    #[test]
    fn pr_link_without_href_is_dropped() {
        let mut out = Vec::new();
        parse_line("sess1", &json!({ "type": "pr-link", "id": "x" }), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn tail_emits_assistant_text_and_tool_use() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
            ],
        );
        let (events, offset) = tail_once(&path, "sess1", 0).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AdapterEvent::Message { .. }));
        assert!(matches!(&events[1], AdapterEvent::ToolUse { .. }));
        assert!(offset > 0);
    }

    #[test]
    fn tail_is_incremental() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"}]}}"#],
        );
        let (e1, off1) = tail_once(&path, "s", 0).unwrap();
        assert_eq!(e1.len(), 1);
        write_lines(
            &path,
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"b"}]}}"#],
        );
        let (e2, off2) = tail_once(&path, "s", off1).unwrap();
        assert_eq!(e2.len(), 1);
        assert!(off2 > off1);
    }

    #[test]
    fn tail_handles_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        // Write a bunch of lines so the offset is meaningfully large.
        for _ in 0..10 {
            write_lines(
                &path,
                &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"}]}}"#],
            );
        }
        let (_, off) = tail_once(&path, "s", 0).unwrap();
        // Simulate /clear: truncate and write a single short line.
        std::fs::write(&path, b"").unwrap();
        write_lines(
            &path,
            &[r#"{"type":"assistant","message":{"content":[{"type":"text","text":"y"}]}}"#],
        );
        let (events, new_off) = tail_once(&path, "s", off).unwrap();
        assert_eq!(events.len(), 1);
        assert!(new_off > 0 && new_off < off + 1024); // restarted from 0
    }

    #[test]
    fn reconcile_tail_closes_a_gap_behind_the_offset() {
        // Simulate the failure: the forward tail advanced (and
        // persisted) its offset past lines whose events never reached the
        // server. The reconcile re-tail backs up behind that offset and
        // re-emits them so the gap self-heals.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = |t: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        write_lines(&path, &[&line("a"), &line("b"), &line("c")]);
        // The persisted offset sits at EOF (all three lines "tailed"), but
        // say b and c never made it to the server.
        let persisted = std::fs::metadata(&path).unwrap().len();

        // A reconcile from a checkpoint behind the offset re-reads the lines.
        // The backup window is larger than the file, so it re-reads from the
        // start (byte 0) and re-emits every line.
        let events = reconcile_tail(&path, "s", persisted).unwrap();
        assert_eq!(events.len(), 3, "all lines behind the offset are re-emitted for dedup");
    }

    #[test]
    fn reconcile_tail_realigns_to_a_line_boundary() {
        // Build a transcript larger than the backup window so the checkpoint
        // (offset - RECONCILE_BACKUP_BYTES) genuinely lands MID-LINE, not at
        // byte 0. The realignment must discard that partial line and emit only
        // whole, fully-parsed messages — never a mangled half-line.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let line = |t: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        // ~80 bytes/line; ~1200 lines comfortably exceeds the 64 KiB window.
        let lines: Vec<String> = (0..1200).map(|i| line(&format!("msg-{i}"))).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        write_lines(&path, &refs);
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > RECONCILE_BACKUP_BYTES, "fixture must exceed the backup window");

        // Persisted offset at EOF -> checkpoint = len - 64KiB, mid-line.
        let events = reconcile_tail(&path, "s", len).unwrap();

        // Boundary safety: every event is a fully-parsed assistant message
        // (a mid-line start would have produced a fragment that fails JSON
        // parse and is dropped — so a corrupted bubble would never appear, but
        // the FIRST whole line after the checkpoint must parse cleanly).
        assert!(!events.is_empty(), "the window's worth of lines is re-emitted");
        for e in &events {
            assert!(matches!(e, AdapterEvent::Message { .. }));
        }
        // It re-emits only the window behind the offset, not the whole file.
        assert!(events.len() < lines.len(), "only the backup window is re-read, not all of it");
    }

    #[test]
    fn tail_skips_trailing_partial_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"complete"}]}}"#,
        )
        .unwrap();
        f.write_all(b"\n").unwrap();
        f.write_all(br#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"#)
            .unwrap();
        // no trailing newline
        let (events, off) = tail_once(&path, "s", 0).unwrap();
        assert_eq!(events.len(), 1, "only the complete line should be emitted");
        // Append the rest.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"\"}]}}\n").unwrap();
        let (events2, _) = tail_once(&path, "s", off).unwrap();
        assert_eq!(events2.len(), 1, "partial line replayed once complete");
    }

    #[test]
    fn user_tool_result_emits_tool_use_event() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok","is_error":false}]}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AdapterEvent::ToolUse { .. }));
    }

    fn user_payloads(lines: &[&str]) -> Vec<Value> {
        let mut events = Vec::new();
        for line in lines {
            parse_line("s", &serde_json::from_str::<Value>(line).unwrap(), &mut events);
        }
        events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Message { payload, .. } if payload["role"] == "user" => {
                    Some(payload.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The server's idempotency index is `(session_id, event_type,
    /// digest(payload::text))`, so payload equality is exactly what decides
    /// whether a turn survives.
    #[test]
    fn repeated_user_text_on_distinct_lines_yields_distinct_payloads() {
        for content in [r#""continue""#, r#"[{"type":"text","text":"continue"}]"#] {
            let payloads = user_payloads(&[
                &format!(
                    r#"{{"type":"user","uuid":"u-1","message":{{"role":"user","content":{content}}}}}"#
                ),
                &format!(
                    r#"{{"type":"user","uuid":"u-2","message":{{"role":"user","content":{content}}}}}"#
                ),
            ]);
            assert_eq!(payloads.len(), 2, "{content}");
            assert_eq!(payloads[0]["text"], payloads[1]["text"]);
            assert_ne!(payloads[0], payloads[1], "{content}");
        }
    }

    #[test]
    fn replayed_user_line_yields_an_identical_payload() {
        let line = r#"{"type":"user","uuid":"u-1","message":{"role":"user","content":"continue"}}"#;
        let payloads = user_payloads(&[line, line]);
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0], payloads[1]);
    }

    #[test]
    fn user_line_without_uuid_falls_back_to_timestamp() {
        let payloads = user_payloads(&[
            r#"{"type":"user","timestamp":"2026-09-08T10:00:00Z","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"user","timestamp":"2026-09-08T10:00:01Z","message":{"role":"user","content":"go"}}"#,
        ]);
        assert_eq!(payloads[0]["line_id"], "2026-09-08T10:00:00Z");
        assert_ne!(payloads[0], payloads[1]);
    }

    #[test]
    fn interrupted_user_line_becomes_marker_not_user_message() {
        for content in [
            r#""[Request interrupted by user]""#,
            r#"[{"type":"text","text":"[Request interrupted by user for tool use]"}]"#,
        ] {
            let mut events = Vec::new();
            let line: Value = serde_json::from_str(&format!(
                r#"{{"type":"user","message":{{"role":"user","content":{content}}}}}"#
            ))
            .unwrap();
            parse_line("s", &line, &mut events);
            assert_eq!(events.len(), 1, "{content}");
            let AdapterEvent::Message { payload, .. } = &events[0] else {
                panic!("expected Message for {content}");
            };
            assert_eq!(payload["role"], "system_marker");
            assert_eq!(payload["marker"], "interrupted");
            assert!(payload["text"].as_str().unwrap().starts_with("[Request interrupted by user"));
        }
    }

    #[test]
    fn user_meta_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                // genuine human input → not meta
                r#"{"type":"user","message":{"content":"do the thing"}}"#,
                // harness tag, isMeta absent → meta via marker match
                r#"{"type":"user","message":{"content":"<task-notification><status>completed</status></task-notification>"}}"#,
                // injected reminder, no angle-bracket tag → meta via prose marker
                r##"{"type":"user","isMeta":true,"message":{"content":[{"type":"text","text":"# Autonomous loop check"}]}}"##,
                // a human composer reply that cctui delivered through
                // Claude's control-socket `reply` op gets recorded `isMeta:true`,
                // but it is genuine human input with no machine marker → not meta.
                r#"{"type":"user","isMeta":true,"message":{"content":[{"type":"text","text":"resume coverart e2e verification"}]}}"#,
                // skill preamble injection, no tag → meta via prose marker
                r#"{"type":"user","isMeta":true,"message":{"content":"Base directory for this skill: /home/you/.claude/skills/x"}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        let metas: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Message { payload, .. } => {
                    Some(payload.get("meta").and_then(Value::as_bool).unwrap_or(false))
                }
                _ => None,
            })
            .collect();
        assert_eq!(metas, vec![false, true, true, false, true]);
    }

    #[test]
    fn emits_session_model_from_assistant_message() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","content":[{"type":"text","text":"hi"}]}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        let model = events.iter().find_map(|e| match e {
            AdapterEvent::SessionModel { local_id, model } => {
                Some((local_id.clone(), model.clone()))
            }
            _ => None,
        });
        assert_eq!(model, Some(("s".to_owned(), "claude-opus-4-8".to_owned())));
    }

    #[test]
    fn no_session_model_when_field_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            // older transcripts / sessions with no model field on the message
            &[
                r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}]}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        assert!(!events.iter().any(|e| matches!(e, AdapterEvent::SessionModel { .. })));
    }

    #[test]
    fn compact_summary_emits_compact_role_not_user() {
        // /compact appends an `isCompactSummary` user line in place
        // (no rotation). It must surface as a `compact_summary` message, not a
        // plain user bubble.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":[{"type":"text","text":"summary of the conversation"}]}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::Message { payload, .. } => {
                assert_eq!(payload.get("role").and_then(Value::as_str), Some("compact_summary"));
                assert_eq!(
                    payload.get("text").and_then(Value::as_str),
                    Some("summary of the conversation")
                );
            }
            other => panic!("expected compact_summary Message, got {other:?}"),
        }
    }

    #[test]
    fn compact_summary_string_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"plain string summary"}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            AdapterEvent::Message { payload, .. } => {
                assert_eq!(
                    payload.get("text").and_then(Value::as_str),
                    Some("plain string summary")
                );
            }
            other => panic!("expected compact_summary Message, got {other:?}"),
        }
    }

    #[test]
    fn assistant_usage_emits_token_usage_event() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"assistant","message":{"id":"msg_abc","content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":900,"cache_creation_input_tokens":10}}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        let tu = events
            .iter()
            .find(|e| matches!(e, AdapterEvent::TokenUsage { .. }))
            .expect("expected TokenUsage event");
        match tu {
            AdapterEvent::TokenUsage {
                message_id,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                ..
            } => {
                assert_eq!(message_id, "msg_abc");
                assert_eq!(*input_tokens, 100);
                assert_eq!(*output_tokens, 50);
                assert_eq!(*cache_read_tokens, 900);
                assert_eq!(*cache_creation_tokens, 10);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn assistant_without_usage_emits_no_token_event() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.jsonl");
        write_lines(
            &path,
            &[
                r#"{"type":"assistant","message":{"id":"msg_x","content":[{"type":"text","text":"hi"}]}}"#,
            ],
        );
        let (events, _) = tail_once(&path, "s", 0).unwrap();
        assert!(!events.iter().any(|e| matches!(e, AdapterEvent::TokenUsage { .. })));
    }

    fn message_payloads(events: &[AdapterEvent]) -> Vec<&Value> {
        events
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Message { payload, .. } => Some(payload),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn redacted_thinking_emits_placeholder_message() {
        let mut out = Vec::new();
        parse_line(
            "s",
            &json!({"type":"assistant","message":{"id":"m1","content":[
                {"type":"redacted_thinking","data":"EncRypTed=="}
            ]}}),
            &mut out,
        );
        let msgs = message_payloads(&out);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].get("role").and_then(Value::as_str),
            Some("assistant_redacted_thinking")
        );
        assert_eq!(msgs[0].get("text").and_then(Value::as_str), Some("[redacted thinking]"));
        assert_eq!(msgs[0].get("message_id").and_then(Value::as_str), Some("m1"));
    }

    #[test]
    fn assistant_image_emits_attachment_message() {
        let mut out = Vec::new();
        parse_line(
            "s",
            &json!({"type":"assistant","message":{"id":"m2","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
            ]}}),
            &mut out,
        );
        let msgs = message_payloads(&out);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get("role").and_then(Value::as_str), Some("assistant_attachment"));
        assert_eq!(msgs[0].get("text").and_then(Value::as_str), Some("[image attachment]"));
        assert_eq!(msgs[0].get("message_id").and_then(Value::as_str), Some("m2"));
    }

    #[test]
    fn server_tool_use_emits_tool_use_event() {
        let mut out = Vec::new();
        parse_line(
            "s",
            &json!({"type":"assistant","message":{"content":[
                {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"rust"}}
            ]}}),
            &mut out,
        );
        match out.as_slice() {
            [AdapterEvent::ToolUse { payload, .. }] => {
                assert_eq!(payload.get("kind").and_then(Value::as_str), Some("server_tool_use"));
                assert_eq!(payload.get("id").and_then(Value::as_str), Some("srvtoolu_1"));
                assert_eq!(payload.get("tool").and_then(Value::as_str), Some("web_search"));
                assert_eq!(payload.pointer("/input/query").and_then(Value::as_str), Some("rust"));
            }
            other => panic!("expected one ToolUse event, got {other:?}"),
        }
    }

    #[test]
    fn server_tool_result_blocks_emit_snipped_tool_use() {
        let big: String = "x".repeat(10_000);
        let mut out = Vec::new();
        parse_line(
            "s",
            &json!({"type":"assistant","message":{"content":[
                {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":big},
                {"type":"code_execution_tool_result","tool_use_id":"srvtoolu_2","content":{"stdout":"ok","return_code":0}}
            ]}}),
            &mut out,
        );
        assert_eq!(out.len(), 2);
        match &out[0] {
            AdapterEvent::ToolUse { payload, .. } => {
                assert_eq!(payload.get("kind").and_then(Value::as_str), Some("server_tool_result"));
                assert_eq!(payload.get("tool_use_id").and_then(Value::as_str), Some("srvtoolu_1"));
                let content = payload.get("content").and_then(Value::as_str).unwrap();
                assert_eq!(content.chars().count(), SERVER_RESULT_SNIPPET_CHARS);
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        match &out[1] {
            AdapterEvent::ToolUse { payload, .. } => {
                assert_eq!(payload.get("kind").and_then(Value::as_str), Some("server_tool_result"));
                let content = payload.get("content").and_then(Value::as_str).unwrap();
                assert!(content.contains("stdout"));
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn skipped_line_types_emit_system_markers() {
        let lines = [
            json!({"type":"attachment","attachment":{"type":"skill_listing","content":"huge blob"}}),
            json!({"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"x"}),
            json!({"type":"worktree-state","worktreeState":"active","sessionId":"x"}),
            json!({"type":"ai-title","aiTitle":"deploy v1","sessionId":"x"}),
            json!({"type":"agent-name","agentName":"kusaritoi","sessionId":"x"}),
            json!({"type":"agent-setting","agentSetting":"claude","sessionId":"x"}),
            json!({"type":"custom-title","customTitle":"my session","sessionId":"x"}),
            json!({"type":"queue-operation","operation":"enqueue","prompt":"<task-notification>go"}),
        ];
        let mut out = Vec::new();
        for l in &lines {
            parse_line("s", l, &mut out);
        }
        let msgs = message_payloads(&out);
        assert_eq!(msgs.len(), lines.len());
        for m in &msgs {
            assert_eq!(m.get("role").and_then(Value::as_str), Some("system_marker"));
            assert!(m.get("marker").and_then(Value::as_str).is_some());
            assert!(!m.get("text").and_then(Value::as_str).unwrap_or_default().is_empty());
        }
        let by_marker = |marker: &str| {
            *msgs.iter().find(|m| m.get("marker").and_then(Value::as_str) == Some(marker)).unwrap()
        };
        let att = by_marker("attachment");
        assert_eq!(att.get("attachment_type").and_then(Value::as_str), Some("skill_listing"));
        assert_eq!(att.get("text").and_then(Value::as_str), Some("[attachment: skill_listing]"));
        assert!(att.get("content").is_none(), "attachment body must not flow through");
        assert_eq!(
            by_marker("permission-mode").get("permission_mode").and_then(Value::as_str),
            Some("bypassPermissions")
        );
        assert_eq!(
            by_marker("worktree-state").get("text").and_then(Value::as_str),
            Some("worktree state: active")
        );
        assert_eq!(by_marker("ai-title").get("title").and_then(Value::as_str), Some("deploy v1"));
        assert_eq!(
            by_marker("agent-name").get("agent_name").and_then(Value::as_str),
            Some("kusaritoi")
        );
        assert_eq!(
            by_marker("agent-setting").get("agent_setting").and_then(Value::as_str),
            Some("claude")
        );
        assert_eq!(
            by_marker("custom-title").get("title").and_then(Value::as_str),
            Some("my session")
        );
        assert_eq!(
            by_marker("queue-operation").get("text").and_then(Value::as_str),
            Some("queued: <task-notification>go")
        );
    }

    #[test]
    fn meta_markers_match_mid_text_not_only_at_the_start() {
        // The case that fails a prefix-only test: the harness prefixes its own
        // sentence before the wrapper it injects.
        assert!(user_text_is_meta(
            "Another session sent a message:\n<task-notification>done</task-notification>"
        ));
        assert!(user_text_is_meta("preamble\n  <system-reminder>hi</system-reminder>"));
        assert!(user_text_is_meta("<system-reminder>hi</system-reminder>"));
    }

    #[test]
    fn human_prose_quoting_a_marker_inline_stays_human() {
        assert!(!user_text_is_meta("the <system-reminder> tag keeps firing, can we mute it?"));
        assert!(!user_text_is_meta("ship it"));
    }

    #[test]
    fn image_bookkeeping_turns_are_meta_whatever_the_wording() {
        for text in [
            "[Image: source: /tmp/a.png]",
            "[Image: original 1440x3120, displayed at 923x2000. Multiply coordinates by 1.56 to map to original image.]",
            "[Image #2]",
            "[Image]",
            "[Image #1]\n[Image #2]",
        ] {
            assert!(user_text_is_meta(text), "must be meta: {text}");
        }
        // A human turn that merely mentions an image is not bookkeeping.
        assert!(!user_text_is_meta("[Image #1]\nwhat is in this screenshot?"));
        assert!(!user_text_is_meta("look at [Image #1] please"));
    }

    fn tally_for(label: &str) -> u64 {
        transcript_drop_tally().into_iter().find(|(k, _)| k == label).map_or(0, |(_, v)| v)
    }

    #[test]
    fn system_subtypes_map_to_timeline_markers() {
        let lines = [
            (
                json!({"type":"system","subtype":"away_summary","summary":"stepped out"}),
                "away summary: stepped out",
            ),
            (
                json!({"type":"system","subtype":"scheduled_task_fire","content":"nightly"}),
                "scheduled task fired: nightly",
            ),
            (
                json!({"type":"system","subtype":"model_refusal_fallback","message":"downgraded"}),
                "model refusal fallback: downgraded",
            ),
            (json!({"type":"system","subtype":"informational","content":"heads up"}), "heads up"),
            (
                json!({"type":"system","subtype":"local_command","command":"/clear"}),
                "local command: /clear",
            ),
        ];
        for (line, want) in &lines {
            let mut out = Vec::new();
            parse_line("s", line, &mut out);
            let msgs = message_payloads(&out);
            assert_eq!(msgs.len(), 1, "one marker per system line: {line}");
            assert_eq!(msgs[0].get("role").and_then(Value::as_str), Some("system_marker"));
            assert_eq!(msgs[0].get("text").and_then(Value::as_str), Some(*want));
        }
    }

    #[test]
    fn turn_bookkeeping_and_pointer_records_are_dropped_but_counted() {
        let lines = [
            json!({"type":"last-prompt","leafUuid":"07cc9471"}),
            json!({"type":"atis-latch","value":1}),
            json!({"type":"system","subtype":"turn_duration","durationMs":1200}),
            json!({"type":"system","subtype":"stop_hook_summary","summary":"ok"}),
        ];
        let mut out = Vec::new();
        for l in &lines {
            parse_line("s", l, &mut out);
        }
        assert!(out.is_empty(), "deliberate drops must emit nothing");
        for label in [
            "ignored:last-prompt",
            "ignored:atis-latch",
            "ignored:system/turn_duration",
            "ignored:system/stop_hook_summary",
        ] {
            assert!(tally_for(label) >= 1, "{label} must be counted");
        }
    }

    #[test]
    fn unknown_shapes_are_counted_by_namespaced_label() {
        let lines = [
            json!({"type":"quantum-entanglement-log","x":1}),
            json!({"type":"system","subtype":"tachyon_burst"}),
            json!({"type":"assistant","message":{"content":[{"type":"holodeck_block"}]}}),
            json!({"type":"user","message":{"content":[{"type":"warp_core_block"}]}}),
        ];
        let mut out = Vec::new();
        for l in &lines {
            parse_line("s", l, &mut out);
        }
        assert!(out.is_empty(), "unknown shapes must not fabricate events");
        for label in [
            "unknown:quantum-entanglement-log",
            "unknown:system/tachyon_burst",
            "unknown-assistant-block:holodeck_block",
            "unknown-user-block:warp_core_block",
        ] {
            assert!(tally_for(label) >= 1, "{label} must be counted");
        }
    }

    #[test]
    fn offset_store_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("offsets.json");
        let mut s = OffsetStore::open(Some(path.clone()));
        s.set("sess1".into(), 42);
        s.flush();
        let s2 = OffsetStore::open(Some(path));
        assert_eq!(s2.get("sess1"), 42);
    }
}
