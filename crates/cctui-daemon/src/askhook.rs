//! `cctui-daemon ask-hook` — the Claude Code hook command wired up by the
//! managed settings file the daemon injects into every fleet-spawned session.
//!
//! `AskUserQuestion` is the one interactive prompt the agents control socket
//! cannot surface live: the socket only reports coarse status (`state`/
//! `detail`), reporting `state:"done"` while a question is pending, and the
//! transcript flushes the `tool_use` block only *after* the turn advances. A
//! `PreToolUse` hook on `AskUserQuestion`, by contrast, fires the instant
//! before the form renders, with the full question payload on stdin.
//!
//! This subcommand reads that hook payload, formats the question, and forwards
//! it to the long-lived daemon over its local Unix socket. It is observe-only:
//! it prints nothing on stdout and always exits 0, so it never blocks or
//! alters the form Claude Code shows the user.

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::dangerous_rm::dangerous_removal;

/// Run the hook for `event` (`"pre"` = question appeared, `"post"` = answered).
///
/// Always returns `Ok(())` — a delivery failure must not break the user's
/// prompt — but logs to stderr so failures are still diagnosable.
///
/// `deny` (whip mode): after forwarding the question to the daemon
/// for visibility, emit a `PreToolUse` `deny` decision on stdout so the form
/// never renders and the model is told to decide and keep working instead of
/// asking. Only meaningful for the `pre` event.
pub fn run(event: &str, sock: &Path, deny: bool) -> anyhow::Result<()> {
    let mut input = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("cctui-daemon ask-hook: failed to read stdin: {err}");
        return Ok(());
    }
    let payload: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("cctui-daemon ask-hook: stdin was not JSON: {err}");
            return Ok(());
        }
    };

    let Some(session_id) = payload.get("session_id").and_then(Value::as_str) else {
        eprintln!("cctui-daemon ask-hook: payload missing session_id");
        return Ok(());
    };

    // Bidirectional tool-permission hook. For the `perm` event the
    // hook BLOCKS: it forwards the pending tool call to the daemon and waits on
    // the same connection for the human's decision, then prints the resulting
    // `PreToolUse` `permissionDecision` so Claude Code allows/denies the tool
    // without an attach + keystroke. On any failure (daemon down, timeout,
    // deferred) it prints nothing and exits 0, so the normal permission prompt
    // renders and the keystroke fallback path can answer it.
    if event == "perm" {
        run_perm(sock, session_id, &payload);
        return Ok(());
    }

    let tool_name = payload.get("tool_name").and_then(Value::as_str).unwrap_or_default();

    if tool_name == "EnterPlanMode" {
        if let Some(decision) = enter_plan_mode_decision(&payload, deny) {
            println!("{decision}");
        }
        return Ok(());
    }

    let line = if event == "post" {
        // One PostToolUse hook fires for both AskUserQuestion and ExitPlanMode;
        // tell the daemon which kind resolved so it drops the right live card.
        if tool_name == "ExitPlanMode" {
            json!({ "kind": "plan_resolved", "session_id": session_id })
        } else {
            json!({ "kind": "resolved", "session_id": session_id })
        }
    } else if tool_name == "ExitPlanMode" {
        // Plan-approval prompt. `tool_input.plan` carries the plan
        // markdown; fall back to the most recent on-disk plan file if it's
        // absent/truncated. Reuse `read_preamble` for the prose that preceded
        // the `ExitPlanMode` call in the same turn.
        let tool_input = payload.get("tool_input");
        let plan = tool_input
            .and_then(|t| t.get("plan"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .or_else(read_latest_plan_file)
            .unwrap_or_default();
        let preamble =
            payload.get("transcript_path").and_then(Value::as_str).and_then(read_preamble);
        json!({
            "kind": "plan",
            "session_id": session_id,
            "plan": plan,
            "preamble": preamble,
        })
    } else {
        let tool_input = payload.get("tool_input");
        let question = format_questions(tool_input);
        // Forward the raw `questions` array too (header/options/multiSelect) so
        // the webui can render the interactive form live, not just the
        // flattened `question` text. Falls back to text alone if the
        // payload shape is unexpected.
        let questions = tool_input.and_then(|t| t.get("questions")).cloned();
        // The assistant prose preceding the question lives in the transcript as
        // its own earlier `type:"assistant"` text line (Claude writes one line
        // per content block); `transcript_path` is handed to us on stdin. Read
        // it so the live question card shows its context instead of being
        // answered blind. Without it the preamble would arrive up to
        // ~2s later over the daemon's transcript poll as a detached bubble.
        let preamble =
            payload.get("transcript_path").and_then(Value::as_str).and_then(read_preamble);
        json!({
            "kind": "ask",
            "session_id": session_id,
            "question": question,
            "questions": questions,
            "preamble": preamble,
        })
    };

    if let Err(err) = send(sock, &line.to_string()) {
        // The daemon may simply be down; that's not fatal for the user.
        eprintln!("cctui-daemon ask-hook: could not reach daemon at {}: {err}", sock.display());
    }

    // Whip mode: deny the tool so the form never renders. We forward the
    // question above purely for UI visibility, then return a `deny` decision
    // that Claude Code surfaces back to the model as the tool result.
    if deny && event != "post" && tool_name != "ExitPlanMode" {
        let decision = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason":
                    "Whip mode (🐎): AskUserQuestion is disabled. Do not ask the user to \
                     choose, confirm, or review — decide using your best judgment and keep \
                     working. Only stop when the task is genuinely complete or you hit a \
                     concrete blocker you cannot resolve yourself.",
            },
        });
        println!("{decision}");
    }
    Ok(())
}

