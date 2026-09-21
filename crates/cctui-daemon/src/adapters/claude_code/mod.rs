//! Claude Code adapter.
//!
//! Two paths coexist behind a runtime feature flag:
//!
//! - **`claude daemon` client (default)** — connects to the supervisor
//!   socket at `/tmp/cc-daemon-<uid>/<hash>/control.sock`, polls `list`,
//!   merges identity from `~/.claude/jobs/<short>/state.json`, and emits
//!   real `AdapterEvent`s.
//! - **Legacy uds** — a Unix domain socket at `$CCTUI_DAEMON_SOCK` (or
//!   `$XDG_RUNTIME_DIR/cctui-daemon.sock`) accepts line-delimited
//!   [`AdapterEvent`] JSON from clients. Opt in via
//!   `CCTUI_ADAPTER_CLAUDE_DAEMON=0` or `config.mode = "legacy"`. Kept
//!   until the legacy path is retired.

mod attach;
mod backfill;
mod claude_service;
mod control;
pub(crate) use control::stage_mid_chat_files;
mod diagnose;
mod discovery;
mod dispatch_done;
mod envcheck;
mod fork_slice;
mod headless;
mod kickstart;
mod mode;
mod oneshot;
mod pty_view;
mod socket;
pub(crate) mod state;
mod streamjson;
mod transcript;
mod version_gate;

use mode::Mode;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cctui_proto::adapter::AdapterEvent;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use crate::adapter_runtime::{Adapter, AdapterCtx, AdapterFactory};

/// Shared `session_id → stable local_id` map, populated by the control driver
/// as it pins transcripts and read by the ask-hook listener to translate the
/// live `session_id` a hook reports into the `local_id` the server keys on.
pub(crate) type SessionMap = Arc<Mutex<HashMap<String, String>>>;

/// Shared map of `local_id`s with an `AskUserQuestion` form currently up in
/// the worker's PTY, each carrying the raw `questions` array the
/// hook delivered (`None` for deliveries without it). Maintained by the
/// ask-hook listener (insert on `kind:"ask"`, remove on `kind:"resolved"`)
/// and consulted by the driver's reply path: a `reply` op injected while the
/// form is up just confirms the highlighted option (the swallowed-text /
/// phantom-"Proceed" bug). With the questions in hand the reply path can
/// instead drive the real form via keystrokes — a native answer claude records
/// as a genuine `tool_result` — falling back to dismiss-then-reply.
pub(crate) type PendingAsks = Arc<Mutex<HashMap<String, Option<serde_json::Value>>>>;

/// A tool-permission hook currently parked in `handle_hook_connection`,
/// long-polling for a human's decision. Keyed by the stable
/// `local_id` of the session the blocked `PreToolUse` hook belongs to (the
/// listener resolves the hook's live `session_id` through [`SessionMap`]). The
/// `oneshot::Sender<bool>` resolves the hook: `true` → the hook returns an
/// `allow` decision, `false` → `deny`. Dropping the sender (timeout / session
/// gone) lets the hook fall through to the keystroke path. The driver's
/// `PermissionResponse` handler resolves the entry instead of attaching +
/// injecting keystrokes whenever one is registered for the target session.
pub(crate) type PendingPermHooks = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>>;

/// Last hook delivery per stable `local_id`: `(kind, when)`. Maintained by
/// the ask-hook listener and read by the session-diagnose aggregation
/// so "last hook event: type + age" is answerable.
pub(crate) type HookLog = Arc<Mutex<HashMap<String, (String, std::time::SystemTime)>>>;

/// Record a hook delivery for diagnose. Best-effort.
fn record_hook(log: &HookLog, local_id: &str, kind: &str) {
    if let Ok(mut m) = log.lock() {
        m.insert(local_id.to_owned(), (kind.to_owned(), std::time::SystemTime::now()));
    }
}

pub struct ClaudeCodeAdapter;

