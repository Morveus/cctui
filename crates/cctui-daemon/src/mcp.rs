//! `cctui-daemon mcp-agent` — the stdio MCP server a claude session is launched
//! with, exposing the `CctuiAgent` and `CctuiUsage` tools.
//!
//! The subcommand is a thin relay, mirroring `ask-hook`: it speaks MCP on
//! stdio and forwards each `tools/call` to the long-lived daemon over its local
//! Unix socket, which owns the machine key and the spawn path. The session id is
//! fixed by the `--session` argv the daemon wrote into the session's MCP config,
//! so a session can never ask on another session's behalf.
//!
//! Calls run concurrently — one thread per `tools/call`, replies keyed by
//! JSON-RPC id — so parallel child spawns actually run in parallel. While a
//! call waits, the daemon's interim progress frames are forwarded as MCP
//! `notifications/progress` (when the client sent a `progressToken`), which
//! resets the client's tool idle timeout: a long-running child no longer
//! looks like a dead call.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const TOOL_NAME: &str = "CctuiAgent";
pub const USAGE_TOOL_NAME: &str = "CctuiUsage";

/// A limits lookup is one cached server read; it must never hold a turn open
/// the way a followed child does.
const USAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// MCP protocol revision this server implements.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Socket line protocol revision: ≥2 tells the daemon this relay understands
/// interim `progress` frames before the final result line; ≥3 that it reads
/// the `attached` frame naming the child and can reattach with `follow_agent`.
const SOCKET_PROTO: u64 = 3;

/// How long the relay keeps trying to reach the daemon again after the socket
/// died under a call in flight. An auto-update `execve` rebinds the socket
/// within a few seconds; a service restart can take longer.
const REATTACH_WINDOW: Duration = Duration::from_mins(3);

/// Pause between reconnect attempts inside [`REATTACH_WINDOW`].
const REATTACH_RETRY: Duration = Duration::from_secs(2);

/// Ceiling on reattaches for one tool call. A daemon restarting in a loop must
/// surface as an error, not as a call that never returns.
const REATTACH_MAX: u32 = 5;

/// Ceiling on a single tool call, and the default when the call names none.
/// Generous: a child review session can legitimately run for many minutes.
const DEFAULT_TIMEOUT_SECS: u64 = 1800;
const MAX_TIMEOUT_SECS: u64 = 7200;

/// The `CctuiAgent` input schema, as advertised to the model.
#[must_use]
pub fn tool_schema() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Spawn a cctui subagent session, follow it while it works, and return \
    its final message. The child is a real cctui session: it appears nested under this one in \
    the UI, its token usage is metered, its spend is capped, and it can be killed. Progress \
    (current tool, status, latest message) streams back while it runs. Parallel calls are \
    supported. To send a follow-up prompt to a child from an earlier call, pass its session_id \
    (returned in the reply) together with the new prompt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "adapter": {
                    "type": "string",
                    "description": "Harness to run the child under, e.g. \"opencode\", \
    \"codex\", \"claude_code\". Only the adapters this session is permitted to spawn are \
    accepted. Ignored when session_id is set.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the child agent (or the follow-up message \
    when session_id is set).",
                },
                "session_id": {
                    "type": "string",
                    "description": "Session id of a child spawned earlier by this session: \
    send `prompt` to it as a follow-up and wait for its answer instead of spawning anew.",
                },
                "model": {
                    "type": "string",
                    "description": "REQUIRED. Model id to run the child on — there is no \
    account default, and a call without one is rejected. Known claude_code ids: \
    \"claude-opus-5[1m]\", \"claude-opus-5\", \"claude-sonnet-5\", \"claude-haiku-4-5\", \
    \"claude-fable-5\"; codex: \"gpt-5.6-sol\", \"gpt-5.6-terra\". An alias from the \
    account's own catalog also works. Ignored when session_id is set, but still name the \
    child's model so the call records what it is talking to.",
                },
                "agent_profile": {
                    "type": "string",
                    "description": "Named agent profile to run under, e.g. \"cctui-reviewer\" \
    (a locked-down opencode reviewer).",
                },
                "permission_mode": {
                    "type": "string",
                    "enum": ["yolo", "auto", "ask"],
                    "description": "Child permission posture. Default \"yolo\" (no prompts — \
    like a Task subagent, nobody is attached to answer them).",
                },
                "name": {
                    "type": "string",
                    "description": "Display name for the child session in the UI.",
                },
                "budget_usd": {
                    "type": "number",
                    "description": "Dollar ceiling for this child's own spend. Must not exceed \
    this session's permitted maximum; omit to inherit it.",
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the child. Defaults to this session's.",
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "How long to wait for the child before giving up \
    (default 1800, max 7200). Expiry is not a failure of the child: it keeps running, its \
    work stays on disk, and it can be reattached via session_id. Raise it for work that \
    routinely runs long. The effective window is echoed in the result.",
                },
            },
            "required": ["prompt", "model"],
            "additionalProperties": false,
        },
    })
}

