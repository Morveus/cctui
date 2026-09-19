//! Daemon side of the `CctuiAgent` tool.
//!
//! Listens on a local Unix socket for the `cctui-daemon mcp-agent` relay.
//! A call spawns a child through the server (the server owns the capability
//! decision — the daemon never grants anything itself), or, when it names a
//! `session_id`, sends a follow-up prompt into a child spawned earlier.
//! Both then follow the child via [`crate::childwatch`], streaming progress
//! frames to a proto≥2 relay while waiting and finishing with the child's
//! final message. Proto 1 relays (older, still attached to live sessions)
//! get the single final line only.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cctui_proto::api::{MessageChildRequest, SpawnChildRequest};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::childwatch::{Assessment, WatchHandle, snippet};
use crate::client::ServerClient;

/// Cadence of progress frames to the relay while a child runs.
const PROGRESS_EVERY: Duration = Duration::from_secs(15);

/// Relay protocol from which the daemon announces the child id in an
/// `attached` frame, and accepts a `follow_agent` reattach.
const ATTACH_PROTO: u64 = 3;

/// A child that has shown no sign of life at all by this point never reached
/// its first model call — waiting out `timeout_secs` only delays the failure.
const SILENT_CHILD_GRACE: Duration = Duration::from_secs(90);

const NUDGE_PROMPT: &str =
    "continue — return your findings / final answer now, in full, and nothing else.";

/// Socket the session's MCP relay connects to. Kept beside the daemon's other
/// runtime state so a worker container with an unwritable `~/.config` still
/// finds a usable path.
#[must_use]
pub fn socket_path() -> PathBuf {
    crate::runtime::state_candidates("cctui-agent.sock")
        .into_iter()
        .next()
        .unwrap_or_else(|| std::env::temp_dir().join("cctui-agent.sock"))
}

enum CallKind {
    Spawn(SpawnChildRequest),
    Message(MessageChildRequest),
    /// Reattach to a child that is already running and follow it to the end,
    /// sending it nothing. The relay issues this after the socket died under
    /// it (daemon re-exec on auto-update): the child never stopped, only the
    /// wait did, so resuming the wait must not re-prompt the child.
    Follow(String),
}

struct Call {
    session_id: String,
    kind: CallKind,
    timeout: Duration,
    /// Relay protocol: ≥2 understands interim `progress` frames.
    proto: u64,
}

/// A signpost, not an allowlist: an account catalog may alias other ids, and
/// the daemon never rejects an id it does not recognise.
const KNOWN_MODELS: &[(&str, &str)] = &[
    (
        "claude-code",
        "claude-opus-5[1m], claude-opus-5, claude-sonnet-5, claude-haiku-4-5, claude-fable-5",
    ),
    ("codex", "gpt-5.6-sol, gpt-5.6-terra"),
];

fn known_models_for(adapter: &str) -> String {
    let normalized = normalize_adapter(adapter);
    KNOWN_MODELS.iter().find(|(id, _)| *id == normalized).map_or_else(
        || {
            KNOWN_MODELS
                .iter()
                .map(|(id, models)| format!("{id}: {models}"))
                .collect::<Vec<_>>()
                .join("; ")
        },
        |(_, models)| (*models).to_owned(),
    )
}

fn missing_model_error(adapter: &str) -> String {
    format!(
        "model is required and was not given. CctuiAgent never falls back to the account \
         default: that silently spends a different budget than the caller intended, and a \
         whole fan-out can die on 429 minutes later without the cause being visible. Pass \
         model explicitly — known ids: {}. An alias from the account's own catalog is also \
         accepted.",
        known_models_for(adapter),
    )
}

fn parse_call(line: &str) -> Result<Call, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("malformed request: {e}"))?;
    let session_id = v
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("request carries no session id")?
        .to_owned();
    let session_id = resolve_session_alias(&session_id);
    let args = v.get("args").cloned().unwrap_or_else(|| json!({}));
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("").to_owned();
    let timeout = crate::mcp::resolve_timeout(v.get("timeout_secs").and_then(Value::as_u64));
    let proto = v.get("proto").and_then(Value::as_u64).unwrap_or(1);
    let kind = match v.get("kind").and_then(Value::as_str) {
        // A reattach carries no prompt by construction: it resumes a wait.
        Some("follow_agent") => CallKind::Follow(
            string_arg(&args, "session_id").ok_or("follow_agent needs the child session_id")?,
        ),
        Some("spawn_agent") => {
            if prompt.trim().is_empty() {
                return Err("prompt is required".to_owned());
            }
            if let Some(child) = string_arg(&args, "session_id") {
                CallKind::Message(MessageChildRequest { session_id: child, prompt })
            } else {
                let adapter =
                    normalize_adapter(args.get("adapter").and_then(Value::as_str).unwrap_or(""));
                let Some(model) = string_arg(&args, "model") else {
                    return Err(missing_model_error(&adapter));
                };
                CallKind::Spawn(SpawnChildRequest {
                    adapter,
                    prompt,
                    model: Some(model),
                    agent_profile: string_arg(&args, "agent_profile"),
                    budget_usd: args.get("budget_usd").and_then(Value::as_f64),
                    cwd: string_arg(&args, "cwd"),
                    permission_mode: string_arg(&args, "permission_mode")
                        .and_then(|m| serde_json::from_value(Value::String(m)).ok()),
                    name: string_arg(&args, "name"),
                })
            }
        }
        _ => return Err("unsupported request kind".to_owned()),
    };
    Ok(Call { session_id, kind, timeout, proto })
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Accept the model-facing spellings of an adapter id and return the canonical
/// one. `claude_code`/`claude` are the ids a model is most likely to guess.
#[must_use]
pub fn normalize_adapter(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "claude" | "claude-code" => "claude-code".to_owned(),
        "codex" | "codex-cli" => "codex".to_owned(),
        other => other.to_owned(),
    }
}