/// Handle the `perm` (`PreToolUse` permission) hook event.
///
/// `AskUserQuestion` is skipped here — it has its own `pre`/`post` hook and is
/// not a tool-permission prompt. For every other tool we mint a correlation id,
/// send a `perm-request` line to the daemon, and block reading the decision
/// back on the same connection. The daemon writes one of:
///   `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"|"deny"|"defer", …}}`
/// which we print verbatim so Claude Code applies it. A `defer`, an empty
/// reply, or any error → we print nothing (exit 0) and the normal permission
/// flow continues, where the keystroke fallback can still answer.
/// Whether the bidirectional permission hook should defer (return no decision,
/// letting Claude Code's normal flow proceed) instead of parking for a human
/// decision over the daemon socket.
///
/// Two cases defer:
/// - `AskUserQuestion` — answered via its own pre/post hook + the reply path,
///   never as a tool permission.
/// - `permission_mode == "bypassPermissions"` (yolo) — the user has explicitly
///   opted out of every prompt. Claude Code still fires `PreToolUse` hooks in this
///   mode, so without this guard the hook would park every tool for a human
///   decision and resurrect a permission prompt for every action, defeating
///   bypass mode. Only `default`/`acceptEdits`/
///   `plan` modes — where a prompt would otherwise render — use the hook.
fn perm_hook_defers(payload: &Value, tool: &str) -> bool {
    if tool == "AskUserQuestion" {
        return true;
    }
    payload.get("permission_mode").and_then(Value::as_str) == Some("bypassPermissions")
}

/// `EnterPlanMode` deny decision. Approving a plan switches the
/// session to whichever mode the approval selects and never restores
/// `bypassPermissions`; the default Bg control socket has no in-place
/// set-permission-mode op, so a yolo/whip worker that enters plan mode silently
/// and unrecoverably drops out of bypass and wedges on prompts. Returns a `deny`
/// only while the live payload `permission_mode` is `bypassPermissions`;
/// otherwise `None` (defer, print nothing) so plan mode works normally in every
/// prompting mode. `whip` only picks the posture label in the reason.
fn enter_plan_mode_decision(payload: &Value, whip: bool) -> Option<Value> {
    if payload.get("permission_mode").and_then(Value::as_str) != Some("bypassPermissions") {
        return None;
    }
    let posture = if whip { "Whip" } else { "Yolo" };
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!(
                "{posture} mode: plan mode is disabled because approving a plan would drop the \
                 session out of bypassPermissions with no way to restore it in place. Proceed \
                 and do the work directly."
            ),
        },
    }))
}

/// Refuse a `Bash` call in `bypassPermissions` whose `rm`/`rmdir` would trip
/// Claude Code's `dangerousRemoval` check. That check is bypass-immune, so it
/// renders a prompt even in yolo/whip, where nothing answers it and the worker
/// parks. Denying instead hands the agent a rewrite it can retry immediately.
/// `None` (defer) for every other command and every other mode.
fn bypass_rm_decision(payload: &Value, tool: &str) -> Option<Value> {
    if tool != "Bash" {
        return None;
    }
    if payload.get("permission_mode").and_then(Value::as_str) != Some("bypassPermissions") {
        return None;
    }
    let command =
        payload.get("tool_input").and_then(|t| t.get("command")).and_then(Value::as_str)?;
    let found = dangerous_removal(command, payload.get("cwd").and_then(Value::as_str))?;
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!(
                "Refused: this rm targets {} (`{}`), which trips Claude Code's bypass-immune \
                 dangerousRemoval check — it cannot be auto-approved and would park this session \
                 on a prompt nobody can answer.\nRewrite it with an explicit, literal target — \
                 e.g. `find \"$D\" -maxdepth 1 -name '*.json' -delete` or \
                 `rm -f -- \"${{D:?}}\"/*.json` — and retry.",
                found.risk.description(),
                found.target,
            ),
        },
    }))
}