#[async_trait::async_trait]
impl Adapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    async fn start(&self, ctx: AdapterCtx) -> anyhow::Result<()> {
        match Mode::from_config(&ctx.config) {
            Mode::Bg => Box::pin(start_bg(ctx)).await,
            // Oneshot driver: one transient `claude -p` per turn,
            // mapped onto the AdapterCommand/AdapterEvent surface via the shared
            // stream-json codec. It binds the same `--settings` ask/permission
            // hook socket bg uses, so headless `-p` runs deliver hooks through
            // the same path.
            Mode::Oneshot => oneshot::OneshotDriver::new(ctx).run().await,
            // SDK driver: stream-json over the Claude Agent SDK, mapped onto the
            // AdapterCommand/AdapterEvent surface.
            Mode::Sdk => headless::SdkDriver::new(ctx).run().await,
            Mode::Legacy => run_legacy_uds(ctx).await,
        }
    }
}

/// The default `claude daemon` control-socket path: build the control driver,
/// spawn the shared ask/permission hook listener, and run. Behavior is
/// byte-for-byte the bg path.
async fn start_bg(ctx: AdapterCtx) -> anyhow::Result<()> {
    tracing::info!("claude-code adapter starting in claude-daemon mode");
    let cfg = control::DriverConfig::from_value(&ctx.config);
    let driver = control::Driver::new(cfg, ctx.events.clone(), ctx.commands, ctx.shutdown.clone())
        // Gateway-env launch chokepoint source.
        .with_server(ctx.server.clone(), ctx.machine_key.clone());
    // The `AskUserQuestion` PreToolUse hook delivers the pending
    // question here over the daemon's local socket. The hook reports claude's
    // live `session_id`; the driver's shared map translates it to the stable
    // `local_id` the rest of the pipeline (and the server) keys on.
    let hook_sock = resolve_legacy_socket_path(&ctx.config);
    let hook_events = ctx.events;
    let hook_shutdown = ctx.shutdown;
    let session_map = driver.session_map();
    let pending_asks = driver.pending_asks();
    let pending_perm_hooks = driver.pending_perm_hooks();
    let hook_log = driver.hook_log();
    tokio::spawn(async move {
        if let Err(err) = run_hook_listener(
            hook_sock,
            hook_events,
            hook_shutdown,
            session_map,
            pending_asks,
            pending_perm_hooks,
            hook_log,
        )
        .await
        {
            tracing::warn!(%err, "claude-code ask-hook listener exited");
        }
    });
    driver.run().await
}

async fn run_legacy_uds(ctx: AdapterCtx) -> anyhow::Result<()> {
    let path = resolve_legacy_socket_path(&ctx.config);
    let _ = std::fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
    }
    tracing::info!(socket = %path.display(), "claude-code legacy uds adapter listening");

    loop {
        tokio::select! {
            () = ctx.shutdown.cancelled() => {
                let _ = std::fs::remove_file(&path);
                return Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let events = ctx.events.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_legacy_connection(stream, events).await {
                        tracing::warn!(%err, "claude-code uds connection error");
                    }
                });
            }
        }
    }
}

async fn handle_legacy_connection(
    stream: tokio::net::UnixStream,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
) -> anyhow::Result<()> {
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let evt: AdapterEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(%err, ?line, "ignoring non-AdapterEvent uds line");
                continue;
            }
        };
        if events.send(evt).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Listen on the daemon's local socket for `AskUserQuestion` hook deliveries.