/// The `CctuiUsage` input schema. No required arguments: the session id is
/// already baked into this relay's argv, so the tool always answers for the
/// caller and can never be pointed at another session.
#[must_use]
pub fn usage_tool_schema() -> Value {
    json!({
        "name": USAGE_TOOL_NAME,
        "description": "Report the rate limits and budget that apply to THIS session: the \
    account it is pinned to (which may be a shared or pool-elected one, not your own), that \
    account's usage windows, the caps in force, this session's dollar spend, and whether each \
    model it could run on is currently allowed or soft-limit blocked. Use it before dispatching \
    a batch of work, and when deciding which model to give a child: a blocked model wastes the \
    whole fan-out on 429s. Returns a one-line summary followed by the full JSON.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Ask about one model instead of this session's current one. \
    The per-model map is returned either way.",
                },
            },
            "required": [],
            "additionalProperties": false,
        },
    })
}

/// Clamp a caller-supplied timeout into the supported range.
#[must_use]
pub fn resolve_timeout(requested: Option<u64>) -> Duration {
    Duration::from_secs(requested.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS))
}

/// Build the `.mcp.json`-shaped config registering this server for a session.
///
/// `exe` is the daemon binary; the session id and socket are baked into argv so
/// the tool call carries no session identity of its own.
#[must_use]
pub fn mcp_config(exe: &str, session_id: &str, sock: &Path) -> Value {
    json!({
        "mcpServers": {
            "cctui": {
                "type": "stdio",
                "command": exe,
                "args": [
                    "mcp-agent",
                    "--session", session_id,
                    "--sock", sock.to_string_lossy(),
                ],
            }
        }
    })
}

/// Serialize one JSON-RPC frame to stdout. The lock is per-line so concurrent
/// tool calls interleave whole frames, never bytes.
#[derive(Clone)]
struct Outbox(Arc<Mutex<std::io::Stdout>>);

impl Outbox {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(std::io::stdout())))
    }

    fn send(&self, frame: &Value) {
        if let Ok(mut out) = self.0.lock() {
            let _ = writeln!(out, "{frame}");
            let _ = out.flush();
        }
    }
}

fn reply(id: Option<&Value>, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.cloned().unwrap_or(Value::Null), "result": result })
}

/// A tool result carrying `text`. `is_error` marks a failed call so the model
/// sees the failure instead of a hang.
fn tool_result(id: Option<&Value>, text: &str, is_error: bool) -> Value {
    reply(id, &json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
}

fn progress_notification(token: &Value, seq: u64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progressToken": token, "progress": seq, "message": message },
    })
}

/// The `_meta.progressToken` of a request, when the client sent one.
fn progress_token(req: &Value) -> Option<Value> {
    req.pointer("/params/_meta/progressToken").cloned().filter(|t| !t.is_null())
}