fn reply_frame(outcome: &crate::childwatch::ChildOutcome) -> Value {
    let id_line = outcome
        .local_id
        .as_deref()
        .map(|id| format!("\n\n[child session id: {id} — pass it as session_id to follow up]"))
        .unwrap_or_default();
    match (&outcome.error, &outcome.final_text) {
        (Some(err), Some(text)) => json!({
            "ok": false,
            "error": format!("child agent failed: {err}\n\nlast output:\n{text}{id_line}"),
        }),
        (Some(err), None) => {
            json!({ "ok": false, "error": format!("child agent failed: {err}{id_line}") })
        }
        (None, Some(text)) => json!({ "ok": true, "result": format!("{text}{id_line}") }),
        (None, None) => json!({
            "ok": true,
            "result": format!("child agent finished without producing any output{id_line}"),
        }),
    }
}

/// Appended to every frame a call returns: the caller must be able to see the
/// model it got rather than the one it assumed.
fn dispatch_note(kind: &CallKind, timeout: Duration) -> String {
    match kind {
        CallKind::Spawn(req) => format!(
            "\n\n[spawned on model {} · adapter {} · follow window {}s]",
            req.model.as_deref().unwrap_or("<unset>"),
            req.adapter,
            timeout.as_secs(),
        ),
        CallKind::Message(req) => format!(
            "\n\n[follow-up to child {} · runs on the child's original model, `model` is ignored here · follow window {}s]",
            req.session_id,
            timeout.as_secs(),
        ),
        CallKind::Follow(child) => format!(
            "\n\n[reattached to child {child} after the daemon restarted · nothing was re-sent to it · follow window {}s]",
            timeout.as_secs(),
        ),
    }
}

fn annotate(mut frame: Value, note: &str) -> Value {
    let key =
        if frame.get("ok").and_then(Value::as_bool) == Some(true) { "result" } else { "error" };
    if let Some(text) = frame.get(key).and_then(Value::as_str) {
        let joined = format!("{text}{note}");
        frame[key] = Value::String(joined);
    }
    frame
}

enum FollowResult {
    Finished(crate::childwatch::ChildOutcome),
    Error(Value),
}

fn follow_result_to_frame(result: FollowResult) -> Value {
    match result {
        FollowResult::Finished(outcome) => reply_frame(&outcome),
        FollowResult::Error(frame) => frame,
    }
}

/// Whether a completed turn's final message reads as a truncated non-answer
/// rather than a deliverable: empty, or ending on a planning/intent tail (a
/// trailing colon, or a bare enumeration marker the model never filled in).
fn looks_truncated(final_text: Option<&str>) -> bool {
    let Some(text) = final_text.map(str::trim).filter(|t| !t.is_empty()) else {
        return true;
    };
    let last = text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    last.ends_with(':') || is_bare_enumeration_marker(last)
}

fn is_bare_enumeration_marker(line: &str) -> bool {
    matches!(line, "-" | "*" | "•") || {
        let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
        rest.len() < line.len() && matches!(rest, "." | ")")
    }
}

/// Whether a finished child warrants the single automatic continuation nudge:
/// only a clean turn (no error) qualifies — a crashed or errored child is
/// never nudged. A turn whose tail was a thinking block is nudged even when
/// the held final text reads complete: that text is stale mid-turn narration.
fn should_nudge(outcome: &crate::childwatch::ChildOutcome) -> bool {
    outcome.error.is_none()
        && (outcome.tail_is_thinking || looks_truncated(outcome.final_text.as_deref()))
}

/// Whether the child has produced any evidence of a running turn: a bound
/// session id alone only proves the harness registered it.
const fn showed_activity(snap: &crate::childwatch::ChildSnapshot) -> bool {
    snap.final_text.is_some()
        || snap.last_tool.is_some()
        || snap.status_line.is_some()
        || snap.blocked.is_some()
}