/// Each line is a `{kind, session_id, question?}` message from the
/// `cctui-daemon ask-hook` command; we translate `session_id → local_id` via
/// the shared map and emit the existing `AskQuestion` / `AskResolved` events.
async fn run_hook_listener(
    path: PathBuf,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
    shutdown: CancellationToken,
    session_map: SessionMap,
    pending_asks: PendingAsks,
    pending_perm_hooks: PendingPermHooks,
    hook_log: HookLog,
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
    tracing::info!(socket = %path.display(), "claude-code ask-hook listener ready");

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                let _ = std::fs::remove_file(&path);
                return Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let events = events.clone();
                let session_map = session_map.clone();
                let pending_asks = pending_asks.clone();
                let pending_perm_hooks = pending_perm_hooks.clone();
                let hook_log = hook_log.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_hook_connection(
                        stream,
                        events,
                        session_map,
                        pending_asks,
                        pending_perm_hooks,
                        hook_log,
                    )
                    .await
                    {
                        tracing::debug!(%err, "ask-hook connection error");
                    }
                });
            }
        }
    }
}

/// How long the daemon parks a blocking `PreToolUse` permission hook waiting
/// for a human's allow/deny before giving up and letting it fall through to the
/// keystroke path. Bounded well under the hook's own configured
/// `timeout` ceiling (which we set high — see `ensure_hook_settings`): a hook
/// that exceeds *its* timeout is treated by Claude Code as a hard deny, so we
/// must always resolve (with a `defer` decision) before that fires. A
/// generous-but-finite human window; on expiry the hook returns `defer` and the
/// existing `tempo:"blocked"`/`needs` keystroke path takes over.
const PERM_HOOK_WAIT: std::time::Duration = std::time::Duration::from_mins(5);