/// Handle one decoded JSON-RPC request inline; `None` for notifications (no
/// reply) AND for `tools/call`, which replies asynchronously from its own
/// thread via the outbox.
fn handle_request(session_id: &str, sock: &Path, req: &Value, outbox: &Outbox) -> Option<Value> {
    let method = req.get("method").and_then(Value::as_str)?;
    let id = req.get("id");
    match method {
        "initialize" => Some(reply(
            id,
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "cctui", "version": env!("CARGO_PKG_VERSION") },
            }),
        )),
        "tools/list" => Some(reply(id, &json!({ "tools": [tool_schema(), usage_tool_schema()] }))),
        "tools/call" => {
            let params = req.get("params");
            let name = params.and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("");
            let kind = match name {
                TOOL_NAME => "spawn_agent",
                USAGE_TOOL_NAME => "usage",
                other => return Some(tool_result(id, &format!("unknown tool {other:?}"), true)),
            };
            let args =
                params.and_then(|p| p.get("arguments")).cloned().unwrap_or_else(|| json!({}));
            let id = id.cloned();
            let token = progress_token(req);
            let session_id = session_id.to_owned();
            let sock = sock.to_owned();
            let outbox = outbox.clone();
            std::thread::spawn(move || {
                let (text, is_error) =
                    call_daemon(&session_id, &sock, kind, &args, token.as_ref(), &outbox);
                outbox.send(&tool_result(id.as_ref(), &text, is_error));
            });
            None
        }
        _ if id.is_none() => None,
        _ => Some(reply(id, &json!({}))),
    }
}

/// One connection's worth of a call: either the daemon answered, or the
/// socket died under us before it could.
enum Leg {
    /// Final result: the text the model sees, and whether it is an error.
    Done(String, bool),
    /// The daemon went away mid-call (EOF, reset). The child it was following
    /// is untouched — only this wait died.
    Dropped,
}

/// Drive one connection to the daemon: send `request`, forward interim
/// progress frames, record the child id the daemon announces, and stop at the
/// first final frame. `tool` names the caller in every error the model sees.
fn run_leg(
    stream: &UnixStream,
    request: &Value,
    token: Option<&Value>,
    outbox: &Outbox,
    seq: &mut u64,
    child: &mut Option<String>,
    tool: &str,
) -> Leg {
    let mut writer = stream;
    if writeln!(writer, "{request}").and_then(|()| writer.flush()).is_err() {
        return Leg::Dropped;
    }
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            // EOF: the daemon closed without a result. On this machine that is
            // the auto-update re-exec (`execve` drops every open connection at
            // once), which is why whole fan-outs used to die together.
            Ok(0) => return Leg::Dropped,
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Leg::Dropped;
            }
            // A read timeout is NOT a restart: the daemon owes us a result and
            // did not send one. Reattaching here would loop forever.
            Err(err) => {
                return Leg::Done(
                    format!("{tool} failed: lost the daemon connection ({err})"),
                    true,
                );
            }
        }
        if line.trim().is_empty() {
            return Leg::Dropped;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            return Leg::Done(format!("{tool} failed: malformed daemon reply"), true);
        };
        if let Some(id) = frame.get("attached").and_then(Value::as_str) {
            *child = Some(id.to_owned());
            continue;
        }
        if let Some(progress) = frame.get("progress").and_then(Value::as_str) {
            if let Some(token) = token {
                *seq += 1;
                outbox.send(&progress_notification(token, *seq, progress));
            }
            continue;
        }
        let ok = frame.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let text = frame
            .get(if ok { "result" } else { "error" })
            .and_then(Value::as_str)
            .unwrap_or("no output")
            .to_owned();
        return Leg::Done(text, !ok);
    }
}

/// Reconnect to the daemon socket, tolerating the window in which the binary
/// has been swapped but the new process has not bound the socket yet.
fn reconnect(sock: &Path, window: Duration, retry: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + window;
    loop {
        if let Ok(s) = UnixStream::connect(sock) {
            return Some(s);
        }
        if Instant::now() + retry >= deadline {
            return None;
        }
        std::thread::sleep(retry);
    }
}