async fn follow_child_with(
    handle: &WatchHandle,
    child_id: &str,
    timeout: Duration,
    silent_grace: Duration,
    proto: u64,
    out: &mut (impl AsyncWriteExt + Unpin),
) -> FollowResult {
    let started = Instant::now();
    let mut last_progress = Instant::now();
    loop {
        handle.changed(Duration::from_secs(2)).await;
        let now = Instant::now();
        let Some(snap) = handle.snapshot() else {
            return FollowResult::Error(
                json!({ "ok": false, "error": "child agent tracking was dropped" }),
            );
        };
        match snap.assess(now) {
            Assessment::Finished(outcome) => return FollowResult::Finished(outcome),
            Assessment::Running(line) => {
                if now.duration_since(started) >= silent_grace && !showed_activity(&snap) {
                    return FollowResult::Error(json!({
                        "ok": false,
                        "error": format!(
                            "child agent {} produced no activity within {}s of being prompted — \
                             no model call, no output and no error, so it almost certainly died \
                             at startup (auth, budget or rate-limit rejection). Not waiting out \
                             the {}s timeout; check the child session in cctui.",
                            snap.local_id.as_deref().unwrap_or(child_id),
                            silent_grace.as_secs(),
                            timeout.as_secs(),
                        ),
                    }));
                }
                if now.duration_since(started) >= timeout {
                    return FollowResult::Error(json!({
                        "ok": false,
                        "error": format!(
                            "the {}s follow window expired for child agent {child_id}. THIS IS \
                             NOT A CRASH: the child is still running, and whatever it has \
                             already written is on disk. Only the wait gave up. Watch it in \
                             cctui, or call CctuiAgent again with session_id {:?} to reattach \
                             and collect its answer. Pass a larger timeout_secs (max 7200) \
                             next time to wait longer.",
                            timeout.as_secs(),
                            snap.local_id.as_deref().unwrap_or(child_id),
                        ),
                    }));
                }
                if proto >= 2 && now.duration_since(last_progress) >= PROGRESS_EVERY {
                    last_progress = now;
                    let frame = json!({
                        "progress": format!(
                            "[{}s] {} · child session {}",
                            now.duration_since(started).as_secs(),
                            snippet(&line, 300),
                            snap.local_id.as_deref().unwrap_or(child_id),
                        ),
                    });
                    if write_line(out, &frame).await.is_err() {
                        return FollowResult::Error(
                            json!({ "ok": false, "error": "relay went away" }),
                        );
                    }
                }
            }
        }
    }
}

async fn write_line(out: &mut (impl AsyncWriteExt + Unpin), frame: &Value) -> std::io::Result<()> {
    out.write_all(format!("{frame}\n").as_bytes()).await?;
    out.flush().await
}

async fn run_call(
    server: &ServerClient,
    machine_key: &str,
    call: Call,
    out: &mut (impl AsyncWriteExt + Unpin),
) -> Value {
    let note = dispatch_note(&call.kind, call.timeout);
    let watch = crate::childwatch::global();
    let (handle, child_id) = match &call.kind {
        CallKind::Spawn(req) => {
            let child = match server.spawn_child(machine_key, &call.session_id, req).await {
                Ok(child) => child,
                Err(err) => {
                    return annotate(json!({ "ok": false, "error": err.to_string() }), &note);
                }
            };
            // Register BEFORE the spawn frame can produce events; the server
            // has already dispatched the spawn at this point, but the child
            // takes seconds to boot, so this stays ahead of its first event.
            let handle = watch.register(&child.session_id);
            tracing::info!(
                parent = %call.session_id,
                child = %child.session_id,
                adapter = %req.adapter,
                "CctuiAgent following spawned child",
            );
            (handle, child.session_id)
        }
        CallKind::Message(req) => {
            let handle = watch.register_bound(&req.session_id);
            if let Err(err) = server.message_child(machine_key, &call.session_id, req).await {
                return annotate(json!({ "ok": false, "error": err.to_string() }), &note);
            }
            tracing::info!(
                parent = %call.session_id,
                child = %req.session_id,
                "CctuiAgent following child after follow-up",
            );
            (handle, req.session_id.clone())
        }
        CallKind::Follow(child) => {
            let handle = watch.register_bound(child);
            tracing::info!(
                parent = %call.session_id,
                child = %child,
                "CctuiAgent reattaching to a running child",
            );
            (handle, child.clone())
        }
    };
    // Tell the relay which child this call is now following, BEFORE the first
    // wait. A daemon re-exec closes this socket without a result, and the
    // relay can only resume the wait if it already knows the child's id —
    // waiting for the first 15s progress frame loses every call that dies in
    // the first quarter-minute.
    if call.proto >= ATTACH_PROTO {
        let _ = write_line(out, &json!({ "attached": child_id })).await;
    }
    let result =
        follow_child_with(&handle, &child_id, call.timeout, SILENT_CHILD_GRACE, call.proto, out)
            .await;
    let FollowResult::Finished(outcome) = result else {
        return annotate(follow_result_to_frame(result), &note);
    };
    if !should_nudge(&outcome) {
        return annotate(reply_frame(&outcome), &note);
    }
    let Some(target) = outcome.local_id.clone() else {
        return annotate(reply_frame(&outcome), &note);
    };
    drop(handle);
    annotate(nudge_once(server, machine_key, &call, &watch, &target, outcome, out).await, &note)
}