fn run_perm(sock: &Path, session_id: &str, payload: &Value) {
    let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or_default();
    if let Some(decision) = bypass_rm_decision(payload, tool) {
        println!("{decision}");
        return;
    }
    // AskUserQuestion is answered via its own hook + the reply path, and in
    // `bypassPermissions` (yolo) mode no prompt should ever render — see
    // `perm_hook_defers`. Both cases defer immediately so we never park here.
    if perm_hook_defers(payload, tool) {
        return;
    }
    let hook_id = format!("{session_id}-{}", std::process::id());
    let request = json!({
        "kind": "perm-request",
        "session_id": session_id,
        "hook_id": hook_id,
        "tool": tool,
        "input": payload.get("tool_input").cloned().unwrap_or(Value::Null),
    });
    match request_decision(sock, &request.to_string()) {
        Ok(Some(decision)) => {
            // Only emit a concrete allow/deny; a `defer` (or anything else)
            // means "let the normal flow run", so print nothing.
            let kind = decision
                .get("hookSpecificOutput")
                .and_then(|o| o.get("permissionDecision"))
                .and_then(Value::as_str)
                .unwrap_or("defer");
            if kind == "allow" || kind == "deny" {
                println!("{decision}");
            }
        }
        Ok(None) => {} // daemon deferred / closed without a decision
        Err(err) => {
            eprintln!("cctui-daemon ask-hook: perm decision unavailable: {err}");
        }
    }
}

/// Send `line` to the daemon and block reading a single newline-delimited JSON
/// decision back on the same connection. Returns `Ok(None)` if the
/// daemon closes without a decision or the reply isn't JSON. The read has no
/// explicit timeout: the daemon bounds its own wait and always writes a
/// decision (`defer` on its timeout) before the hook's configured `timeout`
/// ceiling, so this can't hang the turn indefinitely.
fn request_decision(sock: &Path, line: &str) -> std::io::Result<Option<Value>> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf)?;
    let buf = buf.trim();
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str(buf).ok())
}

/// Fallback for a missing/truncated `tool_input.plan`: read the most
/// recently modified plan markdown Claude Code wrote under `~/.claude/plans`.
/// The plan-file naming is slug-based and not exposed in the hook payload, so
/// rather than guessing a slug we take the newest `*.md` — in practice the plan
/// being presented is the one just written. `None` if the directory is absent
/// or empty.
fn read_latest_plan_file() -> Option<String> {
    let dir = dirs::home_dir()?.join(".claude").join("plans");
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, path)| path)?;
    let body = std::fs::read_to_string(newest).ok()?;
    if body.trim().is_empty() { None } else { Some(body) }
}

/// Read the assistant prose preceding the pending question from the transcript
/// at `path`.
///
/// Claude Code's transcript writer drains to disk on a 100ms timer
/// (`FLUSH_INTERVAL_MS`) independent of the turn loop, and writes one line per
/// content block — so the preamble `text` line lands ~100ms after it streams,
/// ~1s before the `tool_use` line that triggers this `PreToolUse` hook. We pause
/// briefly first to clear that sub-flush window (the hook can fire just before
/// the text line is drained), then scan the file. `None` on any read error or
/// when the model called the tool with no preceding text.
fn read_preamble(path: &str) -> Option<String> {
    // Let the preamble text line drain (FLUSH_INTERVAL_MS = 100). Cheap
    // insurance against the rare case where the hook beats the flush; the
    // delay is invisible next to the question card itself.
    std::thread::sleep(Duration::from_millis(120));
    let body = std::fs::read_to_string(path).ok()?;
    scan_preamble(&body)
}