async fn handle_hook_connection(
    stream: tokio::net::UnixStream,
    events: tokio::sync::mpsc::Sender<AdapterEvent>,
    session_map: SessionMap,
    pending_asks: PendingAsks,
    pending_perm_hooks: PendingPermHooks,
    hook_log: HookLog,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A bidirectional tool-permission hook: register a pending
        // decision keyed by the session's stable `local_id`, surface a
        // `PermissionRequest` to clients, then block this connection until the
        // human's decision arrives (resolved by the driver's
        // `PermissionResponse` handler) or the bounded wait expires. The hook
        // long-polls the same connection, so the decision we write back is what
        // it returns to Claude Code — no attach + keystroke in the common case.
        if let Some(req) = parse_perm_request(line, &session_map) {
            record_hook(&hook_log, &req.local_id, "perm-request");
            let decision = wait_for_perm_decision(req, &events, &pending_perm_hooks).await;
            let _ = write_half.write_all(decision.as_bytes()).await;
            let _ = write_half.write_all(b"\n").await;
            let _ = write_half.flush().await;
            // One request per connection; the hook closes after reading.
            return Ok(());
        }
        let Some(evt) = hook_line_to_event(line, &session_map) else {
            continue;
        };
        // Remember the delivery for diagnose ("last hook event: type, age").
        match &evt {
            AdapterEvent::AskQuestion { local_id, .. } => record_hook(&hook_log, local_id, "ask"),
            AdapterEvent::AskResolved { local_id } => record_hook(&hook_log, local_id, "resolved"),
            AdapterEvent::PlanRequest { local_id, .. } => record_hook(&hook_log, local_id, "plan"),
            AdapterEvent::PlanResolved { local_id } => {
                record_hook(&hook_log, local_id, "plan_resolved");
            }
            _ => {}
        }
        // Track which sessions have the ask form up, keeping the
        // questions payload so the driver's reply path can answer the form
        // natively via keystrokes or dismiss it before injecting text.
        if let Ok(mut map) = pending_asks.lock() {
            match &evt {
                AdapterEvent::AskQuestion { local_id, questions, .. } => {
                    map.insert(local_id.clone(), questions.clone());
                }
                // A plan-approval prompt is a single-select form too:
                // store a synthetic single question whose options are the known
                // ExitPlanMode continuations so the reply path answers picks
                // 1-3 natively via `ask_keystrokes`. Option 4 ("Tell Claude what
                // to change") is free-text and takes the dismiss-then-reply
                // fallback like any free-text ask answer.
                AdapterEvent::PlanRequest { local_id, .. } => {
                    map.insert(local_id.clone(), Some(plan_form_questions()));
                }
                // Either form being resolved clears its pending entry.
                AdapterEvent::AskResolved { local_id }
                | AdapterEvent::PlanResolved { local_id } => {
                    map.remove(local_id);
                }
                _ => {}
            }
        }
        if events.send(evt).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// The synthetic single-select `questions` payload for an `ExitPlanMode`
/// plan-approval prompt. The option set/order mirrors the CLI's PTY
/// prompt (verified against the claude 2.x plan-mode form); it is hardcoded
/// because the labels are rendered by the CLI, not carried in `tool_input`.
/// Stored in `pending_asks` so the reply path can answer picks 1-3 natively via
/// `ask_keystrokes` (a lone single-select submits straight from its digit).
fn plan_form_questions() -> serde_json::Value {
    serde_json::json!([{
        "question": "Ready to code?",
        "options": [
            { "label": "Yes, and auto-accept edits" },
            { "label": "Yes, and manually approve edits" },
            { "label": "No, keep planning" },
            { "label": "Tell Claude what to change" }
        ]
    }])
}

/// Parse one hook line and resolve it to an `AdapterEvent`. The hook reports
/// claude's live `session_id`; we map it to the stable `local_id` (falling
/// back to the `session_id` itself before the driver has pinned the session).
fn hook_line_to_event(line: &str, session_map: &SessionMap) -> Option<AdapterEvent> {
    let v: serde_json::Value = serde_json::from_str(line)
        .map_err(|err| tracing::warn!(%err, ?line, "ignoring malformed ask-hook line"))
        .ok()?;
    let session_id = v.get("session_id").and_then(|s| s.as_str())?;
    let local_id = session_map
        .lock()
        .ok()
        .and_then(|m| m.get(session_id).cloned())
        .unwrap_or_else(|| session_id.to_owned());
    match v.get("kind").and_then(|k| k.as_str()) {
        Some("ask") => {
            let question =
                v.get("question").and_then(|q| q.as_str()).unwrap_or_default().to_owned();
            // Pass the structured `questions` array through so the
            // webui renders interactive option cards live. `null`/absent →
            // `None`, leaving clients to fall back to the text form.
            let questions = v.get("questions").filter(|q| !q.is_null()).cloned();
            // The assistant prose preceding the question in the same turn, read
            // from the transcript by the `ask-hook` subcommand so the live card
            // carries its context. Absent/empty → `None`.
            let preamble = v
                .get("preamble")
                .and_then(|p| p.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned);
            Some(AdapterEvent::AskQuestion { local_id, question, questions, preamble })
        }
        Some("resolved") => Some(AdapterEvent::AskResolved { local_id }),
        Some("plan") => {
            // ExitPlanMode plan-approval prompt. `plan` is the plan
            // markdown; `preamble` the prose before the tool call, same shape
            // as the ask path.
            let plan = v.get("plan").and_then(|p| p.as_str()).unwrap_or_default().to_owned();
            let preamble = v
                .get("preamble")
                .and_then(|p| p.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned);
            Some(AdapterEvent::PlanRequest { local_id, plan, preamble })
        }
        Some("plan_resolved") => Some(AdapterEvent::PlanResolved { local_id }),
        other => {
            tracing::warn!(?other, "ignoring ask-hook line with unknown kind");
            None
        }
    }
}

/// A tool-permission hook delivery awaiting a decision.
struct PermRequest {
    /// Stable session id the request belongs to (resolved through `SessionMap`).
    local_id: String,
    /// Correlation id minted by the hook process; echoed in the
    /// `PermissionRequest` so a decision from any surface routes back here.
    request_id: String,
    /// The tool Claude Code is about to run (`tool_name`), e.g. `Bash`.
    tool: String,
    /// The tool's input payload, surfaced to clients so they can show what
    /// would run before approving.
    input: serde_json::Value,
}

/// Parse a `kind:"perm-request"` hook line into a [`PermRequest`], resolving the
/// live `session_id` to the stable `local_id` via the shared map. Returns
/// `None` for any other line shape so the caller falls through to the existing
/// ask/resolved event path.
fn parse_perm_request(line: &str, session_map: &SessionMap) -> Option<PermRequest> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("kind").and_then(|k| k.as_str()) != Some("perm-request") {
        return None;
    }
    let session_id = v.get("session_id").and_then(|s| s.as_str())?;
    let local_id = session_map
        .lock()
        .ok()
        .and_then(|m| m.get(session_id).cloned())
        .unwrap_or_else(|| session_id.to_owned());
    let request_id = v
        .get("hook_id")
        .and_then(|s| s.as_str())
        .map_or_else(|| format!("hook-perm-{session_id}"), str::to_owned);
    let tool = v.get("tool").and_then(|s| s.as_str()).unwrap_or_default().to_owned();
    let input = v.get("input").cloned().unwrap_or(serde_json::Value::Null);
    Some(PermRequest { local_id, request_id, tool, input })
}