/// Send exactly one continuation prompt to a child that finished on a truncated
/// non-answer and follow that turn. Falls back to the original outcome when the
/// nudge cannot be relayed or the follow-up itself produces nothing.
async fn nudge_once(
    server: &ServerClient,
    machine_key: &str,
    call: &Call,
    watch: &std::sync::Arc<crate::childwatch::ChildWatch>,
    target: &str,
    original: crate::childwatch::ChildOutcome,
    out: &mut (impl AsyncWriteExt + Unpin),
) -> Value {
    let req =
        MessageChildRequest { session_id: target.to_owned(), prompt: NUDGE_PROMPT.to_owned() };
    let handle = watch.register_bound(target);
    if let Err(err) = server.message_child(machine_key, &call.session_id, &req).await {
        tracing::warn!(parent = %call.session_id, child = %target, %err, "CctuiAgent nudge failed");
        return reply_frame(&original);
    }
    tracing::info!(parent = %call.session_id, child = %target, "CctuiAgent nudging truncated child");
    match follow_child_with(&handle, target, call.timeout, SILENT_CHILD_GRACE, call.proto, out)
        .await
    {
        FollowResult::Finished(nudged) if nudged.final_text.is_some() || nudged.error.is_some() => {
            reply_frame(&nudged)
        }
        _ => reply_frame(&original),
    }
}

async fn handle_connection(stream: UnixStream, server: ServerClient, machine_key: String) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let Ok(Some(line)) = lines.next_line().await else { return };
    let frame = match parse_call(&line) {
        Ok(call) => run_call(&server, &machine_key, call, &mut write_half).await,
        Err(err) => json!({ "ok": false, "error": err }),
    };
    let _ = write_line(&mut write_half, &frame).await;
}

/// Serve the agent-tool socket until `shutdown`.
pub async fn serve(
    path: PathBuf,
    server: ServerClient,
    machine_key: String,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    tracing::info!(socket = %path.display(), "CctuiAgent tool listener ready");
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                let _ = std::fs::remove_file(&path);
                return Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let server = server.clone();
                let machine_key = machine_key.clone();
                tokio::spawn(handle_connection(stream, server, machine_key));
            }
        }
    }
}

/// Whether the daemon can serve the tool at all: without a machine key there is
/// nobody to authorize a spawn against, so the listener stays off.
#[must_use]
pub fn is_available(machine_key: &str) -> bool {
    !machine_key.trim().is_empty()
}

/// Launch-key → real-session-id map for harnesses that mint their own id.
///
/// A codex thread id / opencode `ses_…` does not exist yet when the relay's
/// argv is baked, so those sessions carry their launch key as `--session`. The
/// server resolves a parent by `sessions.id`, so without this the call would
/// 404 against a key no session row uses.
static SESSION_ALIASES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Record that the session launched as `launch_key` really is `session_id`.
pub fn bind_session_alias(launch_key: &str, session_id: &str) {
    if launch_key.trim().is_empty() || session_id.trim().is_empty() || launch_key == session_id {
        return;
    }
    if let Ok(mut map) = SESSION_ALIASES.lock() {
        map.insert(launch_key.to_owned(), session_id.to_owned());
    }
}

/// Resolve a relay-supplied session id through [`bind_session_alias`]. An id
/// that was never aliased is already the real one and passes through.
#[must_use]
pub fn resolve_session_alias(id: &str) -> String {
    SESSION_ALIASES
        .lock()
        .ok()
        .and_then(|map| map.get(id).cloned())
        .unwrap_or_else(|| id.to_owned())
}