/// The message a call returns when the daemon vanished before it had even
/// named the child. Nothing can be reattached: there is no id to reattach to.
fn restarted_before_attach(tool: &str) -> String {
    if tool != TOOL_NAME {
        return format!(
            "{tool} failed: the cctui daemon restarted (auto-update re-exec) while this call was \
             running. Nothing was left behind; call it again."
        );
    }
    "CctuiAgent failed: the cctui daemon restarted (auto-update re-exec) while this call was      starting, before it named the child session, so there is nothing to reattach to. THIS IS      NOT THE CHILD CRASHING. Check cctui for a child session under this one; if none appeared,      call CctuiAgent again."
        .to_owned()
}

/// Send the tool call to the daemon and block on its final result, forwarding
/// interim progress frames. When the daemon restarts mid-call (auto-update
/// re-exec closes every agent-tool connection at once), reconnect and reattach
/// to the same child with `follow_agent` instead of failing the call: the
/// child never stopped, only the wait did. Every failure returns text — the
/// model must see an error, never a hang.
fn call_daemon(
    session_id: &str,
    sock: &Path,
    kind: &str,
    args: &Value,
    token: Option<&Value>,
    outbox: &Outbox,
) -> (String, bool) {
    let tool = if kind == "usage" { USAGE_TOOL_NAME } else { TOOL_NAME };
    let timeout = if kind == "usage" {
        USAGE_TIMEOUT
    } else {
        resolve_timeout(args.get("timeout_secs").and_then(Value::as_u64))
    };
    let mut request = json!({
        "kind": kind,
        "session_id": session_id,
        "args": args,
        "timeout_secs": timeout.as_secs(),
        "proto": SOCKET_PROTO,
    });
    let mut stream = match UnixStream::connect(sock) {
        Ok(s) => s,
        Err(err) => {
            return (format!("{tool} unavailable: cannot reach the cctui daemon ({err})"), true);
        }
    };
    let mut seq: u64 = 0;
    let mut child: Option<String> = None;
    for attempt in 0..=REATTACH_MAX {
        // Outlive the daemon's own wait so the daemon's timeout message wins.
        let _ = stream.set_read_timeout(Some(timeout + Duration::from_secs(30)));
        match run_leg(&stream, &request, token, outbox, &mut seq, &mut child, tool) {
            Leg::Done(text, is_error) => return (text, is_error),
            Leg::Dropped => {}
        }
        let Some(id) = child.clone() else { return (restarted_before_attach(tool), true) };
        if attempt == REATTACH_MAX {
            return (
                format!(
                    "CctuiAgent failed: the cctui daemon kept restarting under this call                      ({REATTACH_MAX} reattaches). Child session {id} is unaffected and its work                      is on disk; follow it in cctui, or call CctuiAgent again with session_id                      {id:?} once the daemon is stable."
                ),
                true,
            );
        }
        if let Some(token) = token {
            seq += 1;
            outbox.send(&progress_notification(
                token,
                seq,
                &format!("the cctui daemon restarted; reattaching to child session {id}"),
            ));
        }
        let Some(fresh) = reconnect(sock, REATTACH_WINDOW, REATTACH_RETRY) else {
            return (
                format!(
                    "CctuiAgent failed: the cctui daemon restarted and did not come back within                      {}s. Child session {id} is still running on its own and its work is on                      disk; follow it in cctui.",
                    REATTACH_WINDOW.as_secs(),
                ),
                true,
            );
        };
        stream = fresh;
        request = json!({
            "kind": "follow_agent",
            "session_id": session_id,
            "args": { "session_id": id },
            "timeout_secs": timeout.as_secs(),
            "proto": SOCKET_PROTO,
        });
    }
    unreachable!("the loop returns on its last iteration")
}