/// Surface a `PermissionRequest`, register the pending hook decision, then park
/// until the human answers (driver's `PermissionResponse`) or the bounded wait
/// expires. Returns the JSON line the hook writes to stdout as Claude Code's
/// `PreToolUse` decision: `allow`/`deny` on a human answer, or `defer` on
/// timeout so the normal prompt renders and the keystroke fallback applies.
async fn wait_for_perm_decision(
    req: PermRequest,
    events: &tokio::sync::mpsc::Sender<AdapterEvent>,
    pending_perm_hooks: &PendingPermHooks,
) -> String {
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    // Register before emitting so a fast decision can't race past an empty map.
    if let Ok(mut map) = pending_perm_hooks.lock() {
        map.insert(req.local_id.clone(), tx);
    }
    let _ = events
        .send(AdapterEvent::PermissionRequest {
            local_id: req.local_id.clone(),
            request_id: req.request_id.clone(),
            tool: req.tool.clone(),
            input: req.input,
        })
        .await;

    let outcome = tokio::time::timeout(PERM_HOOK_WAIT, rx).await;
    // Always clear our slot: on timeout the sender is dropped here; on a
    // delivered decision the driver already removed it (a stale re-insert from a
    // racing request is harmless — it's keyed by local_id and replaced).
    if let Ok(mut map) = pending_perm_hooks.lock() {
        map.remove(&req.local_id);
    }
    // Tell clients the inline prompt is no longer pending regardless of outcome.
    let _ = events
        .send(AdapterEvent::PermissionResolved {
            local_id: req.local_id.clone(),
            request_id: req.request_id.clone(),
        })
        .await;

    let decision = match outcome {
        Ok(Ok(true)) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": "Approved from cctui.",
            },
        }),
        Ok(Ok(false)) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "Denied from cctui.",
            },
        }),
        // Sender dropped (driver replaced our slot) or wait timed out: defer to
        // the normal permission flow so the keystroke fallback can answer.
        _ => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "defer",
            },
        }),
    };
    tracing::info!(
        local_id = %req.local_id,
        request_id = %req.request_id,
        tool = %req.tool,
        decision = %decision["hookSpecificOutput"]["permissionDecision"],
        "resolved PreToolUse permission hook",
    );
    decision.to_string()
}