/// Path used when writing a session's MCP config, exposed for the launch path.
#[must_use]
pub fn socket_for_launch() -> &'static Path {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(socket_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::childwatch::ChildOutcome;

    #[test]
    fn a_launch_key_alias_resolves_a_call_onto_the_real_parent_session() {
        bind_session_alias("launch-key-abc", "thread_0199real");
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "launch-key-abc",
            "args": { "prompt": "review this", "model": "gpt-5.6-sol", "adapter": "codex" },
        })
        .to_string();
        let call = parse_call(&line).unwrap();
        assert_eq!(
            call.session_id, "thread_0199real",
            "a codex/opencode child must be attributed to the thread id the server knows, \
             not the key baked into the relay argv"
        );
    }

    #[test]
    fn an_unaliased_session_id_passes_through() {
        assert_eq!(resolve_session_alias("never-bound"), "never-bound");
    }

    #[test]
    fn parses_a_full_spawn_call() {
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "parent-1",
            "timeout_secs": 120,
            "proto": 2,
            "args": {
                "adapter": "opencode",
                "prompt": "review the diff",
                "model": "accounts/fireworks/models/kimi-k3",
                "agent_profile": "cctui-reviewer",
                "budget_usd": 0.5,
                "cwd": "/workspace",
                "permission_mode": "auto",
                "name": "reviewer",
            },
        })
        .to_string();
        let call = parse_call(&line).unwrap();
        assert_eq!(call.session_id, "parent-1");
        assert_eq!(call.timeout, Duration::from_mins(2));
        assert_eq!(call.proto, 2);
        let CallKind::Spawn(req) = call.kind else { panic!("expected spawn") };
        assert_eq!(req.adapter, "opencode");
        assert_eq!(req.agent_profile.as_deref(), Some("cctui-reviewer"));
        assert_eq!(req.budget_usd, Some(0.5));
        assert_eq!(req.cwd.as_deref(), Some("/workspace"));
        assert_eq!(req.permission_mode, Some(cctui_proto::adapter::PermissionMode::Auto));
        assert_eq!(req.name.as_deref(), Some("reviewer"));
    }

    #[test]
    fn a_session_id_arg_turns_the_call_into_a_follow_up() {
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "parent-1",
            "args": { "session_id": "child-9", "prompt": "and check the tests" },
        })
        .to_string();
        let call = parse_call(&line).unwrap();
        let CallKind::Message(req) = call.kind else { panic!("expected message") };
        assert_eq!(req.session_id, "child-9");
        assert_eq!(req.prompt, "and check the tests");
    }

    #[test]
    fn proto_defaults_to_1_for_old_relays() {
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "p",
            "args": { "adapter": "codex", "prompt": "go", "model": "gpt-5.6-sol" },
        })
        .to_string();
        assert_eq!(parse_call(&line).unwrap().proto, 1);
    }

    #[test]
    fn blank_optional_args_are_dropped_not_forwarded_empty() {
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "p",
            "args": { "adapter": "codex", "prompt": "go", "model": "gpt-5.6-sol", "cwd": "",
                      "permission_mode": "notamode" },
        })
        .to_string();
        let call = parse_call(&line).unwrap();
        let CallKind::Spawn(req) = call.kind else { panic!("expected spawn") };
        assert!(req.cwd.is_none());
        assert!(req.permission_mode.is_none());
    }

    #[test]
    fn a_spawn_without_a_model_is_rejected_and_the_error_names_the_ids() {
        for args in [
            json!({ "adapter": "claude-code", "prompt": "go" }),
            json!({ "adapter": "claude-code", "prompt": "go", "model": "   " }),
            json!({ "adapter": "claude-code", "prompt": "go", "model": "" }),
        ] {
            let line =
                json!({ "kind": "spawn_agent", "session_id": "p", "args": args }).to_string();
            let Err(err) = parse_call(&line) else { panic!("a spawn without a model must fail") };
            assert!(err.contains("model is required"), "{err}");
            assert!(err.contains("claude-opus-5[1m]"), "{err}");
            assert!(err.contains("claude-fable-5"), "{err}");
            assert!(err.contains("never falls back"), "{err}");
        }
    }

    #[test]
    fn the_missing_model_error_lists_the_ids_of_the_named_adapter() {
        let codex = missing_model_error("codex-cli");
        assert!(codex.contains("gpt-5.6-sol"), "{codex}");
        assert!(!codex.contains("claude-opus-5"), "{codex}");
        let unknown = missing_model_error("opencode");
        assert!(unknown.contains("claude-code:"), "{unknown}");
        assert!(unknown.contains("codex:"), "{unknown}");
    }

    #[test]
    fn a_spawn_with_a_model_still_parses_unchanged() {
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "p",
            "args": { "adapter": "claude", "prompt": "go", "model": " claude-opus-5[1m] " },
        })
        .to_string();
        let CallKind::Spawn(req) = parse_call(&line).unwrap().kind else {
            panic!("expected spawn")
        };
        assert_eq!(req.model.as_deref(), Some("claude-opus-5[1m]"));
        assert_eq!(req.adapter, "claude-code");
    }

    #[test]
    fn a_follow_up_needs_no_model() {
        let line = json!({
            "kind": "spawn_agent",
            "session_id": "p",
            "args": { "session_id": "child-9", "prompt": "carry on" },
        })
        .to_string();
        assert!(matches!(parse_call(&line).unwrap().kind, CallKind::Message(_)));
    }

    #[test]
    fn every_frame_echoes_the_model_and_the_follow_window() {
        let spawn = CallKind::Spawn(SpawnChildRequest {
            adapter: "claude-code".to_owned(),
            prompt: "go".to_owned(),
            model: Some("claude-opus-5[1m]".to_owned()),
            agent_profile: None,
            budget_usd: None,
            cwd: None,
            permission_mode: None,
            name: None,
        });
        let note = dispatch_note(&spawn, Duration::from_hours(2));
        assert!(note.contains("spawned on model claude-opus-5[1m]"), "{note}");
        assert!(note.contains("adapter claude-code"), "{note}");
        assert!(note.contains("follow window 7200s"), "{note}");

        let ok = annotate(json!({ "ok": true, "result": "all done" }), &note);
        let text = ok["result"].as_str().unwrap();
        assert!(text.starts_with("all done"));
        assert!(text.contains("claude-opus-5[1m]"), "{text}");

        let failed = annotate(json!({ "ok": false, "error": "child agent failed" }), &note);
        assert!(failed["error"].as_str().unwrap().contains("claude-opus-5[1m]"));

        let follow = dispatch_note(
            &CallKind::Message(MessageChildRequest {
                session_id: "child-9".to_owned(),
                prompt: "carry on".to_owned(),
            }),
            Duration::from_mins(30),
        );
        assert!(follow.contains("follow-up to child child-9"), "{follow}");
        assert!(follow.contains("follow window 1800s"), "{follow}");
        assert!(follow.contains("`model` is ignored here"), "{follow}");
    }

    #[test]
    fn a_call_without_a_prompt_or_session_is_rejected() {
        let no_prompt =
            json!({ "kind": "spawn_agent", "session_id": "p", "args": { "adapter": "codex" } });
        assert!(parse_call(&no_prompt.to_string()).is_err());
        let no_session = json!({ "kind": "spawn_agent", "args": { "prompt": "x" } });
        assert!(parse_call(&no_session.to_string()).is_err());
        assert!(parse_call("not json").is_err());
        assert!(parse_call(&json!({ "kind": "other" }).to_string()).is_err());
    }

    /// claudo/inbox#210: after a daemon re-exec the relay reattaches to the
    /// child it was already following. A reattach carries no prompt — sending
    /// one would make the child redo work — so the parser must accept it
    /// without one, and must refuse one that names no child.
    #[test]
    fn a_follow_agent_call_reattaches_to_a_child_without_a_prompt() {
        let line = json!({
            "kind": "follow_agent",
            "session_id": "parent-1",
            "proto": 3,
            "args": { "session_id": "child-42" },
        })
        .to_string();
        let call = parse_call(&line).unwrap();
        assert_eq!(call.session_id, "parent-1");
        let CallKind::Follow(child) = call.kind else { panic!("expected a reattach") };
        assert_eq!(child, "child-42");

        let no_child = json!({ "kind": "follow_agent", "session_id": "parent-1", "args": {} });
        assert!(
            parse_call(&no_child.to_string()).is_err(),
            "a reattach that names no child has nothing to follow"
        );
    }

    #[test]
    fn model_spellings_normalize_to_adapter_ids() {
        assert_eq!(normalize_adapter("claude_code"), "claude-code");
        assert_eq!(normalize_adapter("Claude"), "claude-code");
        assert_eq!(normalize_adapter("codex-cli"), "codex");
        assert_eq!(normalize_adapter(" opencode "), "opencode");
    }

    #[test]
    fn reply_frames_distinguish_success_failure_and_silence() {
        let out = |text: Option<&str>, err: Option<&str>| ChildOutcome {
            final_text: text.map(str::to_owned),
            error: err.map(str::to_owned),
            local_id: Some("child-7".into()),
            tail_is_thinking: false,
        };
        let ok = reply_frame(&out(Some("verdict: ship"), None));
        assert_eq!(ok["ok"], json!(true));
        let text = ok["result"].as_str().unwrap();
        assert!(text.starts_with("verdict: ship"));
        assert!(text.contains("child-7"), "reply must carry the child id: {text}");

        let failed = reply_frame(&out(None, Some("crashed")));
        assert_eq!(failed["ok"], json!(false));
        assert!(failed["error"].as_str().unwrap().contains("crashed"));

        let partial = reply_frame(&out(Some("got halfway"), Some("killed")));
        assert_eq!(partial["ok"], json!(false));
        let text = partial["error"].as_str().unwrap();
        assert!(text.contains("killed") && text.contains("got halfway"));

        let silent = reply_frame(&out(None, None));
        assert_eq!(silent["ok"], json!(true));
        assert!(silent["result"].as_str().unwrap().contains("without producing any output"));
    }

    #[tokio::test]
    async fn follow_child_streams_progress_then_final_result() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        let observer = watch.clone();
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            observer.observe(&cctui_proto::adapter::AdapterEvent::Message {
                local_id: "child-1".into(),
                payload: json!({ "role": "assistant", "text": "all done" }),
            });
            observer.observe(&cctui_proto::adapter::AdapterEvent::SessionEnded {
                local_id: "child-1".into(),
                reason: cctui_proto::adapter::EndReason::Completed,
            });
        });
        let mut out: Vec<u8> = Vec::new();
        let frame = follow_result_to_frame(
            follow_child_with(
                &handle,
                "child-1",
                Duration::from_secs(10),
                SILENT_CHILD_GRACE,
                2,
                &mut out,
            )
            .await,
        );
        feeder.await.unwrap();
        assert_eq!(frame["ok"], json!(true));
        assert!(frame["result"].as_str().unwrap().starts_with("all done"));
    }

    #[tokio::test]
    async fn follow_child_times_out_with_a_follow_up_hint() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        watch.observe(&cctui_proto::adapter::AdapterEvent::SessionStarted {
            local_id: "child-1".into(),
            meta: cctui_proto::adapter::SessionMeta::default(),
        });
        let mut out: Vec<u8> = Vec::new();
        let frame = follow_result_to_frame(
            follow_child_with(
                &handle,
                "child-1",
                Duration::from_millis(10),
                SILENT_CHILD_GRACE,
                1,
                &mut out,
            )
            .await,
        );
        assert_eq!(frame["ok"], json!(false));
        let text = frame["error"].as_str().unwrap();
        assert!(text.contains("NOT A CRASH"), "{text}");
        assert!(text.contains("still running"), "{text}");
        assert!(text.contains("on disk"), "{text}");
        assert!(text.contains("session_id"), "{text}");
        assert!(out.is_empty(), "proto 1 must never receive progress frames");
    }

    #[tokio::test]
    async fn a_silent_child_fails_fast_instead_of_waiting_out_the_timeout() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        watch.observe(&cctui_proto::adapter::AdapterEvent::SessionStarted {
            local_id: "child-1".into(),
            meta: cctui_proto::adapter::SessionMeta::default(),
        });
        let mut out: Vec<u8> = Vec::new();
        let frame = follow_result_to_frame(
            follow_child_with(
                &handle,
                "child-1",
                Duration::from_mins(30),
                Duration::from_millis(10),
                2,
                &mut out,
            )
            .await,
        );
        assert_eq!(frame["ok"], json!(false));
        let text = frame["error"].as_str().unwrap();
        assert!(text.contains("no activity"), "{text}");
        assert!(text.contains("died at startup"), "{text}");
    }

    #[tokio::test]
    async fn a_child_that_showed_activity_is_never_failed_fast() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        watch.observe(&cctui_proto::adapter::AdapterEvent::SessionStarted {
            local_id: "child-1".into(),
            meta: cctui_proto::adapter::SessionMeta::default(),
        });
        watch.observe(&cctui_proto::adapter::AdapterEvent::ToolUse {
            local_id: "child-1".into(),
            payload: json!({ "tool": "Bash" }),
        });
        let mut out: Vec<u8> = Vec::new();
        let frame = follow_result_to_frame(
            follow_child_with(
                &handle,
                "child-1",
                Duration::from_millis(20),
                Duration::from_millis(10),
                1,
                &mut out,
            )
            .await,
        );
        let text = frame["error"].as_str().unwrap();
        assert!(
            text.contains("still running"),
            "a working child must hit the timeout path: {text}"
        );
    }

    #[tokio::test]
    async fn a_crashed_child_returns_before_the_timeout() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        let observer = watch.clone();
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            observer.observe(&cctui_proto::adapter::AdapterEvent::SessionEnded {
                local_id: "child-1".into(),
                reason: cctui_proto::adapter::EndReason::Crashed {
                    detail: "gateway rejected the first model call".into(),
                },
            });
        });
        let mut out: Vec<u8> = Vec::new();
        let started = Instant::now();
        let frame = follow_result_to_frame(
            follow_child_with(
                &handle,
                "child-1",
                Duration::from_mins(30),
                Duration::from_mins(30),
                2,
                &mut out,
            )
            .await,
        );
        feeder.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(30), "must not wait out the timeout");
        assert_eq!(frame["ok"], json!(false));
        assert!(frame["error"].as_str().unwrap().contains("gateway rejected"));
    }

    #[test]
    fn truncated_non_answers_are_detected() {
        assert!(looks_truncated(None));
        assert!(looks_truncated(Some("   \n  ")));
        assert!(looks_truncated(Some("Reviewing the diff.\n\nNow I need to verify key claims:")));
        assert!(looks_truncated(Some("Here is my plan.\n1.")));
        assert!(looks_truncated(Some("First steps:\n-")));
        assert!(looks_truncated(Some("Considering the options:\n2)")));
    }

    #[test]
    fn real_deliverables_are_not_truncated() {
        assert!(!looks_truncated(Some("VERDICT: approve")));
        assert!(!looks_truncated(Some(
            "Findings:\n1. off-by-one at line 4\n2. missing await\n\nOverall: ship after fixes."
        )));
        assert!(!looks_truncated(Some("No issues found.")));
        assert!(!looks_truncated(Some("Line 4: the guard is inverted.")));
    }

    #[test]
    fn only_clean_truncated_turns_are_nudged_never_crashes() {
        let outcome = |text: Option<&str>, err: Option<&str>| ChildOutcome {
            final_text: text.map(str::to_owned),
            error: err.map(str::to_owned),
            local_id: Some("child-1".into()),
            tail_is_thinking: false,
        };
        assert!(should_nudge(&outcome(Some("Now I need to verify key claims:"), None)));
        assert!(should_nudge(&outcome(None, None)));
        assert!(!should_nudge(&outcome(Some("VERDICT: approve"), None)));
        assert!(!should_nudge(&outcome(Some("Now I need to verify:"), Some("crashed"))));
        assert!(!should_nudge(&outcome(None, Some("gateway rejected"))));
        assert!(!should_nudge(&outcome(Some("Findings: none, ship it."), None)));
    }

    #[test]
    fn a_thinking_tail_nudges_even_when_the_held_text_reads_complete() {
        let outcome = |thinking: bool, err: Option<&str>| ChildOutcome {
            final_text: Some("Verified the fix, all tests pass.".to_owned()),
            error: err.map(str::to_owned),
            local_id: Some("child-1".into()),
            tail_is_thinking: thinking,
        };
        assert!(should_nudge(&outcome(true, None)));
        assert!(!should_nudge(&outcome(false, None)));
        assert!(!should_nudge(&outcome(true, Some("crashed"))));
    }

    #[test]
    fn narration_then_thinking_end_nudges_but_a_final_text_after_thinking_does_not() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        watch.observe(&cctui_proto::adapter::AdapterEvent::Message {
            local_id: "child-1".into(),
            payload: json!({ "role": "assistant", "text": "Checked the diff, looks clean." }),
        });
        watch.observe(&cctui_proto::adapter::AdapterEvent::Message {
            local_id: "child-1".into(),
            payload: json!({ "role": "assistant_thinking", "text": "now let me verify" }),
        });
        watch.observe(&cctui_proto::adapter::AdapterEvent::SessionEnded {
            local_id: "child-1".into(),
            reason: cctui_proto::adapter::EndReason::Completed,
        });
        let snap = handle.snapshot().unwrap();
        let Assessment::Finished(outcome) = snap.assess(Instant::now()) else {
            panic!("ended child must be finished");
        };
        assert!(outcome.tail_is_thinking);
        assert!(should_nudge(&outcome), "stale narration held as final text must nudge");

        let handle = watch.register("child-2");
        watch.observe(&cctui_proto::adapter::AdapterEvent::Message {
            local_id: "child-2".into(),
            payload: json!({ "role": "assistant_thinking", "text": "planning" }),
        });
        watch.observe(&cctui_proto::adapter::AdapterEvent::Message {
            local_id: "child-2".into(),
            payload: json!({ "role": "assistant", "text": "VERDICT: approve" }),
        });
        watch.observe(&cctui_proto::adapter::AdapterEvent::SessionEnded {
            local_id: "child-2".into(),
            reason: cctui_proto::adapter::EndReason::Completed,
        });
        let snap = handle.snapshot().unwrap();
        let Assessment::Finished(outcome) = snap.assess(Instant::now()) else {
            panic!("ended child must be finished");
        };
        assert!(!outcome.tail_is_thinking);
        assert!(!should_nudge(&outcome));
    }

    #[test]
    fn a_killed_child_is_never_nudged() {
        let watch = std::sync::Arc::new(crate::childwatch::ChildWatch::default());
        let handle = watch.register("child-1");
        watch.observe(&cctui_proto::adapter::AdapterEvent::Message {
            local_id: "child-1".into(),
            payload: json!({ "role": "assistant", "text": "Now I need to verify key claims:" }),
        });
        watch.observe(&cctui_proto::adapter::AdapterEvent::SessionEnded {
            local_id: "child-1".into(),
            reason: cctui_proto::adapter::EndReason::Killed,
        });
        let snap = handle.snapshot().unwrap();
        let Assessment::Finished(outcome) = snap.assess(Instant::now()) else {
            panic!("killed child must be finished");
        };
        assert!(outcome.error.as_deref().unwrap().contains("killed"));
        assert!(
            !should_nudge(&outcome),
            "a killed child must never be nudged even on truncated text"
        );
    }

    #[test]
    fn tool_is_unavailable_without_a_machine_key() {
        assert!(!is_available(""));
        assert!(!is_available("   "));
        assert!(is_available("machine-key"));
    }
}