/// Scan transcript `body` (newline-delimited JSON) backward for the assistant
/// prose of the current turn: the most recent `type:"assistant"` line carrying
/// a `{type:"text"}` block. Thinking-only and `tool_use`-only assistant lines
/// (the `AskUserQuestion` call itself) are skipped; a `type:"user"` line is a
/// turn boundary, so we stop there rather than grab text from an earlier turn.
fn scan_preamble(body: &str) -> Option<String> {
    for line in body.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            // A user / tool_result line is the turn boundary — if we reach it
            // before any assistant text, the tool was called with no preamble.
            Some("user") => return None,
            Some("assistant") => {
                let text = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    return Some(text);
                }
                // thinking-only / tool_use-only line: same turn, keep scanning.
            }
            _ => {} // summaries, attachments, etc. — skip, not a boundary.
        }
    }
    None
}

/// Render `tool_input.questions` into a readable plain-text prompt: each
/// question's header + text, then its options as bullet lines. This is the
/// show-only carrier for the existing `AskQuestion` event; a structured,
/// answer-from-webui form is a follow-up.
fn format_questions(tool_input: Option<&Value>) -> String {
    let Some(questions) = tool_input.and_then(|t| t.get("questions")).and_then(Value::as_array)
    else {
        return String::new();
    };
    let mut out = String::new();
    for q in questions {
        let header = q.get("header").and_then(Value::as_str).unwrap_or_default();
        let text = q.get("question").and_then(Value::as_str).unwrap_or_default();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        if header.is_empty() {
            out.push_str(text);
        } else {
            let _ = write!(out, "{header}: {text}");
        }
        if let Some(options) = q.get("options").and_then(Value::as_array) {
            for opt in options {
                let label = opt.get("label").and_then(Value::as_str).unwrap_or_default();
                if label.is_empty() {
                    continue;
                }
                let desc = opt.get("description").and_then(Value::as_str).unwrap_or_default();
                if desc.is_empty() {
                    let _ = write!(out, "\n  • {label}");
                } else {
                    let _ = write!(out, "\n  • {label} — {desc}");
                }
            }
        }
    }
    out
}