/// Serve MCP on stdio until the client closes it.
pub fn run(session_id: &str, sock: &Path) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let outbox = Outbox::new();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(resp) = handle_request(session_id, sock, &req, &outbox) {
            outbox.send(&resp);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(session_id: &str, sock: &Path, req: &Value) -> Option<Value> {
        handle_request(session_id, sock, req, &Outbox::new())
    }

    #[test]
    fn tool_schema_names_the_tool_and_its_required_args() {
        let schema = tool_schema();
        assert_eq!(schema["name"], TOOL_NAME);
        assert_eq!(schema["inputSchema"]["required"], json!(["prompt", "model"]));
        let model_doc =
            schema["inputSchema"]["properties"]["model"]["description"].as_str().unwrap();
        assert!(model_doc.contains("REQUIRED"), "{model_doc}");
        assert!(model_doc.contains("claude-opus-5[1m]"), "{model_doc}");
        assert!(model_doc.contains("no account default"), "{model_doc}");
        let props = schema["inputSchema"]["properties"].as_object().unwrap();
        for key in [
            "adapter",
            "prompt",
            "session_id",
            "model",
            "agent_profile",
            "permission_mode",
            "name",
            "budget_usd",
            "cwd",
            "timeout_secs",
        ] {
            assert!(props.contains_key(key), "{key} missing from the schema");
        }
        assert_eq!(props["budget_usd"]["type"], "number");
        assert_eq!(schema["inputSchema"]["additionalProperties"], json!(false));
    }

    #[test]
    fn tool_schema_round_trips_as_json() {
        let raw = serde_json::to_string(&tool_schema()).unwrap();
        let back: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, tool_schema());
    }

    #[test]
    fn initialize_advertises_tools() {
        let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
        let resp = handle("s1", Path::new("/tmp/x.sock"), &req).unwrap();
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_returns_the_agent_and_usage_tools() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let resp = handle("s1", Path::new("/tmp/x.sock"), &req).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec![TOOL_NAME, USAGE_TOOL_NAME]);
    }

    #[test]
    fn the_usage_tool_needs_no_arguments_and_takes_only_an_optional_model() {
        let schema = usage_tool_schema();
        assert_eq!(schema["name"], USAGE_TOOL_NAME);
        assert_eq!(schema["inputSchema"]["required"], json!([]));
        assert_eq!(schema["inputSchema"]["additionalProperties"], json!(false));
        let props = schema["inputSchema"]["properties"].as_object().unwrap();
        assert_eq!(props.keys().collect::<Vec<_>>(), vec!["model"]);
        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains("THIS session"), "{desc}");
        assert!(desc.contains("blocked"), "{desc}");
    }

    #[test]
    fn a_usage_call_reaches_the_daemon_as_a_usage_kind_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["kind"], json!("usage"));
            assert_eq!(req["session_id"], json!("s1"));
            assert_eq!(req["args"]["model"], json!("claude-opus-5"));
            writeln!(stream, "{}", json!({ "ok": true, "result": "5h 46% · weekly 71%" })).unwrap();
        });
        let (text, is_error) = call_daemon(
            "s1",
            &sock_path,
            "usage",
            &json!({ "model": "claude-opus-5" }),
            None,
            &Outbox::new(),
        );
        server.join().unwrap();
        assert!(!is_error);
        assert_eq!(text, "5h 46% · weekly 71%");
    }

    #[test]
    fn a_dead_socket_fails_the_usage_call_by_name_instead_of_hanging() {
        let (text, is_error) = call_daemon(
            "s1",
            Path::new("/nonexistent/cctui-agent.sock"),
            "usage",
            &json!({}),
            None,
            &Outbox::new(),
        );
        assert!(is_error);
        assert!(text.starts_with(USAGE_TOOL_NAME), "{text}");
        assert!(text.contains("cannot reach the cctui daemon"), "{text}");
    }

    #[test]
    fn notifications_get_no_reply() {
        let req = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle("s1", Path::new("/tmp/x.sock"), &req).is_none());
    }

    #[test]
    fn unknown_tool_is_an_error_result_not_a_hang() {
        let req = json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "SomethingElse", "arguments": {} },
        });
        let resp = handle("s1", Path::new("/tmp/x.sock"), &req).unwrap();
        assert_eq!(resp["result"]["isError"], json!(true));
    }

    #[test]
    fn a_dead_daemon_socket_returns_an_error_result() {
        let (text, is_error) = call_daemon(
            "s1",
            Path::new("/nonexistent/cctui-agent.sock"),
            "spawn_agent",
            &json!({ "adapter": "opencode", "prompt": "hi", "timeout_secs": 1 }),
            None,
            &Outbox::new(),
        );
        assert!(is_error);
        assert!(text.contains("cannot reach the cctui daemon"));
    }

    #[test]
    fn progress_frames_forward_as_notifications_and_final_line_ends_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["proto"], json!(SOCKET_PROTO));
            writeln!(stream, "{}", json!({ "progress": "child working · tool: Bash" })).unwrap();
            writeln!(stream, "{}", json!({ "ok": true, "result": "verdict: ship" })).unwrap();
        });
        let (text, is_error) = call_daemon(
            "s1",
            &sock_path,
            "spawn_agent",
            &json!({ "adapter": "codex", "prompt": "go", "timeout_secs": 5 }),
            Some(&json!("tok-1")),
            &Outbox::new(),
        );
        server.join().unwrap();
        assert!(!is_error);
        assert_eq!(text, "verdict: ship");
    }

    #[test]
    fn concurrent_tool_calls_reply_out_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        let server = std::thread::spawn(move || {
            let mut streams = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                streams.push((stream, req["args"]["prompt"].as_str().unwrap().to_owned()));
            }
            // Answer in reverse arrival order: the second call must not be
            // blocked behind the first.
            streams.reverse();
            for (mut stream, prompt) in streams {
                writeln!(stream, "{}", json!({ "ok": true, "result": format!("done: {prompt}") }))
                    .unwrap();
            }
        });
        let sock_a = sock_path.clone();
        let a = std::thread::spawn(move || {
            call_daemon(
                "s1",
                &sock_a,
                "spawn_agent",
                &json!({ "adapter": "codex", "prompt": "first", "timeout_secs": 5 }),
                None,
                &Outbox::new(),
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        let b = std::thread::spawn(move || {
            call_daemon(
                "s1",
                &sock_path,
                "spawn_agent",
                &json!({ "adapter": "codex", "prompt": "second", "timeout_secs": 5 }),
                None,
                &Outbox::new(),
            )
        });
        let (text_a, err_a) = a.join().unwrap();
        let (text_b, err_b) = b.join().unwrap();
        server.join().unwrap();
        assert!(!err_a && !err_b);
        assert_eq!(text_a, "done: first");
        assert_eq!(text_b, "done: second");
    }

    #[test]
    fn progress_token_is_read_from_meta() {
        let req = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {
                "name": TOOL_NAME,
                "_meta": { "progressToken": 7 },
                "arguments": {},
            },
        });
        assert_eq!(progress_token(&req), Some(json!(7)));
        assert!(progress_token(&json!({ "params": {} })).is_none());
    }

    #[test]
    fn timeout_is_clamped_to_the_supported_range() {
        assert_eq!(resolve_timeout(None).as_secs(), DEFAULT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout(Some(60)).as_secs(), 60);
        assert_eq!(resolve_timeout(Some(0)).as_secs(), 1);
        assert_eq!(resolve_timeout(Some(999_999)).as_secs(), MAX_TIMEOUT_SECS);
    }

    #[test]
    fn mcp_config_bakes_the_session_and_socket_into_argv() {
        let cfg = mcp_config("/usr/bin/cctui-daemon", "sess-1", Path::new("/run/cctui/agent.sock"));
        let server = &cfg["mcpServers"]["cctui"];
        assert_eq!(server["command"], "/usr/bin/cctui-daemon");
        assert_eq!(server["type"], "stdio");
        assert_eq!(
            server["args"],
            json!(["mcp-agent", "--session", "sess-1", "--sock", "/run/cctui/agent.sock"])
        );
    }

    /// A stub daemon: each accepted connection is handed to `serve`, which
    /// reads the one request line and writes whatever the scenario dictates.
    /// Returns the socket path; the listener thread stops when `rounds` are
    /// served.
    fn stub_daemon(
        dir: &std::path::Path,
        rounds: usize,
        mut serve: impl FnMut(usize, &Value, &mut dyn Write) -> bool + Send + 'static,
    ) -> std::path::PathBuf {
        use std::os::unix::net::UnixListener;
        let sock = dir.join("agent.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            for round in 0..rounds {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    return;
                }
                let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                let mut out = &stream;
                if !serve(round, &req, &mut out) {
                    // Close without a result — exactly what an auto-update
                    // `execve` does to every open connection.
                    drop(stream);
                }
            }
        });
        sock
    }

    /// claudo/inbox#210, fresh symptom: the daemon auto-updates and re-execs,
    /// every agent-tool connection dies at once, and each parent's `CctuiAgent`
    /// call used to return "the daemon closed the connection without a
    /// result" — a hard failure for a child that never stopped running. The
    /// relay must reattach to the same child instead, and must not re-prompt
    /// it.
    #[test]
    fn a_daemon_restart_mid_call_reattaches_to_the_child_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let recorder = Arc::clone(&seen);
        let sock = stub_daemon(dir.path(), 2, move |round, req, out| {
            recorder.lock().unwrap().push(req.clone());
            if round == 0 {
                // Name the child, then die mid-call.
                writeln!(out, "{}", json!({ "attached": "child-42" })).unwrap();
                out.flush().unwrap();
                return false;
            }
            writeln!(out, "{}", json!({ "ok": true, "result": "verdict: ship" })).unwrap();
            out.flush().unwrap();
            true
        });

        let (text, is_error) = call_daemon(
            "parent-1",
            &sock,
            "spawn_agent",
            &json!({ "prompt": "review", "model": "claude-opus-5" }),
            None,
            &Outbox::new(),
        );
        assert!(!is_error, "a daemon restart must not fail the call: {text}");
        assert_eq!(text, "verdict: ship");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "the relay must come back for a second leg");
        assert_eq!(seen[0]["kind"], "spawn_agent");
        assert_eq!(seen[1]["kind"], "follow_agent", "the second leg reattaches, never respawns");
        assert_eq!(seen[1]["args"]["session_id"], "child-42");
        assert!(
            seen[1]["args"].get("prompt").is_none(),
            "a reattach must send the child nothing: {}",
            seen[1],
        );
    }

    /// Without a child id there is nothing to reattach to, so the call still
    /// fails — but it must say the daemon restarted, not blame the child.
    #[test]
    fn a_restart_before_the_child_is_named_fails_with_an_honest_message() {
        let dir = tempfile::tempdir().unwrap();
        let sock = stub_daemon(dir.path(), 1, |_round, _req, _out| false);
        let (text, is_error) = call_daemon(
            "parent-1",
            &sock,
            "spawn_agent",
            &json!({ "prompt": "review", "model": "claude-opus-5" }),
            None,
            &Outbox::new(),
        );
        assert!(is_error);
        assert!(text.contains("restarted"), "{text}");
        assert!(text.contains("NOT THE CHILD CRASHING"), "{text}");
    }

    /// A relay that cannot tell a restart from a plain result would reattach
    /// forever. A final frame ends the call on the first leg.
    #[test]
    fn a_result_on_the_first_leg_ends_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let sock = stub_daemon(dir.path(), 1, |_round, _req, out| {
            writeln!(out, "{}", json!({ "ok": false, "error": "child agent failed" })).unwrap();
            out.flush().unwrap();
            true
        });
        let (text, is_error) = call_daemon(
            "parent-1",
            &sock,
            "spawn_agent",
            &json!({ "prompt": "x" }),
            None,
            &Outbox::new(),
        );
        assert!(is_error);
        assert_eq!(text, "child agent failed");
    }
}