pub(crate) fn resolve_legacy_socket_path(config: &serde_json::Value) -> PathBuf {
    if let Some(p) = config.get("socket_path").and_then(|v| v.as_str()) {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("CCTUI_DAEMON_SOCK") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("cctui-daemon.sock")
}

pub struct ClaudeCodeFactory;

impl AdapterFactory for ClaudeCodeFactory {
    fn id(&self) -> &'static str {
        "claude-code"
    }
    fn build(&self, _config: serde_json::Value) -> Box<dyn Adapter> {
        Box::new(ClaudeCodeAdapter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_via_config_mode() {
        let v = serde_json::json!({"mode": "claude-daemon"});
        assert_eq!(Mode::from_config(&v), Mode::Bg);
    }

    #[test]
    fn flag_via_config_mode_other_value() {
        assert_eq!(Mode::from_config(&serde_json::json!({"mode": "legacy"})), Mode::Legacy);
    }

    #[test]
    fn hook_line_plan_maps_to_plan_request() {
        // an ExitPlanMode plan hook line carries the plan markdown +
        // optional preamble and maps to PlanRequest.
        let map: SessionMap = Arc::default();
        let line = r##"{"kind":"plan","session_id":"s1","plan":"# Plan\n- step one","preamble":"Here is my plan."}"##;
        match hook_line_to_event(line, &map) {
            Some(AdapterEvent::PlanRequest { local_id, plan, preamble }) => {
                assert_eq!(local_id, "s1");
                assert_eq!(plan, "# Plan\n- step one");
                assert_eq!(preamble.as_deref(), Some("Here is my plan."));
            }
            other => panic!("expected PlanRequest, got {other:?}"),
        }
    }

    #[test]
    fn hook_line_plan_blank_preamble_is_none() {
        let map: SessionMap = Arc::default();
        let line = r#"{"kind":"plan","session_id":"s1","plan":"do it","preamble":"   "}"#;
        match hook_line_to_event(line, &map) {
            Some(AdapterEvent::PlanRequest { preamble, .. }) => assert_eq!(preamble, None),
            other => panic!("expected PlanRequest, got {other:?}"),
        }
    }

    #[test]
    fn hook_line_plan_resolved_maps_to_plan_resolved() {
        let map: SessionMap = Arc::default();
        let line = r#"{"kind":"plan_resolved","session_id":"s1"}"#;
        match hook_line_to_event(line, &map) {
            Some(AdapterEvent::PlanResolved { local_id }) => assert_eq!(local_id, "s1"),
            other => panic!("expected PlanResolved, got {other:?}"),
        }
    }

    #[test]
    fn plan_form_questions_is_single_select_with_four_options() {
        // The synthetic plan form must be a lone single-select question (no
        // multiSelect) so `ask_keystrokes` answers a digit pick natively
        // without a review screen.
        let qs = plan_form_questions();
        let arr = qs.as_array().expect("array");
        assert_eq!(arr.len(), 1, "exactly one synthetic question");
        assert_eq!(arr[0]["options"].as_array().unwrap().len(), 4);
        assert!(arr[0].get("multiSelect").is_none(), "single-select");
    }

    #[test]
    fn ask_hook_line_carries_structured_questions() {
        // the hook forwards the raw `questions` array so the webui can
        // render the interactive form live, not just the flattened text.
        let map: SessionMap = Arc::default();
        let line = r#"{"kind":"ask","session_id":"s1","question":"Color: pick","questions":[{"question":"Color?","options":[{"label":"Red"}]}]}"#;
        match hook_line_to_event(line, &map) {
            Some(AdapterEvent::AskQuestion { local_id, question, questions, .. }) => {
                assert_eq!(local_id, "s1");
                assert_eq!(question, "Color: pick");
                let qs = questions.expect("structured questions present");
                assert_eq!(qs[0]["question"], "Color?");
                assert_eq!(qs[0]["options"][0]["label"], "Red");
            }
            other => panic!("expected AskQuestion with questions, got {other:?}"),
        }
    }

    #[test]
    fn ask_hook_line_carries_preamble() {
        // the hook forwards the assistant prose preceding the question
        // (read from the transcript) so the live card isn't answered blind.
        let map: SessionMap = Arc::default();
        let line = r#"{"kind":"ask","session_id":"s1","question":"Pick","preamble":"Here is the analysis."}"#;
        match hook_line_to_event(line, &map) {
            Some(AdapterEvent::AskQuestion { preamble, .. }) => {
                assert_eq!(preamble.as_deref(), Some("Here is the analysis."));
            }
            other => panic!("expected AskQuestion with preamble, got {other:?}"),
        }
        // Blank/absent preamble → None so clients render the question alone.
        let blank = r#"{"kind":"ask","session_id":"s1","question":"Pick","preamble":"   "}"#;
        match hook_line_to_event(blank, &map) {
            Some(AdapterEvent::AskQuestion { preamble, .. }) => assert!(preamble.is_none()),
            other => panic!("expected AskQuestion, got {other:?}"),
        }
    }

    #[test]
    fn parse_perm_request_resolves_local_id_and_fields() {
        // a perm-request line is recognised, the live session_id is
        // mapped to the stable local_id, and tool/input/hook_id flow through.
        let map: SessionMap = Arc::default();
        map.lock().unwrap().insert("sess-live".into(), "local-42".into());
        let line = r#"{"kind":"perm-request","session_id":"sess-live","hook_id":"h1","tool":"Bash","input":{"command":"ls"}}"#;
        let req = parse_perm_request(line, &map).expect("perm-request parsed");
        assert_eq!(req.local_id, "local-42");
        assert_eq!(req.request_id, "h1");
        assert_eq!(req.tool, "Bash");
        assert_eq!(req.input["command"], "ls");
    }

    #[test]
    fn parse_perm_request_ignores_other_kinds() {
        let map: SessionMap = Arc::default();
        assert!(parse_perm_request(r#"{"kind":"ask","session_id":"s"}"#, &map).is_none());
        assert!(parse_perm_request(r#"{"kind":"resolved","session_id":"s"}"#, &map).is_none());
        assert!(parse_perm_request("not json", &map).is_none());
    }

    #[test]
    fn parse_perm_request_falls_back_to_session_id_when_unmapped() {
        // Before the driver has pinned the session, the live session_id is used
        // as the local_id and a synthesized hook_id covers a missing one.
        let map: SessionMap = Arc::default();
        let line = r#"{"kind":"perm-request","session_id":"sX","tool":"Write"}"#;
        let req = parse_perm_request(line, &map).expect("parsed");
        assert_eq!(req.local_id, "sX");
        assert_eq!(req.request_id, "hook-perm-sX");
        assert_eq!(req.tool, "Write");
    }

    #[tokio::test]
    async fn wait_for_perm_decision_emits_request_then_returns_allow() {
        // A registered decision resolves the parked hook to an `allow` JSON line.
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let hooks: PendingPermHooks = Arc::default();
        let req = PermRequest {
            local_id: "L1".into(),
            request_id: "r1".into(),
            tool: "Bash".into(),
            input: json!({"command": "ls"}),
        };
        let hooks2 = hooks.clone();
        let join = tokio::spawn(async move { wait_for_perm_decision(req, &tx, &hooks).await });
        // The PermissionRequest must be emitted before we answer.
        match rx.recv().await.expect("request event") {
            AdapterEvent::PermissionRequest { local_id, tool, .. } => {
                assert_eq!(local_id, "L1");
                assert_eq!(tool, "Bash");
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }
        // Deliver the human decision the way the driver would.
        let sender = hooks2.lock().unwrap().remove("L1").expect("registered");
        sender.send(true).unwrap();
        let decision = join.await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&decision).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
        // A PermissionResolved follows so clients drop the inline card.
        assert!(matches!(rx.recv().await, Some(AdapterEvent::PermissionResolved { .. })));
    }

    #[test]
    fn ask_hook_line_without_questions_is_none() {
        // A legacy/text-only delivery (no `questions`) still yields an event,
        // with `questions: None` so clients fall back to the text form.
        let map: SessionMap = Arc::default();
        let line = r#"{"kind":"ask","session_id":"s1","question":"hi"}"#;
        match hook_line_to_event(line, &map) {
            Some(AdapterEvent::AskQuestion { questions, preamble, .. }) => {
                assert!(questions.is_none());
                assert!(preamble.is_none());
            }
            other => panic!("expected AskQuestion, got {other:?}"),
        }
    }
}