/// Connect to the daemon socket and write one newline-delimited JSON line.
/// A connect timeout keeps the hook from ever hanging the agent's turn.
fn send(sock: &Path, line: &str) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_question_with_options() {
        let ti = json!({
            "questions": [{
                "header": "Scope",
                "question": "How should we handle it?",
                "options": [
                    {"label": "Show-only", "description": "minimal"},
                    {"label": "Full form"}
                ]
            }]
        });
        let s = format_questions(Some(&ti));
        assert!(s.contains("Scope: How should we handle it?"));
        assert!(s.contains("• Show-only — minimal"));
        assert!(s.contains("• Full form"));
    }

    #[test]
    fn formats_multiple_questions() {
        let ti = json!({
            "questions": [
                {"question": "First?"},
                {"question": "Second?"}
            ]
        });
        let s = format_questions(Some(&ti));
        assert!(s.contains("First?"));
        assert!(s.contains("Second?"));
        assert!(s.contains("\n\n"));
    }

    #[test]
    fn missing_questions_yields_empty() {
        assert_eq!(format_questions(None), "");
        assert_eq!(format_questions(Some(&json!({}))), "");
    }

    #[test]
    fn scan_preamble_finds_text_before_tool_use() {
        // the AskUserQuestion tool_use lands on its own line, preceded
        // by the assistant's text line in the same turn. Scanning backward must
        // skip the tool_use line and return the preceding prose.
        let body = concat!(
            r#"{"type":"user","message":{"content":"go"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Here is my analysis."}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion","input":{}}]}}"#,
            "\n",
        );
        assert_eq!(scan_preamble(body).as_deref(), Some("Here is my analysis."));
    }

    #[test]
    fn scan_preamble_skips_thinking_only_lines() {
        let body = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"the recommendation"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion"}]}}"#,
            "\n",
        );
        assert_eq!(scan_preamble(body).as_deref(), Some("the recommendation"));
    }

    #[test]
    fn scan_preamble_stops_at_turn_boundary() {
        // The tool was called right after a tool_result/user line with no
        // preamble — must not reach back into the previous turn's text.
        let body = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old turn text"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"AskUserQuestion"}]}}"#,
            "\n",
        );
        assert_eq!(scan_preamble(body), None);
    }

    #[test]
    fn scan_preamble_empty_or_garbage_is_none() {
        assert_eq!(scan_preamble(""), None);
        assert_eq!(scan_preamble("not json\n{also not\n"), None);
    }

    #[test]
    fn perm_hook_defers_in_bypass_mode() {
        // yolo (bypassPermissions) must never park a tool for a human
        // decision — Claude still fires PreToolUse hooks but the user opted out
        // of every prompt.
        let bypass = json!({"permission_mode": "bypassPermissions"});
        assert!(perm_hook_defers(&bypass, "Bash"));
        assert!(perm_hook_defers(&bypass, "Edit"));
        assert!(perm_hook_defers(&bypass, "Task"));
    }

    #[test]
    fn perm_hook_parks_in_default_modes() {
        // default/acceptEdits/plan (and a missing field) still go through the
        // bidirectional hook so the prompt surfaces in the webui.
        assert!(!perm_hook_defers(&json!({"permission_mode": "default"}), "Bash"));
        assert!(!perm_hook_defers(&json!({"permission_mode": "acceptEdits"}), "Bash"));
        assert!(!perm_hook_defers(&json!({"permission_mode": "plan"}), "Bash"));
        assert!(!perm_hook_defers(&json!({}), "Bash"));
    }

    #[test]
    fn perm_hook_always_defers_ask_user_question() {
        // AskUserQuestion is never a tool permission, in any mode.
        assert!(perm_hook_defers(&json!({"permission_mode": "default"}), "AskUserQuestion"));
        assert!(perm_hook_defers(&json!({}), "AskUserQuestion"));
    }

    fn bash_payload(mode: &str, command: &str) -> Value {
        json!({
            "session_id": "c4f46329-163c-4faf-a72a-6e49168d1b88",
            "transcript_path": "/home/dorsk/.claude/projects/repo/c4f46329.jsonl",
            "cwd": "/home/dorsk/Documents/repo",
            "permission_mode": mode,
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command, "description": "clean cache"},
        })
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn bypass_denies_dangerous_rm_with_guidance() {
        let payload = bash_payload(
            "bypassPermissions",
            "D=~/.claude/artifacts/win; mkdir -p $D; rm -f $D/*.json",
        );
        let decision = bypass_rm_decision(&payload, "Bash").expect("deny");
        let out = &decision["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse");
        assert_eq!(out["permissionDecision"], "deny");
        let reason = out["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains("$D/*.json"), "{reason}");
        assert!(reason.contains("dangerousRemoval"), "{reason}");
        assert!(reason.contains("-name '*.json' -delete"), "{reason}");
        assert!(reason.contains("${D:?}"), "{reason}");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn bypass_defers_ordinary_commands() {
        let payload = bash_payload("bypassPermissions", "cargo test -p cctui-daemon");
        assert!(bypass_rm_decision(&payload, "Bash").is_none());
        assert!(perm_hook_defers(&payload, "Bash"));

        let safe = bash_payload("bypassPermissions", r#"rm -f -- "${D:?}"/*.json"#);
        assert!(bypass_rm_decision(&safe, "Bash").is_none());
        assert!(perm_hook_defers(&safe, "Bash"));
    }

    #[test]
    fn dangerous_rm_outside_bypass_is_unchanged() {
        for mode in ["default", "acceptEdits", "plan"] {
            let payload = bash_payload(mode, "rm -rf $D");
            assert!(bypass_rm_decision(&payload, "Bash").is_none(), "{mode}");
            assert!(!perm_hook_defers(&payload, "Bash"), "{mode}");
        }
    }

    #[test]
    fn non_bash_tools_are_never_rm_denied() {
        let mut payload = bash_payload("bypassPermissions", "rm -rf $D");
        payload["tool_name"] = json!("Edit");
        assert!(bypass_rm_decision(&payload, "Edit").is_none());
    }

    #[test]
    fn enter_plan_mode_denies_only_in_bypass() {
        let bypass = json!({"tool_name": "EnterPlanMode", "permission_mode": "bypassPermissions"});
        let decision = enter_plan_mode_decision(&bypass, false).expect("deny in bypass");
        let out = &decision["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse");
        assert_eq!(out["permissionDecision"], "deny");
        assert!(
            out["permissionDecisionReason"].as_str().unwrap().starts_with("Yolo mode:"),
            "yolo posture label"
        );

        let whip = enter_plan_mode_decision(&bypass, true).expect("deny in bypass");
        assert!(
            whip["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .starts_with("Whip mode:"),
            "whip posture label"
        );
    }

    #[test]
    fn enter_plan_mode_defers_in_prompting_modes() {
        for mode in ["default", "acceptEdits", "plan"] {
            let payload = json!({"tool_name": "EnterPlanMode", "permission_mode": mode});
            assert!(enter_plan_mode_decision(&payload, false).is_none(), "defer in {mode}");
            assert!(enter_plan_mode_decision(&payload, true).is_none(), "defer in {mode} (whip)");
        }
        assert!(enter_plan_mode_decision(&json!({"tool_name": "EnterPlanMode"}), true).is_none());
    }
}
