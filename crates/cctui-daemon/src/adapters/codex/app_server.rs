//! Codex `app-server` driver.
//!
//! Drives sessions that cctui *spawns*, as opposed to the log-tail
//! ([`super::log_tail`]) which passively observes sessions started outside
//! cctui (e.g. the Codex TUI). The two coexist: session identity is the
//! rollout id (`UUIDv7`), so an app-server-driven session and its on-disk
//! rollout file refer to the same `local_id`.
//!
//! `codex app-server` speaks newline-delimited JSON-RPC 2.0 over stdio
//! (stderr is logs). The handshake is `initialize` (declaring client
//! capabilities) → `initialized` notification → `thread/start { cwd }`
//! → `turn/start { threadId, input }`. The minimum supported Codex
//! version and the retained JSON Schema live in [`super::contract`]. A stale
//! cctui-owned thread is revived
//! with `thread/resume { threadId }` before the next `turn/start`.
//! Streaming arrives as id-less
//! notifications (`item/completed`, `turn/completed`, …); tool approvals
//! arrive as server→client *requests* (they carry both `method` and `id`)
//! that block until we reply with a `decision`.
//!
//! This module is split into a pure protocol layer (request builders +
//! [`classify`]) that is unit-tested with fixtures, and an async driver
//! ([`CodexSession`]) that owns the subprocess and pumps IO.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cctui_crypto::redact::{self, CompiledPatterns};
use cctui_proto::adapter::{AdapterEvent, EndReason, SessionMeta};
use cctui_proto::codex_catalog::{CodexModel, CodexModelCatalog};
use cctui_proto::diagnose::{CodexProtocolError, CodexRpcFrame, CodexStderrLine};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{contract, model_list};

/// Outbound request id seeds. The handshake uses fixed ids so the driver
/// can recognise the responses it is waiting for; everything after is
/// monotonic from [`Self::RUN_BASE`].
const ID_INITIALIZE: i64 = 1;
const ID_THREAD_START: i64 = 2;
const RUN_BASE: i64 = 100;

/// How many trailing `codex app-server` stderr lines to retain for crash
/// diagnostics. The app-server logs to stderr; when it dies
/// unexpectedly these lines are the only clue why, so they are folded into
/// the [`EndReason::Crashed`] detail instead of being discarded to
/// `/dev/null`.
const STDERR_RING: usize = 200;

const RPC_RING: usize = 50;

const PROTOCOL_ERROR_RING: usize = 20;

const RPC_FRAME_MAX: usize = 2 * 1024;

/// Prefix of a frame actually scanned for secrets. Larger than [`RPC_FRAME_MAX`]
/// so a token straddling the retention cut is still masked in what is kept.
const RPC_SCAN_MAX: usize = 8 * 1024;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// `CommandResult` error for an `Interrupt` received while no turn is active.
pub const NO_TURN_IN_FLIGHT: &str = "no turn in flight";

/// Budget for the whole handshake (`initialize` → model check →
/// `thread/start|resume|fork`), measured from process launch. A resume of a
/// long transcript needs more than one RPC deadline; the spawn caller
/// (webui `awaitCommand`) waits this long plus a margin, so a hung handshake
/// must fail here first.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(45);

// ---------------------------------------------------------------------------
// Pure protocol layer
// ---------------------------------------------------------------------------

/// Which `decision` vocabulary an approval reply must use. Codex uses two
/// distinct enums depending on the approval method (verified against the
/// app-server JSON schema, codex-cli 0.134):
///
/// - command-execution and file-change approvals →
///   `"accept"` / `"decline"`.
/// - apply-patch and exec-command approvals (legacy `ReviewDecision`) →
///   `"approved"` / `"denied"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    AcceptDecline,
    ApprovedDenied,
}

impl ApprovalKind {
    const fn decision(self, allow: bool) -> &'static str {
        match (self, allow) {
            (Self::AcceptDecline, true) => "accept",
            (Self::AcceptDecline, false) => "decline",
            (Self::ApprovedDenied, true) => "approved",
            (Self::ApprovedDenied, false) => "denied",
        }
    }
}

/// Classification of a single inbound JSON-RPC object.
#[derive(Debug)]
pub enum Incoming {
    /// Reply to one of our requests: has `id`, no `method`.
    Response { id: i64, value: Value },
    /// Server→client request that blocks on a decision (tool/patch
    /// approval): carries both `method` and `id`. `rpc_id` is echoed back
    /// verbatim in the reply; `request_id` is the stable id surfaced to the
    /// TUI via [`AdapterEvent::PermissionRequest`].
    Approval { rpc_id: Value, request_id: String, tool: String, kind: ApprovalKind, input: Value },
    /// `item/tool/requestUserInput`: codex's `AskUserQuestion`.
    /// `question_ids` are needed to key the [`ToolRequestUserInputResponse`].
    Question { rpc_id: Value, question: String, questions: Value, question_ids: Vec<String> },
    /// A server→client request cctui cannot fulfil. It carries an `id`, so
    /// leaving it unanswered blocks codex forever; `reply` is the decline/error
    /// to write back immediately instead.
    Decline { reply: Value },
    /// A notification we mapped onto an adapter event.
    Event(AdapterEvent),
    /// A schema-known notification with no user signal of its own. `reason`
    /// records why nothing is emitted; the frame stays in the diagnose ring.
    Traced { method: String, reason: &'static str },
    /// A method absent from the pinned protocol schema — a newer codex added
    /// it. Traced loudly so a protocol addition surfaces as a gap, and still
    /// carried into the timeline.
    Unhandled { method: String, event: AdapterEvent },
    /// A frame that is neither request, response nor notification.
    Ignored,
}

/// Classify one parsed JSON-RPC object. `local_id` is the thread/session id
/// (only meaningful once the handshake has completed; during the handshake
/// only [`Incoming::Response`] values are acted upon).
#[must_use]
pub fn classify(local_id: &str, v: &Value) -> Incoming {
    let has_id = v.get("id").is_some();
    let method = v.get("method").and_then(Value::as_str);
    match (method, has_id) {
        (Some(m), true) => classify_server_request(m, v),
        (Some(m), false) => map_notification(local_id, m, v),
        (None, true) => {
            let id = v.get("id").and_then(Value::as_i64).unwrap_or(-1);
            Incoming::Response { id, value: v.clone() }
        }
        (None, false) => Incoming::Ignored,
    }
}

fn classify_server_request(method: &str, v: &Value) -> Incoming {
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    let rpc_id = v.get("id").cloned().unwrap_or(Value::Null);
    let (kind, tool) = match method {
        "item/commandExecution/requestApproval" => (ApprovalKind::AcceptDecline, "shell"),
        "item/fileChange/requestApproval" => (ApprovalKind::AcceptDecline, "file_change"),
        "applyPatchApproval" => (ApprovalKind::ApprovedDenied, "apply_patch"),
        "execCommandApproval" => (ApprovalKind::ApprovedDenied, "shell"),
        "item/tool/requestUserInput" => return classify_user_input(rpc_id, &params),
        // Known-but-unsupported requests get their schema-correct decline reply
        // so codex isn't blocked forever; everything else (dynamic
        // tool call, token refresh, attestation, future methods) gets a generic
        // method-not-supported error.
        "mcpServer/elicitation/request" => {
            return Incoming::Decline { reply: elicitation_decline(&rpc_id) };
        }
        "item/permissions/requestApproval" => {
            return Incoming::Decline { reply: permissions_decline(&rpc_id) };
        }
        _ => return Incoming::Decline { reply: request_not_supported(&rpc_id, method) },
    };
    let request_id = params
        .get("itemId")
        .and_then(Value::as_str)
        .map_or_else(|| format!("codex-approval-{rpc_id}"), std::string::ToString::to_string);
    Incoming::Approval { rpc_id, request_id, tool: tool.to_string(), kind, input: params }
}

/// Classify an `item/tool/requestUserInput` request into a [`Incoming::Question`].
/// Flattens the per-question `header`/`question` into a single text
/// (for the flattened claude field) while passing the raw `questions` array
/// through for the interactive card, and collects the question ids the answer
/// must be keyed on.
fn classify_user_input(rpc_id: Value, params: &Value) -> Incoming {
    let questions = params.get("questions").cloned().unwrap_or_else(|| json!([]));
    let list = questions.as_array().cloned().unwrap_or_default();
    let question_ids: Vec<String> = list
        .iter()
        .filter_map(|q| q.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let question = list
        .iter()
        .filter_map(|q| {
            let text = q.get("question").and_then(Value::as_str)?;
            Some(
                q.get("header")
                    .and_then(Value::as_str)
                    .filter(|h| !h.is_empty())
                    .map_or_else(|| text.to_owned(), |header| format!("{header} — {text}")),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Incoming::Question { rpc_id, question, questions, question_ids }
}

/// Decline an MCP `elicitation/create` request. cctui does not render
/// the typed form, so it answers `decline` — the schema's neutral "user did not
/// provide input" action — rather than leaving the turn blocked.
fn elicitation_decline(rpc_id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"action": "decline"}})
}

/// Decline a sandbox-permission elevation request. Granting nothing
/// (an empty `GrantedPermissionProfile`) is the deny: codex continues the turn
/// without the extra permissions instead of waiting on a reply that never comes.
fn permissions_decline(rpc_id: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"permissions": {}}})
}

/// Reject a server request method cctui does not implement with a JSON-RPC
/// method-not-found error, so codex fails the request fast instead of blocking.
fn request_not_supported(rpc_id: &Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "error": {"code": -32601, "message": format!("cctui does not support server request {method}")},
    })
}

/// Reply to an `item/tool/requestUserInput` request. The single free
/// text answer is mapped onto every question id — requestUserInput forms are
/// single-question in practice, and codex feeds the string straight to the tool.
fn user_input_reply(rpc_id: &Value, question_ids: &[String], answer: &str) -> Value {
    let answers: serde_json::Map<String, Value> =
        question_ids.iter().map(|id| (id.clone(), json!({"answers": [answer]}))).collect();
    json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"answers": answers}})
}

/// What the adapter does with one `ServerNotification` method. Every method in
/// the pinned schema resolves to a non-[`Disposition::Unknown`] arm — enforced
/// against `schema/codex_app_server_protocol.schemas.json` by
/// `every_schema_notification_has_a_disposition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Mapped onto a dedicated adapter event.
    Mapped,
    /// Consumed elsewhere in the driver ([`ItemAccumulator`],
    /// [`turn_lifecycle`]) or a stream cctui does not render.
    Stream(&'static str),
    /// No dedicated home: recorded into the timeline as a generic notice with
    /// this severity.
    Notice(&'static str),
    /// Not in the pinned schema.
    Unknown,
}

/// Every `ServerNotification` method the pinned schema defines, paired with
/// what the adapter does with it. Kept in lockstep with
/// `schema/codex_app_server_protocol.schemas.json` in both directions by
/// `every_schema_notification_has_a_disposition` and
/// `disposition_table_has_no_methods_the_schema_lacks`.
const NOTIFICATION_DISPOSITIONS: &[(&str, Disposition)] = &[
    ("item/completed", Disposition::Mapped),
    ("thread/status/changed", Disposition::Mapped),
    ("thread/tokenUsage/updated", Disposition::Mapped),
    ("thread/name/updated", Disposition::Mapped),
    ("error", Disposition::Mapped),
    ("turn/completed", Disposition::Mapped),
    ("turn/plan/updated", Disposition::Mapped),
    ("thread/compacted", Disposition::Mapped),
    ("turn/started", Disposition::Stream("turn lifecycle")),
    ("item/started", Disposition::Stream("item accumulator")),
    ("item/agentMessage/delta", Disposition::Stream("delta coalesced into item/completed")),
    ("item/plan/delta", Disposition::Stream("delta coalesced into item/completed")),
    ("item/reasoning/textDelta", Disposition::Stream("delta coalesced into item/completed")),
    ("item/reasoning/summaryTextDelta", Disposition::Stream("delta coalesced into item/completed")),
    ("item/reasoning/summaryPartAdded", Disposition::Stream("delta coalesced into item/completed")),
    (
        "item/commandExecution/outputDelta",
        Disposition::Stream("delta coalesced into item/completed"),
    ),
    ("command/exec/outputDelta", Disposition::Stream("delta coalesced into item/completed")),
    ("item/fileChange/outputDelta", Disposition::Stream("delta coalesced into item/completed")),
    ("process/outputDelta", Disposition::Stream("delta coalesced into item/completed")),
    (
        "item/commandExecution/terminalInteraction",
        Disposition::Stream("superseded by the completed item"),
    ),
    ("item/fileChange/patchUpdated", Disposition::Stream("superseded by the completed item")),
    ("item/mcpToolCall/progress", Disposition::Stream("superseded by the completed item")),
    ("turn/diff/updated", Disposition::Stream("superseded by the completed item")),
    ("serverRequest/resolved", Disposition::Stream("approval bookkeeping")),
    (
        "fuzzyFileSearch/sessionUpdated",
        Disposition::Stream("file-picker session cctui does not drive"),
    ),
    (
        "fuzzyFileSearch/sessionCompleted",
        Disposition::Stream("file-picker session cctui does not drive"),
    ),
    ("thread/realtime/started", Disposition::Stream("realtime voice session")),
    ("thread/realtime/itemAdded", Disposition::Stream("realtime voice session")),
    ("thread/realtime/item/started", Disposition::Stream("realtime voice session")),
    ("thread/realtime/item/transcript/delta", Disposition::Stream("realtime voice session")),
    ("thread/realtime/item/completed", Disposition::Stream("realtime voice session")),
    ("thread/realtime/transcript/delta", Disposition::Stream("realtime voice session")),
    ("thread/realtime/transcript/done", Disposition::Stream("realtime voice session")),
    ("thread/realtime/outputAudio/delta", Disposition::Stream("realtime voice session")),
    ("thread/realtime/sdp", Disposition::Stream("realtime voice session")),
    ("thread/realtime/error", Disposition::Stream("realtime voice session")),
    ("thread/realtime/closed", Disposition::Stream("realtime voice session")),
    ("fs/changed", Disposition::Stream("high-volume inventory churn")),
    ("app/list/updated", Disposition::Stream("high-volume inventory churn")),
    ("skills/changed", Disposition::Stream("high-volume inventory churn")),
    ("thread/queue/changed", Disposition::Stream("high-volume inventory churn")),
    ("mcpServer/event/stream/notification", Disposition::Stream("opaque MCP passthrough")),
    ("warning", Disposition::Notice("warning")),
    ("guardianWarning", Disposition::Notice("warning")),
    ("configWarning", Disposition::Notice("warning")),
    ("deprecationNotice", Disposition::Notice("warning")),
    ("windows/worldWritableWarning", Disposition::Notice("warning")),
    ("autoApprovalReview/strictReviewRequired", Disposition::Notice("warning")),
    ("turn/moderationMetadata", Disposition::Notice("warning")),
    ("model/rerouted", Disposition::Notice("info")),
    ("model/verification", Disposition::Notice("info")),
    ("model/safetyBuffering/updated", Disposition::Notice("info")),
    ("modelProvider/authRecoveryStarted", Disposition::Notice("info")),
    ("modelProvider/authRecoveryCompleted", Disposition::Notice("info")),
    ("mcpServer/startupStatus/updated", Disposition::Notice("info")),
    ("mcpServer/oauthLogin/completed", Disposition::Notice("info")),
    ("process/exited", Disposition::Notice("info")),
    ("thread/environment/connected", Disposition::Notice("info")),
    ("thread/environment/disconnected", Disposition::Notice("info")),
    ("thread/settings/updated", Disposition::Notice("info")),
    ("thread/goal/updated", Disposition::Notice("info")),
    ("thread/goal/cleared", Disposition::Notice("info")),
    ("thread/project/updated", Disposition::Notice("info")),
    ("project/changed", Disposition::Notice("info")),
    ("thread/started", Disposition::Notice("info")),
    ("thread/archived", Disposition::Notice("info")),
    ("thread/unarchived", Disposition::Notice("info")),
    ("thread/deleted", Disposition::Notice("info")),
    ("thread/closed", Disposition::Notice("info")),
    ("thread/reverted", Disposition::Notice("info")),
    ("hook/started", Disposition::Notice("info")),
    ("hook/completed", Disposition::Notice("info")),
    ("item/autoApprovalReview/started", Disposition::Notice("info")),
    ("item/autoApprovalReview/completed", Disposition::Notice("info")),
    ("account/updated", Disposition::Notice("info")),
    ("account/rateLimits/updated", Disposition::Notice("info")),
    ("account/login/completed", Disposition::Notice("info")),
    ("remoteControl/status/changed", Disposition::Notice("info")),
    ("externalAgentConfig/import/progress", Disposition::Notice("info")),
    ("externalAgentConfig/import/completed", Disposition::Notice("info")),
    ("windowsSandbox/setupCompleted", Disposition::Notice("info")),
];

/// Resolve a `ServerNotification` method to its [`Disposition`].
#[must_use]
pub fn disposition(method: &str) -> Disposition {
    NOTIFICATION_DISPOSITIONS
        .iter()
        .find(|(m, _)| *m == method)
        .map_or(Disposition::Unknown, |(_, d)| *d)
}

fn map_notification(local_id: &str, method: &str, v: &Value) -> Incoming {
    match disposition(method) {
        Disposition::Mapped => match method {
            // Emit on `item/completed` only; `item/started` and `item/<kind>/delta`
            // are consumed by [`ItemAccumulator`] in the driver, not here.
            "item/completed" => map_item_completed(local_id, v),
            // Thread liveness/attention → Status (drives the dots + ✋).
            "thread/status/changed" => map_status(local_id, v),
            // Per-turn token usage → TokenUsage.
            "thread/tokenUsage/updated" => map_token_usage(local_id, v),
            // Thread rename → Status carrying just the name (display gated on).
            "thread/name/updated" => map_name(local_id, v),
            // Structured turn errors → failed Status.
            "error" => map_error_notification(local_id, v),
            "turn/completed" => map_turn_completed(local_id, v),
            "turn/plan/updated" => map_plan_updated(local_id, v),
            "thread/compacted" => map_compacted(local_id, v),
            _ => unreachable!("Disposition::Mapped without a mapper: {method}"),
        },
        Disposition::Stream(reason) => Incoming::Traced { method: method.to_owned(), reason },
        Disposition::Notice(level) => {
            Incoming::Event(notice_event(local_id, method, level, v.get("params")))
        }
        Disposition::Unknown => Incoming::Unhandled {
            method: method.to_owned(),
            event: notice_event(local_id, method, "unhandled", v.get("params")),
        },
    }
}

/// A notification with no dedicated rendering, carried into the timeline as a
/// `codexNotice` item. `cctui-server`'s codex normalizer turns it into a
/// notice line; without an entry there it would be dropped as an unknown item.
fn notice_event(local_id: &str, method: &str, level: &str, params: Option<&Value>) -> AdapterEvent {
    AdapterEvent::Message {
        local_id: local_id.to_owned(),
        payload: json!({
            "type": "codexNotice",
            "level": level,
            "method": method,
            "text": notice_text(method, params),
            "params": params.cloned().unwrap_or(Value::Null),
        }),
        turn_id: None,
    }
}

/// Best-effort human summary of a notice: the schema's `message` field where
/// there is one, else a compact rendering of the params.
fn notice_text(method: &str, params: Option<&Value>) -> String {
    let detail = params.and_then(|p| {
        for key in ["message", "reason", "name", "status", "state"] {
            if let Some(s) = p.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()) {
                return Some(s.to_owned());
            }
        }
        None
    });
    detail.map_or_else(|| method.to_owned(), |d| format!("{method}: {d}"))
}

/// Map `turn/plan/updated` → a `plan` item, the codex half of the agent task
/// list. Rendered as an assistant plan line by the existing `plan` normalizer.
fn map_plan_updated(local_id: &str, v: &Value) -> Incoming {
    let Some(steps) = v.pointer("/params/plan").and_then(Value::as_array) else {
        return Incoming::Traced { method: "turn/plan/updated".to_owned(), reason: "no plan" };
    };
    let rendered = steps
        .iter()
        .map(|s| {
            let text = s.get("step").and_then(Value::as_str).unwrap_or_default();
            let mark = match s.get("status").and_then(Value::as_str) {
                Some("completed") => "x",
                Some("in_progress") => "~",
                _ => " ",
            };
            format!("- [{mark}] {text}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let explanation = v.pointer("/params/explanation").and_then(Value::as_str);
    let text = explanation
        .filter(|e| !e.is_empty())
        .map_or_else(|| rendered.clone(), |e| format!("{e}\n\n{rendered}"));
    Incoming::Event(AdapterEvent::Message {
        local_id: local_id.to_owned(),
        payload: json!({"type": "plan", "text": text, "plan": steps}),
        turn_id: None,
    })
}

/// Map `thread/compacted` → a `contextCompaction` item: the context-reset
/// boundary the claude adapter already emits on `/clear` + `/compact`.
fn map_compacted(local_id: &str, v: &Value) -> Incoming {
    Incoming::Event(AdapterEvent::Message {
        local_id: local_id.to_owned(),
        payload: json!({
            "type": "contextCompaction",
            "text": v
                .pointer("/params/summary")
                .and_then(Value::as_str)
                .unwrap_or("context compacted"),
        }),
        turn_id: None,
    })
}

/// Map the structured `error` notification → [`AdapterEvent::Status`].
/// `willRetry: true` means codex is retrying the turn itself, so only the
/// detail is surfaced; a non-retried error marks the session failed.
fn map_error_notification(local_id: &str, v: &Value) -> Incoming {
    let Some(message) = v.pointer("/params/error/message").and_then(Value::as_str) else {
        return Incoming::Traced { method: "error".to_owned(), reason: "no error message" };
    };
    let will_retry = v.pointer("/params/willRetry").and_then(Value::as_bool).unwrap_or(false);
    let (state, activity) =
        if will_retry { (None, None) } else { (Some("failed"), Some("failure")) };
    Incoming::Event(AdapterEvent::Status {
        local_id: local_id.to_owned(),
        tempo: None,
        state: state.map(str::to_owned),
        detail: Some(message.to_owned()),
        activity: activity.map(str::to_owned),
        name: None,
        intent: None,
        model: None,
        effort: None,
        permission_mode: None,
        children: vec![],
    })
}

/// Map `turn/completed` whose `turn.status == "failed"` → failed
/// [`AdapterEvent::Status`] carrying the turn error message.
/// Successful turns stay ignored: idle status arrives via
/// `thread/status/changed`.
fn map_turn_completed(local_id: &str, v: &Value) -> Incoming {
    if v.pointer("/params/turn/status").and_then(Value::as_str) != Some("failed") {
        return Incoming::Traced { method: "turn/completed".to_owned(), reason: "turn not failed" };
    }
    let detail = v
        .pointer("/params/turn/error/message")
        .and_then(Value::as_str)
        .unwrap_or("turn failed")
        .to_owned();
    Incoming::Event(AdapterEvent::Status {
        local_id: local_id.to_owned(),
        tempo: None,
        state: Some("failed".to_owned()),
        detail: Some(detail),
        activity: Some("failure".to_owned()),
        name: None,
        intent: None,
        model: None,
        effort: None,
        permission_mode: None,
        children: vec![],
    })
}

fn map_item_completed(local_id: &str, v: &Value) -> Incoming {
    let Some(item) = v.pointer("/params/item") else {
        return Incoming::Traced { method: "item/completed".to_owned(), reason: "no item" };
    };
    Incoming::Event(item_event(local_id, item))
}

/// Split one codex `ThreadItem` onto `ToolUse` vs `Message`. Shared by the
/// live `item/completed` path and [`super::thread_read`]'s replayed history so
/// both render through one mapping.
#[must_use]
pub fn item_event(local_id: &str, item: &Value) -> AdapterEvent {
    let payload = item.clone();
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "commandExecution"
        | "fileChange"
        | "mcpToolCall"
        | "dynamicToolCall"
        | "collabAgentToolCall"
        | "webSearch"
        | "imageView"
        | "imageGeneration" => AdapterEvent::ToolUse { local_id: local_id.to_owned(), payload },
        _ => AdapterEvent::Message { local_id: local_id.to_owned(), payload, turn_id: None },
    }
}

/// Map `thread/status/changed` → [`AdapterEvent::Status`]. The codex
/// `ThreadStatus` (`active` / `idle` / `systemError`) plus `activeFlags`
/// (`waitingOnApproval` / `waitingOnUserInput`) project onto the same
/// `tempo`/`state`/`activity` the classifier consumes: a waiting flag means
/// `tempo = "blocked"` (the ✋ "needs input" signal).
fn map_status(local_id: &str, v: &Value) -> Incoming {
    let Some(status) = v.pointer("/params/status") else {
        return Incoming::Traced {
            method: "thread/status/changed".to_owned(),
            reason: "no status",
        };
    };
    let ty = status.get("type").and_then(Value::as_str).unwrap_or("");
    let waiting = status.get("activeFlags").and_then(Value::as_array).is_some_and(|flags| {
        flags
            .iter()
            .filter_map(Value::as_str)
            .any(|f| f == "waitingOnApproval" || f == "waitingOnUserInput")
    });
    let (tempo, state, activity) = match ty {
        "active" if waiting => (Some("blocked"), Some("working"), None),
        "active" => (Some("active"), Some("working"), None),
        "idle" => (None, Some("idle"), None),
        "systemError" => (None, Some("failed"), Some("failure")),
        // `notLoaded` / unknown — nothing actionable.
        _ => {
            return Incoming::Traced {
                method: "thread/status/changed".to_owned(),
                reason: "status not actionable",
            };
        }
    };
    Incoming::Event(AdapterEvent::Status {
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
        children: vec![],
    })
}

/// Map `thread/tokenUsage/updated` → [`AdapterEvent::TokenUsage`], keyed by
/// `turnId` so the server's per-message aggregation sums turns into the
/// thread total. `inputTokens` includes cached input; subtract it so the
/// non-cached/cached split matches the claude adapter's semantics.
fn map_token_usage(local_id: &str, v: &Value) -> Incoming {
    let turn_id = v.pointer("/params/turnId").and_then(Value::as_str).unwrap_or("");
    if turn_id.is_empty() {
        return Incoming::Traced {
            method: "thread/tokenUsage/updated".to_owned(),
            reason: "no turn id",
        };
    }
    let Some(last) = v.pointer("/params/tokenUsage/last") else {
        return Incoming::Traced {
            method: "thread/tokenUsage/updated".to_owned(),
            reason: "no last usage",
        };
    };
    let g = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached = g("cachedInputTokens");
    Incoming::Event(AdapterEvent::TokenUsage {
        local_id: local_id.to_owned(),
        message_id: turn_id.to_owned(),
        input_tokens: g("inputTokens").saturating_sub(cached),
        output_tokens: g("outputTokens"),
        cache_read_tokens: cached,
        cache_creation_tokens: 0,
    })
}

/// Map `thread/name/updated` → [`AdapterEvent::Status`] carrying just the
/// name. Adapter-level parity with claude; the web display of the name is
/// tracked separately.
fn map_name(local_id: &str, v: &Value) -> Incoming {
    let Some(name) = v.pointer("/params/name").and_then(Value::as_str) else {
        return Incoming::Traced { method: "thread/name/updated".to_owned(), reason: "no name" };
    };
    Incoming::Event(AdapterEvent::Status {
        local_id: local_id.to_owned(),
        tempo: None,
        state: None,
        detail: None,
        activity: None,
        name: Some(name.to_owned()),
        intent: None,
        model: None,
        effort: None,
        permission_mode: None,
        children: vec![],
    })
}

/// Thread identity extracted from a `thread/start` response.
#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub thread_id: String,
    pub cwd: Option<String>,
    pub rollout_path: Option<String>,
}

/// Pull thread identity out of a `thread/start` response `result` object.
#[must_use]
pub fn thread_info(result: &Value) -> Option<ThreadInfo> {
    let t = result.get("thread")?;
    let thread_id = t
        .get("sessionId")
        .or_else(|| t.get("id"))
        .and_then(Value::as_str)
        .map(std::string::ToString::to_string)?;
    Some(ThreadInfo {
        thread_id,
        cwd: t.get("cwd").and_then(Value::as_str).map(std::string::ToString::to_string),
        rollout_path: t.get("path").and_then(Value::as_str).map(std::string::ToString::to_string),
    })
}

/// Build the documented `initialize` request. Capabilities are
/// declared explicitly rather than left to defaults so a protocol change that
/// flips a default is visible here: cctui speaks the stable (non-experimental)
/// API and does not participate in upstream attestation.
fn initialize_req() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": ID_INITIALIZE,
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "cctui", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "experimentalApi": false,
                "requestAttestation": false,
            },
        },
    })
}

/// The `initialized` notification that completes the handshake. Codex expects
/// it after the client has processed the `initialize` response; only then is
/// the server fully ready for `thread/*` requests.
pub(super) fn initialized_notification() -> Value {
    json!({"jsonrpc": "2.0", "method": "initialized"})
}

/// Pull the Codex version out of an `initialize` response and log a diagnostic:
/// info when supported, a loud warning when the server is below
/// [`contract::CODEX_MIN_VERSION`] (the protocol shapes cctui relies on are not
/// guaranteed there). The version is returned so it can ride on the
/// [`AdapterEvent::SessionStarted`] meta for downstream diagnose reports.
pub(super) fn record_codex_version(response: &Value) -> Option<String> {
    let user_agent = response.pointer("/result/userAgent").and_then(Value::as_str);
    let version = user_agent.and_then(contract::version_from_user_agent);
    match &version {
        Some(v) if contract::version_supported(v) => {
            tracing::info!(
                codex_version = %v,
                min = contract::CODEX_MIN_VERSION,
                "codex app-server handshake: supported version",
            );
        }
        Some(v) => {
            tracing::warn!(
                codex_version = %v,
                min = contract::CODEX_MIN_VERSION,
                "codex app-server is below the minimum supported version; protocol may drift",
            );
        }
        None => {
            tracing::warn!(
                user_agent = user_agent.unwrap_or("<missing>"),
                "codex app-server initialize response had no parseable version",
            );
        }
    }
    version
}

/// The gateway routing block as a per-thread `ThreadStart`/`ThreadResumeParams`
/// input rather than a process-level `-c` flag, so one app-server can host
/// threads for several accounts at once.
///
/// The bearer is a literal `authorization` header instead of the `env_key`
/// indirection, which resolves against the *process* env and so cannot differ
/// per thread. Codex persists only `model_provider = "cctui"` in the rollout —
/// never the definition or the secret — so this must be re-supplied on every
/// resume or the thread fails config load.
#[must_use]
pub fn gateway_thread_config(
    env: &std::collections::BTreeMap<String, String>,
) -> Option<(String, Value)> {
    let base_url = env.get("OPENAI_BASE_URL")?;
    let mut headers = json!({"x-openai-actor-authorization": "cctui-gateway"});
    let mut provider = json!({
        "name": "cctui-gateway",
        "base_url": base_url,
        "wire_api": "responses",
    });
    match env.get("OPENAI_API_KEY").filter(|k| !k.is_empty()) {
        Some(key) => headers["authorization"] = json!(format!("Bearer {key}")),
        None => provider["env_key"] = json!("OPENAI_API_KEY"),
    }
    provider["http_headers"] = headers;
    Some(("cctui".to_owned(), json!({"model_providers": {"cctui": provider}})))
}

/// `"default"` or `"fast"` (codex maps `fast` → request tier `priority`).
/// Anything else resolves to `None`: supplying no tier beats guessing one.
#[must_use]
pub fn normalize_service_tier(raw: Option<&str>) -> Option<String> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "default" => Some("default".to_owned()),
        "fast" => Some("fast".to_owned()),
        _ => None,
    }
}

#[must_use]
pub fn service_tier_from_settings(settings: Option<&Value>) -> Option<String> {
    normalize_service_tier(settings?.get("service_tier").and_then(Value::as_str))
}

/// Attach the per-thread provider + credential and the per-thread service tier
/// to a `thread/{start,resume,fork}` params object. A session with no gateway
/// binding keeps codex's default provider.
///
/// The tier rides both the native `serviceTier` param and the per-thread
/// `config` overlay, because codex persists NEITHER in the rollout — only
/// `model_provider` survives — so every resume and fork must re-supply it or
/// the thread silently falls back to codex's own `priority` default.
fn with_thread_config(
    mut params: Value,
    env: &std::collections::BTreeMap<String, String>,
    service_tier: Option<&str>,
) -> Value {
    let Some(map) = params.as_object_mut() else { return params };
    let mut config = match gateway_thread_config(env) {
        Some((provider, config)) => {
            map.insert("modelProvider".to_owned(), json!(provider));
            config
        }
        None => json!({}),
    };
    if let Some(tier) = normalize_service_tier(service_tier) {
        map.insert("serviceTier".to_owned(), json!(tier));
        config["service_tier"] = json!(tier);
    }
    if config.as_object().is_some_and(|c| !c.is_empty()) {
        map.insert("config".to_owned(), config);
    }
    params
}

fn thread_start_req(
    cwd: &str,
    env: &std::collections::BTreeMap<String, String>,
    service_tier: Option<&str>,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": ID_THREAD_START,
        "method": "thread/start",
        "params": with_thread_config(json!({"cwd": cwd}), env, service_tier),
    })
}

fn thread_resume_req(
    thread_id: &str,
    cwd: &str,
    env: &std::collections::BTreeMap<String, String>,
    service_tier: Option<&str>,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": ID_THREAD_START,
        "method": "thread/resume",
        "params": with_thread_config(
            json!({"threadId": thread_id, "cwd": cwd}),
            env,
            service_tier,
        ),
    })
}

/// Fork an existing thread into a brand-new one seeded from its history.
/// The app-server returns a fresh `thread` (its own id) just like
/// `thread/start`, so the response is parsed through the same `ID_THREAD_START`
/// path. Model/effort overrides ride on the subprocess `-c` flags (set in the
/// command pump), mirroring the spawn path, so they apply to the forked thread.
fn thread_fork_req(
    parent_thread_id: &str,
    cwd: &str,
    env: &std::collections::BTreeMap<String, String>,
    service_tier: Option<&str>,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": ID_THREAD_START,
        "method": "thread/fork",
        "params": with_thread_config(
            json!({"threadId": parent_thread_id, "cwd": cwd}),
            env,
            service_tier,
        ),
    })
}

fn thread_name_set_req(id: i64, thread_id: &str, name: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "thread/name/set",
        "params": {"threadId": thread_id, "name": name},
    })
}

/// A native codex thread lifecycle operation. Each maps to a single
/// JSON-RPC method taking `{ threadId }`. Archive/unarchive are wired to the
/// CCTUI archive/reopen actions; `Delete` implements the third native op for
/// parity (no CCTUI destructive-delete action wires to it yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleOp {
    Archive,
    Unarchive,
    #[allow(dead_code)]
    Delete,
}

impl LifecycleOp {
    #[must_use]
    const fn method(self) -> &'static str {
        match self {
            Self::Archive => "thread/archive",
            Self::Unarchive => "thread/unarchive",
            Self::Delete => "thread/delete",
        }
    }
}

fn thread_lifecycle_req(id: i64, op: LifecycleOp, thread_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": op.method(),
        "params": {"threadId": thread_id},
    })
}

/// Whether a `thread/{archive,unarchive,delete}` JSON-RPC error can be treated
/// as success for idempotency: the thread is already in the target
/// state or no longer exists, so CCTUI and native lifecycle state can't wedge
/// each other. Matched on the codex error text since the app-server exposes no
/// stable machine codes for these.
#[must_use]
pub fn is_idempotent_lifecycle_error(op: LifecycleOp, err: &str) -> bool {
    let e = err.to_lowercase();
    // A missing thread makes any lifecycle op a no-op success.
    let missing = e.contains("not found")
        || e.contains("no such")
        || e.contains("does not exist")
        || e.contains("doesn't exist")
        || e.contains("unknown thread")
        || e.contains("no thread");
    // Already in the requested terminal state.
    let already = match op {
        LifecycleOp::Archive => e.contains("already archived"),
        LifecycleOp::Unarchive => e.contains("already unarchived") || e.contains("not archived"),
        LifecycleOp::Delete => e.contains("already deleted"),
    };
    missing || already
}

/// Run a native codex thread lifecycle op via a short-lived stdio
/// `codex app-server`, mirroring the one-shot pattern the
/// [`super::thread_list`] inventory poll uses. Spawns the app-server, sends
/// `initialize` → `initialized` → the lifecycle RPC, correlates the response by
/// id, and reaps the process. Idempotent: an "already in target state" /
/// "thread missing" error resolves as success ([`is_idempotent_lifecycle_error`])
/// so CCTUI and native lifecycle state can't wedge each other. No gateway env is
/// needed — no turn is started.
pub async fn run_thread_lifecycle(
    app: &AppServerConfig,
    daemon: Option<&super::daemon::SharedDaemon>,
    thread_id: &str,
    op: LifecycleOp,
) -> Result<()> {
    if let Some(shared) = daemon
        && let Some(handle) = shared.handle().await
    {
        match handle.request(op.method(), json!({"threadId": thread_id})).await {
            Ok(_) => return Ok(()),
            Err(err) => {
                let msg = err.to_string();
                if is_idempotent_lifecycle_error(op, &msg) {
                    tracing::info!(
                        %thread_id,
                        op = op.method(),
                        "codex lifecycle op idempotent no-op: {msg}"
                    );
                    return Ok(());
                }
                tracing::debug!(%err, op = op.method(), "codex: shared lifecycle op failed, using stdio");
            }
        }
    }
    let mut cmd = Command::new(&app.bin);
    cmd.arg("app-server")
        // No turn is started, so sandbox mode only matters because codex
        // refuses to boot when it cannot create the bwrap namespace on some
        // kernels — pass the configured (host-default) mode through.
        .arg("-c")
        .arg(format!("sandbox_mode=\"{}\"", app.sandbox_mode))
        .env("PATH", crate::childenv::child_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().context("codex app-server stdin unavailable")?;
    let stdout = child.stdout.take().context("codex app-server stdout unavailable")?;

    let req_id = RUN_BASE;
    let outcome = tokio::time::timeout(RPC_TIMEOUT, async {
        let mut lines = BufReader::new(stdout).lines();
        write_json(&mut stdin, &initialize_req()).await?;
        write_json(&mut stdin, &initialized_notification()).await?;
        write_json(&mut stdin, &thread_lifecycle_req(req_id, op, thread_id)).await?;
        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else { continue };
            if v.get("id").and_then(Value::as_i64) == Some(req_id) {
                return anyhow::Ok(response_outcome(&v));
            }
        }
        anyhow::bail!("codex {} response not received before EOF", op.method())
    })
    .await;

    // Close stdin and reap regardless of how the read went.
    drop(stdin);
    let _ = child.start_kill();
    let _ = child.wait().await;

    match outcome {
        Err(_) => anyhow::bail!("codex {} timed out", op.method()),
        Ok(Err(e)) => Err(e),
        Ok(Ok(Ok(_))) => Ok(()),
        Ok(Ok(Err(msg))) => {
            if is_idempotent_lifecycle_error(op, &msg) {
                tracing::info!(
                    %thread_id,
                    op = op.method(),
                    "codex lifecycle op idempotent no-op: {msg}"
                );
                Ok(())
            } else {
                Err(anyhow::anyhow!(msg))
            }
        }
    }
}

/// Build the `input` array for a turn. Staged image attachments ride
/// as native `localImage` items so codex feeds the picture to the model; every
/// other staged file keeps the path/text semantics — its absolute path is
/// listed in the text item, matching the adapter-neutral mid-chat injection.
/// The array is never empty: a turn with only images still carries a text item
/// so an image-only prompt is valid.
fn turn_input_items(text: &str, attachments: &[String]) -> Vec<Value> {
    use std::fmt::Write as _;

    let mut body = text.to_owned();
    let non_images: Vec<&str> = attachments
        .iter()
        .map(String::as_str)
        .filter(|p| !crate::adapters::uploads::is_image_path(p))
        .collect();
    if !non_images.is_empty() {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str("Attached files:");
        for p in non_images {
            let _ = write!(body, "\n  - {p}");
        }
    }

    let mut items = Vec::new();
    if !body.is_empty() {
        items.push(json!({"type": "text", "text": body}));
    }
    for p in attachments.iter().filter(|p| crate::adapters::uploads::is_image_path(p)) {
        items.push(json!({"type": "localImage", "path": p}));
    }
    if items.is_empty() {
        items.push(json!({"type": "text", "text": ""}));
    }
    items
}

/// Build a `turn/start`. An in-place model/effort change rides here
/// as a per-turn override that codex promotes to the later default — the stable
/// alternative to the `experimentalApi`-gated `thread/settings/update`. Only
/// set fields are sent so an unchanged setting keeps codex's own default.
/// Staged attachments become native image / path-in-text inputs.
fn turn_start_req(
    id: i64,
    thread_id: &str,
    text: &str,
    attachments: &[String],
    model: Option<&str>,
    effort: Option<&str>,
) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_owned(), json!(thread_id));
    params.insert("input".to_owned(), json!(turn_input_items(text, attachments)));
    if let Some(model) = model {
        params.insert("model".to_owned(), json!(model));
    }
    if let Some(effort) = effort {
        params.insert("effort".to_owned(), json!(effort));
    }
    json!({"jsonrpc": "2.0", "id": id, "method": "turn/start", "params": params})
}

/// Steer a user message into the currently active turn. Unlike
/// `turn/start` — which codex rejects while a turn is in flight — `turn/steer`
/// appends the input to the running turn. `expectedTurnId` is a precondition:
/// the request fails if it no longer matches the active turn (it just ended),
/// which the driver recovers from by falling back to `turn/start`. Attachments
/// build the same native image / path-in-text inputs as a start.
fn turn_steer_req(
    id: i64,
    thread_id: &str,
    expected_turn_id: &str,
    text: &str,
    attachments: &[String],
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "turn/steer",
        "params": {
            "threadId": thread_id,
            "expectedTurnId": expected_turn_id,
            "input": turn_input_items(text, attachments),
        },
    })
}

/// Interrupt the active turn. `TurnInterruptParams` requires both
/// `threadId` and `turnId`; codex rejects the request with `-32602` otherwise.
fn turn_interrupt_req(id: i64, thread_id: &str, turn_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "turn/interrupt",
        "params": {"threadId": thread_id, "turnId": turn_id},
    })
}

/// A turn lifecycle transition parsed from a `turn/started` or `turn/completed`
/// notification. The driver tracks the active turn id from these so a
/// follow-up message is routed via `turn/steer` into the running turn instead
/// of a second `turn/start` codex would reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnLifecycle {
    Started { turn_id: String },
    Completed { turn_id: String },
}

/// Extract a [`TurnLifecycle`] from a `turn/started` / `turn/completed`
/// notification. Both carry `params.turn.id`; anything else yields `None`.
#[must_use]
pub fn turn_lifecycle(v: &Value) -> Option<TurnLifecycle> {
    let method = v.get("method").and_then(Value::as_str)?;
    let turn_id = v.pointer("/params/turn/id").and_then(Value::as_str)?.to_owned();
    match method {
        "turn/started" => Some(TurnLifecycle::Started { turn_id }),
        "turn/completed" => Some(TurnLifecycle::Completed { turn_id }),
        _ => None,
    }
}

/// Tracks the session's in-flight turn. `turn/started` sets the
/// active turn; a `turn/completed` for the SAME turn clears it. The active id
/// selects `turn/steer` (with it as `expectedTurnId`) over `turn/start`.
#[derive(Debug, Default)]
pub struct ActiveTurn {
    id: Option<String>,
}

impl ActiveTurn {
    pub fn apply(&mut self, ev: &TurnLifecycle) {
        match ev {
            TurnLifecycle::Started { turn_id } => self.id = Some(turn_id.clone()),
            TurnLifecycle::Completed { turn_id } => {
                if self.id.as_deref() == Some(turn_id.as_str()) {
                    self.id = None;
                }
            }
        }
    }

    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn clear(&mut self) {
        self.id = None;
    }
}

/// Accumulates streamed item deltas by item id. Codex ships an item
/// as `item/started` → `item/<kind>/delta`* → `item/completed`. The completed
/// item is authoritative for rendering — mirroring the claude adapter, which
/// drops partial SSE deltas in favour of the coalesced final frame — so deltas
/// never emit their own events. They are consumed here only to back-fill a
/// completed item the server left text-empty: reasoning items in particular
/// ship `content: []` / encrypted content while the visible reasoning arrived
/// solely via `item/reasoning/textDelta`, so without this they render blank.
#[derive(Debug, Default)]
pub struct ItemAccumulator {
    /// `agentMessage` / `plan` text (`item/agentMessage|plan/delta`).
    text: HashMap<String, String>,
    /// `reasoning` content (`item/reasoning/textDelta`).
    reasoning: HashMap<String, String>,
    /// `reasoning` summary (`item/reasoning/summaryTextDelta`).
    summary: HashMap<String, String>,
    /// `commandExecution` aggregated output (`item/commandExecution/outputDelta`).
    output: HashMap<String, String>,
    /// itemId → item type, seeded from `item/started`.
    started: HashMap<String, String>,
}

fn push_delta(map: &mut HashMap<String, String>, v: &Value) {
    if let (Some(id), Some(delta)) = (
        v.pointer("/params/itemId").and_then(Value::as_str),
        v.pointer("/params/delta").and_then(Value::as_str),
    ) {
        map.entry(id.to_owned()).or_default().push_str(delta);
    }
}

/// A JSON `content`/`summary` field carries no renderable text: absent, an
/// empty array, or an array of only empty strings.
fn is_text_empty(field: Option<&Value>) -> bool {
    match field {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => {
            a.iter().all(|e| e.as_str().is_none_or(str::is_empty) && e.get("text").is_none())
        }
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

impl ItemAccumulator {
    /// Feed one inbound notification: record `item/started` item types and
    /// append `item/*/delta` text by item id. No-op for anything else.
    pub fn note(&mut self, v: &Value) {
        match v.get("method").and_then(Value::as_str) {
            Some("item/started") => {
                if let (Some(id), Some(ty)) = (
                    v.pointer("/params/item/id").and_then(Value::as_str),
                    v.pointer("/params/item/type").and_then(Value::as_str),
                ) {
                    self.started.insert(id.to_owned(), ty.to_owned());
                }
            }
            Some("item/agentMessage/delta" | "item/plan/delta") => push_delta(&mut self.text, v),
            Some("item/reasoning/textDelta") => push_delta(&mut self.reasoning, v),
            Some("item/reasoning/summaryTextDelta") => push_delta(&mut self.summary, v),
            Some("item/commandExecution/outputDelta" | "command/exec/outputDelta") => {
                push_delta(&mut self.output, v);
            }
            _ => {}
        }
    }

    /// If `v` is an `item/completed`, back-fill any empty text/output field on
    /// the item from the accumulated stream, then forget that item's buffers.
    /// Every other notification is returned unchanged.
    #[must_use]
    pub fn enrich_completed(&mut self, mut v: Value) -> Value {
        if v.get("method").and_then(Value::as_str) != Some("item/completed") {
            return v;
        }
        let Some(item) = v.pointer_mut("/params/item") else { return v };
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return v;
        };
        match item.get("type").and_then(Value::as_str).unwrap_or_default() {
            "agentMessage" | "plan" => {
                if let Some(buf) = self.text.get(&id).filter(|b| !b.is_empty())
                    && item.get("text").and_then(Value::as_str).unwrap_or_default().is_empty()
                {
                    item["text"] = json!(buf);
                }
            }
            "reasoning" => {
                if let Some(buf) = self.reasoning.get(&id).filter(|b| !b.is_empty())
                    && is_text_empty(item.get("content"))
                {
                    item["content"] = json!([buf]);
                }
                if let Some(buf) = self.summary.get(&id).filter(|b| !b.is_empty())
                    && is_text_empty(item.get("summary"))
                {
                    item["summary"] = json!([buf]);
                }
            }
            "commandExecution" => {
                if let Some(buf) = self.output.get(&id).filter(|b| !b.is_empty())
                    && item
                        .get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .is_empty()
                {
                    item["aggregatedOutput"] = json!(buf);
                }
            }
            _ => {}
        }
        self.forget(&id);
        v
    }

    fn forget(&mut self, id: &str) {
        self.text.remove(id);
        self.reasoning.remove(id);
        self.summary.remove(id);
        self.output.remove(id);
        self.started.remove(id);
    }
}

/// How a user message is delivered given the current active turn:
/// steer into a running turn, else start a fresh one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDispatch {
    Start,
    Steer { turn_id: String },
}

#[must_use]
pub fn prompt_dispatch(active: &ActiveTurn) -> PromptDispatch {
    active.id().map_or(PromptDispatch::Start, |turn_id| PromptDispatch::Steer {
        turn_id: turn_id.to_owned(),
    })
}

/// How to recover from a `turn/steer` failure. A turn that just ended
/// (the common `expectedTurnId` race) frees the turn slot, so the message is
/// retried as a fresh `turn/start`; a turn that is running but non-steerable
/// (`/review` or manual `/compact`, `activeTurnNotSteerable`) would reject a
/// `turn/start` too, so the message is rejected visibly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerRecovery {
    FallbackToStart,
    Reject,
}

#[must_use]
pub fn steer_recovery(error: &str) -> SteerRecovery {
    if error.to_lowercase().contains("steerable") {
        SteerRecovery::Reject
    } else {
        SteerRecovery::FallbackToStart
    }
}

/// Reply to a server-issued approval request. `rpc_id` must be the exact
/// `id` value from the request; `kind` selects the decision vocabulary.
fn approval_reply(rpc_id: &Value, kind: ApprovalKind, allow: bool) -> Value {
    json!({"jsonrpc": "2.0", "id": rpc_id, "result": {"decision": kind.decision(allow)}})
}

/// One outstanding outbound JSON-RPC request.
#[derive(Debug)]
pub struct PendingRpc {
    pub method: String,
    /// Server-minted correlation id: when set, the request's outcome is
    /// reported back as an [`AdapterEvent::CommandResult`].
    pub command_id: Option<Uuid>,
    pub deadline: Instant,
}

impl PendingRpc {
    /// Whether this request is part of the session-establishing handshake —
    /// its failure means the session cannot run at all.
    #[must_use]
    pub fn is_handshake(&self) -> bool {
        matches!(
            self.method.as_str(),
            "initialize" | "thread/start" | "thread/resume" | "thread/fork"
        )
    }
}

/// Correlation table for outbound JSON-RPC requests, keyed by request id.
/// The driver inserts before each write, resolves on the matching
/// response (propagating `error` objects as failures), expires entries past
/// their deadline, and drains everything when the app-server process exits.
#[derive(Debug, Default)]
pub struct PendingRpcs {
    inner: HashMap<i64, PendingRpc>,
}

impl PendingRpcs {
    pub fn insert(&mut self, id: i64, method: &str, command_id: Option<Uuid>, deadline: Instant) {
        self.inner.insert(id, PendingRpc { method: method.to_owned(), command_id, deadline });
    }

    /// Resolve the pending request matching a response `id`. Returns the
    /// entry plus the parsed outcome; `None` for an unknown id.
    pub fn resolve(
        &mut self,
        id: i64,
        response: &Value,
    ) -> Option<(PendingRpc, Result<Value, String>)> {
        let pending = self.inner.remove(&id)?;
        Some((pending, response_outcome(response)))
    }

    /// Forget a request whose write never reached the app-server, so the
    /// retry that re-issues it owns its correlation id alone.
    pub fn remove(&mut self, id: i64) -> Option<PendingRpc> {
        self.inner.remove(&id)
    }

    /// Remove and return every request whose deadline has passed.
    pub fn expire(&mut self, now: Instant) -> Vec<(i64, PendingRpc)> {
        self.inner.extract_if(|_, p| p.deadline <= now).collect()
    }

    /// Remove and return everything — the process is gone, nothing pending
    /// can ever resolve.
    pub fn drain(&mut self) -> Vec<(i64, PendingRpc)> {
        self.inner.drain().collect()
    }

    /// Methods of every outstanding request (diagnostics).
    #[must_use]
    pub fn pending_methods(&self) -> Vec<String> {
        self.inner.values().map(|p| p.method.clone()).collect()
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Parse a JSON-RPC response into success (`result`) or failure (the `error`
/// object rendered as a message).
fn response_outcome(v: &Value) -> Result<Value, String> {
    let Some(err) = v.get("error").filter(|e| !e.is_null()) else {
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    };
    let message = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
    let mut out = err.get("code").and_then(Value::as_i64).map_or_else(
        || format!("codex app-server error: {message}"),
        |code| format!("codex app-server error {code}: {message}"),
    );
    if let Some(data) = err.get("data").filter(|d| !d.is_null()) {
        use std::fmt::Write as _;
        let _ = write!(out, " ({data})");
    }
    Err(out)
}

// ---------------------------------------------------------------------------
// Async driver
// ---------------------------------------------------------------------------

/// Holds the spawn/fork `command_id` until the launch outcome is known: the
/// success ack is deferred to `thread/start`/`thread/resume`/`thread/fork`
/// succeeding, and every failure path (JSON-RPC error, timeout, process exit,
/// spawn error) resolves it as a failure instead. One-shot: the
/// first resolution wins, later calls are no-ops.
struct SpawnAck {
    command_id: Option<Uuid>,
    events: mpsc::Sender<AdapterEvent>,
}

impl SpawnAck {
    async fn ok(&mut self) {
        if let Some(command_id) = self.command_id.take() {
            let _ = self
                .events
                .send(AdapterEvent::CommandResult { command_id, ok: true, error: None })
                .await;
        }
    }

    async fn fail(&mut self, error: &str) {
        if let Some(command_id) = self.command_id.take() {
            let _ = self
                .events
                .send(AdapterEvent::CommandResult {
                    command_id,
                    ok: false,
                    error: Some(error.to_owned()),
                })
                .await;
        }
    }
}

/// Per-session commands routed from the adapter-level command pump.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Answer a pending approval (`request_id` came from the emitted
    /// `PermissionRequest`).
    Permission { request_id: String, allow: bool },
    /// Start a new turn with user text. `command_id` correlates the
    /// `turn/start` (or `turn/steer`) JSON-RPC outcome back to an
    /// [`AdapterEvent::CommandResult`].
    Send { text: String, command_id: Option<Uuid> },
    /// Persist the display name into Codex's thread metadata.
    Rename { name: String },
    /// Interrupt the in-flight turn and terminate the session. `signal` is
    /// the requested POSIX signal: `Some(15)` (SIGTERM) for a graceful stop
    /// that lets codex flush its rollout file; anything else (incl. `None`)
    /// falls back to an immediate SIGKILL.
    Kill { signal: Option<i32> },
    /// Interrupt the in-flight turn but KEEP the session alive:
    /// sends `turn/interrupt` WITHOUT terminating the app-server, so the
    /// thread stays resumable. Distinct from `Kill`, which interrupts *and*
    /// terminates the child. `command_id` correlates the `turn/interrupt`
    /// JSON-RPC outcome back to an [`AdapterEvent::CommandResult`].
    Interrupt { command_id: Option<Uuid> },
    /// Change the model and/or reasoning effort of the running thread in place:
    /// records the override so the next `turn/start` carries it (a
    /// stable per-turn override codex promotes to the later default),
    /// and echoes the resolved values back via [`AdapterEvent::Status`] so the
    /// webui chip updates live. `command_id` correlates the outcome back as an
    /// [`AdapterEvent::CommandResult`].
    SetModel { model: Option<String>, effort: Option<String>, command_id: Option<Uuid> },
    /// Gather a point-in-time snapshot of the live driver's internal state for
    /// the adapter-neutral diagnose report and return it on `reply`.
    Diagnose { reply: mpsc::Sender<CodexLiveSnapshot> },
}

impl SessionCommand {
    #[must_use]
    pub const fn is_resumable(&self) -> bool {
        matches!(self, Self::Send { .. } | Self::Rename { .. } | Self::SetModel { .. })
    }

    #[must_use]
    pub const fn command_id(&self) -> Option<Uuid> {
        match self {
            Self::Send { command_id, .. }
            | Self::SetModel { command_id, .. }
            | Self::Interrupt { command_id } => *command_id,
            _ => None,
        }
    }
}

/// Point-in-time snapshot of a live codex session's internal driver state,
/// gathered on demand for the diagnose report.
#[derive(Debug, Clone, Default)]
pub struct CodexLiveSnapshot {
    pub codex_version: Option<String>,
    pub pid: Option<u32>,
    pub active_turn_id: Option<String>,
    pub pending_rpc_methods: Vec<String>,
    pub protocol_errors: Vec<CodexProtocolError>,
    pub stderr_tail: Vec<CodexStderrLine>,
    pub rpc_tail: Vec<CodexRpcFrame>,
    pub rollout_path: Option<String>,
    pub rollout_size_bytes: Option<u64>,
}

/// Live command registry: `local_id` → command sender for the owning app-server
/// task. Senders disappear when the app-server exits; the durable
/// [`SessionRegistry`] below stays so a later reply can revive the thread.
pub type LiveSessionRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<SessionCommand>>>>;

/// Durable-in-daemon metadata for cctui-owned Codex threads. This is not a
/// process handle; it is the minimum launch context needed to call
/// `thread/resume` after a clean app-server exit. The log-tail also
/// uses this map as the ownership set so it does not double-ingest these
/// rollout files while they are hibernated.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub cfg: AppServerConfig,
    pub cwd: String,
    pub name: Option<String>,
    /// Resolved launch-time env — chiefly the gateway-routing
    /// credential pulled from the server's durable `sessions.account_id`
    /// binding. Stored so a resume relaunches the codex app-server with the
    /// same gateway env instead of starting env-less and 401ing (the codex
    /// analogue of the claude cold-launch bug).
    pub env: std::collections::BTreeMap<String, String>,
    /// Whether this thread's launch declared the `CctuiAgent` relay. Persisted
    /// because the relay map is process-local and a resume carries no
    /// capability to re-derive the decision from.
    pub spawn_relay: bool,
}

/// `local_id` → cctui-owned Codex thread metadata.
pub type SessionRegistry = Arc<Mutex<HashMap<String, SessionRecord>>>;

// `Resume` carries a full `SessionRecord` (now incl. the launch env);
// the size gap to the unit `Delivered`/`Missing` variants is intrinsic and the
// value is short-lived (built, matched, dropped per command), so boxing it
// would add an allocation for no real benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum RouteAction {
    Delivered,
    Resume { record: SessionRecord, command: SessionCommand },
    Missing,
}

/// Try the live sender first. If it is gone or closed, fall back to the
/// durable Codex thread record so the caller can spawn a resume driver.
pub async fn route_or_prepare_resume(
    live: &LiveSessionRegistry,
    sessions: &SessionRegistry,
    local_id: &str,
    command: SessionCommand,
) -> RouteAction {
    let sender = live.lock().await.get(local_id).cloned();
    if let Some(tx) = sender {
        if tx.send(command.clone()).await.is_ok() {
            return RouteAction::Delivered;
        }
        live.lock().await.remove(local_id);
        tracing::warn!(%local_id, "codex: live session command channel closed");
    }

    sessions
        .lock()
        .await
        .get(local_id)
        .cloned()
        .map_or(RouteAction::Missing, |record| RouteAction::Resume { record, command })
}

/// Configuration for spawning the `codex app-server` subprocess.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppServerConfig {
    /// Binary to invoke (default `"codex"`).
    pub bin: String,
    /// Approval policy passed via `-c approval_policy=...`. `"untrusted"`
    /// (the default) makes Codex ask for approval on commands so the relay
    /// has something to forward; `"never"` disables prompts.
    pub approval_policy: String,
    /// Sandbox mode passed via `-c sandbox_mode=...`. `"read-only"`
    /// and `"workspace-write"` wrap commands in bubblewrap; on a host whose
    /// kernel forbids unprivileged user namespaces those fail to launch, so a
    /// per-host default of `"danger-full-access"` (no sandbox) is required
    /// there. Overridable per-spawn via the full-access toggle.
    pub sandbox_mode: String,
    /// Reasoning effort passed via `-c model_reasoning_effort=...`
    /// (codex: `minimal`/`low`/`medium`/`high`). `None` keeps the codex
    /// default. Set per-spawn from the spawn request.
    pub reasoning_effort: Option<String>,
    /// Model passed via `-c model="…"`. `None` keeps the codex
    /// default. Set per-spawn from the spawn request.
    pub model: Option<String>,
    /// Per-session service tier, `"default"` or `"fast"`. NOT a
    /// `config_overrides()` key: it is per-thread, and lives here only because
    /// this struct is the durable per-session cache (persisted in
    /// [`SessionRecord::cfg`]) that lets `thread/{resume,fork}` re-supply it.
    /// `None` keeps codex's own (expensive `priority`) default.
    pub service_tier: Option<String>,
    /// Whether to refresh the codex model catalog on session start
    /// by issuing `model/list` over this session's authenticated app-server
    /// connection. `false` (`model_catalog = false`) disables the refresh.
    pub model_catalog: bool,
}

impl Default for AppServerConfig {
    fn default() -> Self {
        Self {
            bin: "codex".to_string(),
            approval_policy: "untrusted".to_string(),
            sandbox_mode: "workspace-write".to_string(),
            reasoning_effort: None,
            model: None,
            service_tier: None,
            model_catalog: true,
        }
    }
}

impl AppServerConfig {
    /// The `-c key="value"` overrides passed to `codex app-server` for a spawn.
    /// This is the COMPLETE set of config knobs cctui sets. They are
    /// PROCESS-level — they apply to every thread this app-server serves — so
    /// nothing per-session belongs here.
    ///
    /// Fast mode (`service_tier = "fast"`) is deliberately absent. It is a
    /// speed/price tier — 1.5x speed and increased usage on the SAME model at
    /// the SAME quality, not a quality downgrade — and it is per-thread, so it
    /// rides `with_thread_config()` on `thread/{start,resume,fork}` instead.
    ///
    /// Omitting it here is NOT the safe branch: codex's own default tier is
    /// `priority` (every gpt-5.x entry in `models_cache.json` carries
    /// `"default_service_tier": "priority"`), so an app-server with no opinion
    /// runs the EXPENSIVE tier. The server resolves a concrete tier per session;
    /// the daemon must supply it per thread.
    #[must_use]
    pub fn config_overrides(&self) -> Vec<(String, String)> {
        let mut args = vec![
            ("approval_policy".to_owned(), self.approval_policy.clone()),
            ("sandbox_mode".to_owned(), self.sandbox_mode.clone()),
        ];
        if let Some(effort) = self.reasoning_effort.as_deref() {
            args.push(("model_reasoning_effort".to_owned(), effort.to_owned()));
        }
        if let Some(model) = self.model.as_deref() {
            args.push(("model".to_owned(), model.to_owned()));
        }
        args
    }

    pub fn from_value(v: &Value) -> Self {
        let mut cfg = Self::default();
        if let Some(b) = v.get("codex_bin").and_then(Value::as_str) {
            cfg.bin = b.to_string();
        }
        if let Some(p) = v.get("approval_policy").and_then(Value::as_str) {
            cfg.approval_policy = p.to_string();
        }
        if let Some(s) = v.get("sandbox_mode").and_then(Value::as_str) {
            cfg.sandbox_mode = s.to_string();
        }
        if let Some(e) = v.get("model_reasoning_effort").and_then(Value::as_str) {
            cfg.reasoning_effort = Some(e.to_string());
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            cfg.model = Some(m.to_string());
        }
        cfg.service_tier = normalize_service_tier(v.get("service_tier").and_then(Value::as_str));
        cfg.model_catalog = model_list::catalog_enabled(v);
        cfg
    }
}

/// The `-c` overrides that route codex's model provider through the cctui
/// gateway. Codex does NOT honor `OPENAI_BASE_URL`/`OPENAI_API_KEY`
/// from the environment alone: launched with only those env vars it POSTs to
/// api.openai.com with no Authorization header and 401s. It reads them solely
/// through a `model_providers` entry — `base_url` inlined here, the bearer via
/// `env_key` from the launch env at request time. Mirrors the worker
/// entrypoint's `phase_codex_config`, which fixed the same failure
/// for k8s workers by writing this block into config.toml. Empty only when the
/// base URL is absent (an unbound session keeps codex's default provider).
///
/// The block is emitted on the base URL ALONE, without the credential: codex
/// persists `model_provider = "cctui"` in the rollout, so a relaunch that omits
/// the definition fails config load on resume and bricks the thread
/// permanently, whereas a definition whose `env_key` is unset merely fails the
/// turn and heals on the next credential pull.
#[must_use]
pub fn gateway_provider_overrides(
    env: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let Some(base_url) = env.get("OPENAI_BASE_URL") else {
        return Vec::new();
    };
    vec![
        ("model_provider".to_owned(), "cctui".to_owned()),
        ("model_providers.cctui.name".to_owned(), "cctui-gateway".to_owned()),
        ("model_providers.cctui.base_url".to_owned(), base_url.clone()),
        ("model_providers.cctui.env_key".to_owned(), "OPENAI_API_KEY".to_owned()),
        ("model_providers.cctui.wire_api".to_owned(), "responses".to_owned()),
        // Codex only registers the built-in `image_gen` tool for a provider that
        // `uses_openai_actor_authorization()` — a non-empty static
        // `x-openai-actor-authorization` header with `requires_openai_auth`
        // false. The value is never read upstream: the gateway strips it.
        (
            "model_providers.cctui.http_headers.\"x-openai-actor-authorization\"".to_owned(),
            "cctui-gateway".to_owned(),
        ),
    ]
}

/// Quote a value that is always a TOML string (`config_overrides` and the
/// gateway provider block are string-valued by construction).
fn quoted(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    pairs.into_iter().map(|(k, v)| (k, format!("\"{v}\""))).collect()
}

/// The full, ordered `-c key=value` list for an app-server launch, with values
/// already rendered as TOML literals.
///
/// Per-account settings go FIRST and cctui's managed overrides LAST, and a
/// managed key drops any account entry of the same name outright: the ladder
/// must not depend on codex's own last-wins behaviour for `-c` duplicates, and
/// an account must never be able to move gateway routing, the permission
/// posture, or the session's model.
fn launch_overrides(
    cfg: &AppServerConfig,
    env: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let managed: Vec<(String, String)> =
        [quoted(cfg.config_overrides()), quoted(gateway_provider_overrides(env))].concat();
    let account = env
        .get(cctui_proto::codex_config::CONFIG_TOML_ENV)
        .map(|b| cctui_proto::codex_config::overrides_from_block(b))
        .unwrap_or_default();
    let owned: std::collections::BTreeSet<&str> = managed.iter().map(|(k, _)| k.as_str()).collect();
    account
        .into_iter()
        .filter(|(k, _)| !owned.contains(k.as_str()))
        .chain(managed.iter().cloned())
        .collect()
}

#[derive(Debug, Clone)]
enum SessionLaunch {
    Fresh {
        prompt: Option<String>,
        name: Option<String>,
        /// Staged spawn-attachment paths, fed into the first turn.
        attachments: Vec<String>,
    },
    Resume {
        thread_id: String,
        initial_commands: Vec<SessionCommand>,
    },
    /// Fork a parent thread into a new one seeded from its history.
    /// Post-fork it behaves like `Fresh` (optional name + first turn), but the
    /// start handshake sends `thread/fork { threadId }` and the resulting
    /// `SessionStarted` carries `parent_local_id` for discoverability.
    Fork {
        parent_thread_id: String,
        prompt: Option<String>,
        name: Option<String>,
        attachments: Vec<String>,
    },
}

/// One spawned Codex session: owns a `codex app-server` subprocess and a
/// single thread within it.
pub struct CodexSession {
    cfg: AppServerConfig,
    cwd: String,
    /// Launch-time env merged onto the `codex app-server` child process.
    /// Holds the gateway-routing credential resolved at spawn /
    /// fork / resume; see [`SessionRecord::env`].
    env: std::collections::BTreeMap<String, String>,
    launch: SessionLaunch,
    /// Spawn/fork correlation id: resolved as an
    /// [`AdapterEvent::CommandResult`] only once the launch outcome is known.
    command_id: Option<Uuid>,
    /// Server-pre-minted session id, echoed on `SessionStarted` so childwatch
    /// can bind the thread codex mints to the `CctuiAgent` waiter.
    spawn_key: Option<String>,
    /// The spawning parent session for a `CctuiAgent` child, carried onto
    /// `SessionStarted` so the server nests it under its caller.
    parent_local_id: Option<String>,
    /// `CctuiAgent` relay to declare to this app-server, when the server granted
    /// the session spawn rights. `None` means the tool is absent — a session
    /// without a capability must not be able to see it.
    agent_mcp: Option<crate::adapters::agent_mcp::AgentMcp>,
    events: mpsc::Sender<AdapterEvent>,
    live: LiveSessionRegistry,
    registry: SessionRegistry,
    shutdown: CancellationToken,
}

impl CodexSession {
    #[must_use]
    pub fn with_agent_mcp(
        mut self,
        agent_mcp: Option<crate::adapters::agent_mcp::AgentMcp>,
    ) -> Self {
        self.agent_mcp = agent_mcp;
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub const fn new_fresh(
        cfg: AppServerConfig,
        cwd: String,
        env: std::collections::BTreeMap<String, String>,
        prompt: Option<String>,
        name: Option<String>,
        attachments: Vec<String>,
        command_id: Option<Uuid>,
        spawn_key: Option<String>,
        parent_local_id: Option<String>,
        events: mpsc::Sender<AdapterEvent>,
        live: LiveSessionRegistry,
        registry: SessionRegistry,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            cfg,
            cwd,
            env,
            launch: SessionLaunch::Fresh { prompt, name, attachments },
            command_id,
            spawn_key,
            parent_local_id,
            agent_mcp: None,
            events,
            live,
            registry,
            shutdown,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub const fn new_fork(
        cfg: AppServerConfig,
        cwd: String,
        env: std::collections::BTreeMap<String, String>,
        parent_thread_id: String,
        prompt: Option<String>,
        name: Option<String>,
        attachments: Vec<String>,
        command_id: Option<Uuid>,
        events: mpsc::Sender<AdapterEvent>,
        live: LiveSessionRegistry,
        registry: SessionRegistry,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            cfg,
            cwd,
            env,
            launch: SessionLaunch::Fork { parent_thread_id, prompt, name, attachments },
            command_id,
            spawn_key: None,
            parent_local_id: None,
            agent_mcp: None,
            events,
            live,
            registry,
            shutdown,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub const fn new_resume(
        cfg: AppServerConfig,
        cwd: String,
        env: std::collections::BTreeMap<String, String>,
        thread_id: String,
        initial_commands: Vec<SessionCommand>,
        events: mpsc::Sender<AdapterEvent>,
        live: LiveSessionRegistry,
        registry: SessionRegistry,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            cfg,
            cwd,
            env,
            launch: SessionLaunch::Resume { thread_id, initial_commands },
            command_id: None,
            spawn_key: None,
            parent_local_id: None,
            agent_mcp: None,
            events,
            live,
            registry,
            shutdown,
        }
    }

    /// Spawn the subprocess, complete the handshake, then pump IO until the
    /// process exits, the session is killed, or the daemon shuts down.
    /// The spawn/fork `command_id` (when present) is resolved exactly once:
    /// `ok` after the thread request succeeds, failure on any other outcome.
    pub async fn run(mut self) -> Result<()> {
        let mut ack = SpawnAck { command_id: self.command_id.take(), events: self.events.clone() };
        let rings = Arc::new(DiagnoseRings::default());
        let res = self.run_inner(&mut ack, &rings).await;
        match &res {
            Err(err) => {
                let detail = format!("{err}{}", stderr_tail(&rings));
                self.fail_handshake(&mut ack, &detail).await;
            }
            Ok(()) => ack.fail("codex app-server exited before the thread was started").await,
        }
        res
    }

    /// Resolve the spawn ack as failed. A resume has no `command_id`, so its
    /// failure is reported on the thread instead: a failed `Status` plus a
    /// `SessionEnded` the server persists as `resume_failed`.
    async fn fail_handshake(&self, ack: &mut SpawnAck, detail: &str) {
        ack.fail(detail).await;
        let SessionLaunch::Resume { thread_id, .. } = &self.launch else { return };
        self.events
            .send(AdapterEvent::Status {
                local_id: thread_id.clone(),
                tempo: None,
                state: Some("failed".to_owned()),
                detail: Some(detail.to_owned()),
                activity: Some("failure".to_owned()),
                name: None,
                intent: None,
                model: None,
                effort: None,
                permission_mode: None,
                children: Vec::new(),
            })
            .await
            .ok();
        self.events
            .send(AdapterEvent::SessionEnded {
                local_id: thread_id.clone(),
                reason: EndReason::ResumeFailed { detail: detail.to_owned() },
            })
            .await
            .ok();
    }

    fn thread_request(&self) -> (Value, &'static str) {
        let tier = self.cfg.service_tier.as_deref();
        match &self.launch {
            SessionLaunch::Fresh { .. } => {
                (thread_start_req(&self.cwd, &self.env, tier), "thread/start")
            }
            SessionLaunch::Resume { thread_id, .. } => {
                (thread_resume_req(thread_id, &self.cwd, &self.env, tier), "thread/resume")
            }
            SessionLaunch::Fork { parent_thread_id, .. } => {
                (thread_fork_req(parent_thread_id, &self.cwd, &self.env, tier), "thread/fork")
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    async fn run_inner(&self, ack: &mut SpawnAck, rings: &Arc<DiagnoseRings>) -> Result<()> {
        let cwd_path = std::path::Path::new(&self.cwd);
        if !cwd_path.is_dir() {
            anyhow::bail!("spawn: working_dir does not exist or is not a directory: {}", self.cwd);
        }

        let mut cmd = Command::new(&self.cfg.bin);
        cmd.arg("app-server");
        for (key, value) in launch_overrides(&self.cfg, &self.env) {
            cmd.arg("-c").arg(format!("{key}={value}"));
        }
        // Already TOML literals (quoted scalar / array), unlike the scalar knobs
        // above which are quoted here.
        if let Some(agent_mcp) = &self.agent_mcp {
            for (key, value) in agent_mcp.codex_config_overrides() {
                cmd.arg("-c").arg(format!("{key}={value}"));
            }
        }
        // Forward the resolved launch env — chiefly the gateway
        // credential pulled from the server's `sessions.account_id` binding —
        // onto the app-server child, so a session bound to a named gateway
        // account routes through it instead of hitting the default upstream and
        // 401ing. Applied before `PATH` below so the launchd PATH fix wins even
        // if the resolved env carried a `PATH` of its own. The fail-closed
        // contract (refuse an account-bound launch with empty gateway env) is
        // enforced upstream in the adapter command pump;.
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
        let mut child = cmd
            .current_dir(cwd_path)
            // launchd strips `PATH` down to a minimal set that omits
            // `/opt/homebrew/bin`, so a bare `codex` fails ENOENT.
            .env("PATH", crate::childenv::child_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Capture stderr (the app-server's log stream) rather than
            // discarding it — it is the only diagnostic when codex dies
            // unexpectedly (CCT macOS "randomly dies" report).
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn `{} app-server`", self.cfg.bin))?;

        let mut stdin = RpcStdin {
            inner: child.stdin.take().context("child stdin missing")?,
            rings: rings.clone(),
        };
        let stdout = child.stdout.take().context("child stdout missing")?;
        let mut lines = BufReader::new(stdout).lines();

        // Drain stderr into the bounded ring in the background. Each line
        // is also logged at info under its own target; the retained tail is
        // surfaced in every failure detail (handshake and crash).
        let stderr_drain = child.stderr.take().map(|stderr| {
            let rings = rings.clone();
            tokio::spawn(async move {
                let mut err_lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = err_lines.next_line().await {
                    tracing::info!(target: "codex_app_server_stderr", "{line}");
                    rings.note_stderr(&line);
                }
            })
        });

        // Handshake: initialize → thread/start or thread/resume.
        let mut pending_rpcs = PendingRpcs::default();
        let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        pending_rpcs.insert(ID_INITIALIZE, "initialize", None, handshake_deadline);
        // EPIPE here means codex already died (auth/config errors exit at
        // once); let the stdout EOF below reach the epilogue, which reports the
        // exit status with the stderr tail.
        if let Err(e) = stdin.send(&initialize_req()).await {
            tracing::warn!(%e, "codex: initialize write failed");
        }
        // `model/list` issued before the thread request to reject an unknown
        // `-c model=` up front; while set, that request is part of the handshake.
        let mut validating_model = false;
        let mut catalog_sent = false;
        let mut handshake_failed = false;
        let mut local_id = String::new();
        let mut codex_version: Option<String> = None;
        let mut rollout_path: Option<String> = None;
        let mut next_id = RUN_BASE;
        // request_id (surfaced to TUI) → (rpc_id echoed to codex, decision kind).
        let mut pending_approvals: HashMap<String, (Value, ApprovalKind)> = HashMap::new();
        // Parked `item/tool/requestUserInput` requests: the next user
        // reply answers the oldest one (codex blocks the turn on it) rather than
        // starting a fresh turn.
        let mut pending_questions: VecDeque<(Value, Vec<String>)> = VecDeque::new();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<SessionCommand>(32);
        let mut registered = false;
        // Set when the session is terminated on purpose (daemon shutdown or a
        // Kill command) so the epilogue reports `Killed` rather than treating
        // the non-zero exit as a crash.
        let mut killed = false;
        let mut retry_after_hibernate: Option<SessionCommand> = None;
        let mut active_turn = ActiveTurn::default();
        let mut items = ItemAccumulator::default();
        // In-place model/effort override. A SetModel records it here;
        // every subsequent `turn/start` carries it so codex adopts it as the
        // later default. Left `None` at launch — the spawn-time `-c model=`/
        // `-c model_reasoning_effort=` flags already seed the initial turns.
        let mut override_model: Option<String> = None;
        let mut override_effort: Option<String> = None;
        let mut steer_texts: HashMap<i64, String> = HashMap::new();
        // `model/list` pages accumulated over this session's
        // authenticated connection; the counter bounds `nextCursor` following.
        let mut model_catalog: Vec<CodexModel> = Vec::new();
        let mut model_catalog_pages: usize = 0;
        let mut sweep = tokio::time::interval(Duration::from_secs(1));
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let reexec = crate::selfupdate::reexec_prep();
        let mut reexec_exit = false;

        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => {
                    let _ = child.start_kill();
                    killed = true;
                    break;
                }
                // SIGTERM (not kill) so codex flushes its rollout before the
                // re-exec; the record stays so the new daemon can resume.
                () = reexec.cancelled() => {
                    terminate_child(&mut child, Some(SIGTERM));
                    reexec_exit = true;
                    break;
                }
                _ = sweep.tick() => {
                    let mut handshake_dead = false;
                    for (id, pending) in pending_rpcs.expire(Instant::now()) {
                        tracing::warn!(rpc_id = id, method = %pending.method, "codex: JSON-RPC request timed out");
                        if let Some(command_id) = pending.command_id {
                            let _ = self.events
                                .send(AdapterEvent::CommandResult {
                                    command_id,
                                    ok: false,
                                    error: Some(format!("codex {} timed out", pending.method)),
                                })
                                .await;
                        }
                        if pending.is_handshake() || validating_model {
                            let detail = format!(
                                "codex {} timed out after {}s{}",
                                pending.method,
                                HANDSHAKE_TIMEOUT.as_secs(),
                                stderr_tail(rings)
                            );
                            self.fail_handshake(ack, &detail).await;
                            handshake_dead = true;
                        }
                    }
                    if handshake_dead {
                        handshake_failed = true;
                        let _ = child.start_kill();
                        break;
                    }
                }
                cmd = cmd_rx.recv(), if registered => {
                    match cmd {
                        Some(SessionCommand::Permission { request_id, allow }) => {
                            if let Some((rpc_id, kind)) = pending_approvals.remove(&request_id) {
                                if let Err(e) =
                                    stdin.send(&approval_reply(&rpc_id, kind, allow)).await
                                {
                                    tracing::warn!(%e, "codex: approval write failed; ending session");
                                    break;
                                }
                            } else {
                                tracing::warn!(%request_id, "codex: no pending approval for response");
                            }
                        }
                        Some(SessionCommand::Send { text, command_id }) => {
                            if let Some((rpc_id, question_ids)) = pending_questions.pop_front() {
                                let reply = user_input_reply(&rpc_id, &question_ids, &text);
                                if let Err(e) = stdin.send(&reply).await {
                                    tracing::warn!(%e, "codex: requestUserInput answer write failed; ending session");
                                    break;
                                }
                                if let Some(command_id) = command_id {
                                    let _ = self.events
                                        .send(AdapterEvent::CommandResult { command_id, ok: true, error: None })
                                        .await;
                                }
                                self.events
                                    .send(AdapterEvent::AskResolved { local_id: local_id.clone() })
                                    .await
                                    .ok();
                                continue;
                            }
                            let (req, method) = match prompt_dispatch(&active_turn) {
                                PromptDispatch::Steer { turn_id } => {
                                    steer_texts.insert(next_id, text.clone());
                                    (turn_steer_req(next_id, &local_id, &turn_id, &text, &[]), "turn/steer")
                                }
                                PromptDispatch::Start => {
                                    (
                                        turn_start_req(
                                            next_id,
                                            &local_id,
                                            &text,
                                            &[],
                                            override_model.as_deref(),
                                            override_effort.as_deref(),
                                        ),
                                        "turn/start",
                                    )
                                }
                            };
                            pending_rpcs.insert(next_id, method, command_id, Instant::now() + RPC_TIMEOUT);
                            next_id += 1;
                            // A write failure here means the app-server is gone
                            // — remember the turn and let the epilogue revive
                            // the thread if this was a clean hibernation exit.
                            if let Err(e) = stdin.send(&req).await {
                                tracing::warn!(%e, "codex: turn dispatch write failed; ending session");
                                steer_texts.remove(&(next_id - 1));
                                pending_rpcs.remove(next_id - 1);
                                retry_after_hibernate = Some(SessionCommand::Send { text, command_id });
                                break;
                            }
                        }
                        Some(SessionCommand::Rename { name }) => {
                            if let Err(e) = set_thread_name(
                                &mut stdin,
                                &mut next_id,
                                &mut pending_rpcs,
                                &local_id,
                                &name,
                                &self.events,
                                &self.registry,
                            )
                            .await
                            {
                                tracing::warn!(%e, "codex: thread/name/set write failed; ending session");
                                retry_after_hibernate = Some(SessionCommand::Rename { name });
                                break;
                            }
                        }
                        Some(SessionCommand::Kill { signal }) => {
                            if let Some(turn_id) = active_turn.id() {
                                let req = turn_interrupt_req(next_id, &local_id, turn_id);
                                let _ = stdin.send(&req).await;
                            }
                            terminate_child(&mut child, signal);
                            killed = true;
                            break;
                        }
                        Some(SessionCommand::Interrupt { command_id }) => {
                            // Keep-alive interrupt: abort the turn but
                            // leave the app-server running so the session keeps
                            // going — unlike Kill, we do NOT terminate the child.
                            let Some(turn_id) = active_turn.id() else {
                                if let Some(command_id) = command_id {
                                    let _ = self.events
                                        .send(AdapterEvent::CommandResult {
                                            command_id,
                                            ok: false,
                                            error: Some(NO_TURN_IN_FLIGHT.to_owned()),
                                        })
                                        .await;
                                }
                                continue;
                            };
                            let req = turn_interrupt_req(next_id, &local_id, turn_id);
                            pending_rpcs.insert(next_id, "turn/interrupt", command_id, Instant::now() + RPC_TIMEOUT);
                            next_id += 1;
                            if let Err(e) = stdin.send(&req).await {
                                tracing::warn!(%e, "codex: turn/interrupt write failed; ending session");
                                break;
                            }
                        }
                        Some(SessionCommand::SetModel { model, effort, command_id }) => {
                            record_model_override(
                                &mut override_model,
                                &mut override_effort,
                                model.as_deref(),
                                effort.as_deref(),
                                &local_id,
                                &self.events,
                                &self.registry,
                                command_id,
                            )
                            .await;
                        }
                        Some(SessionCommand::Diagnose { reply }) => {
                            let snapshot = CodexLiveSnapshot {
                                codex_version: codex_version.clone(),
                                pid: child.id(),
                                active_turn_id: active_turn.id().map(str::to_owned),
                                pending_rpc_methods: pending_rpcs.pending_methods(),
                                protocol_errors: rings.protocol_errors_with_shared(),
                                stderr_tail: rings.stderr_tail(),
                                rpc_tail: rings.rpc_tail_with_shared(),
                                rollout_path: rollout_path.clone(),
                                rollout_size_bytes: rollout_path
                                    .as_ref()
                                    .and_then(|p| std::fs::metadata(p).ok())
                                    .map(|m| m.len()),
                            };
                            let _ = reply.send(snapshot).await;
                        }
                        None => break,
                    }
                }
                line = lines.next_line() => {
                    // EOF (`Ok(None)`) or a read error both mean the app-server
                    // is gone; break and let the epilogue classify the exit.
                    let line = match line {
                        Ok(Some(line)) => line,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(%e, "codex: stdout read error; ending session");
                            break;
                        }
                    };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                        tracing::debug!(line = %trimmed, "codex: non-JSON line");
                        continue;
                    };
                    rings.note_rpc("in", &value);
                    if let Some(ev) = turn_lifecycle(&value) {
                        active_turn.apply(&ev);
                        // A spawned child's caller is parked on turn
                        // completion; codex has no state.json to flip, so emit
                        // the done status childwatch classifies on. Scoped to
                        // spawn_key sessions — observed threads keep the
                        // successful-turn-is-ignored behavior.
                        if matches!(ev, TurnLifecycle::Completed { .. })
                            && self.spawn_key.is_some()
                            && !local_id.is_empty()
                        {
                            self.events
                                .send(AdapterEvent::Status {
                                    local_id: local_id.clone(),
                                    tempo: None,
                                    state: Some("done".to_owned()),
                                    detail: None,
                                    activity: Some("success".to_owned()),
                                    name: None,
                                    intent: None,
                                    model: None,
                                    effort: None,
                                    permission_mode: None,
                                    children: Vec::new(),
                                })
                                .await
                                .ok();
                        }
                    }
                    items.note(&value);
                    let value = items.enrich_completed(value);
                    match classify(&local_id, &value) {
                        Incoming::Response { id, value } => {
                            let Some((pending, outcome)) = pending_rpcs.resolve(id, &value) else {
                                tracing::debug!(rpc_id = id, "codex: response for unknown request id");
                                continue;
                            };
                            if let Err(ref e) = outcome {
                                rings.note_protocol_error(&format!("{}: {e}", pending.method));
                            }
                            if outcome.is_ok() && let Some(command_id) = pending.command_id {
                                let _ = self.events
                                    .send(AdapterEvent::CommandResult { command_id, ok: true, error: None })
                                    .await;
                            }
                            match (pending.method.as_str(), outcome) {
                        ("initialize", Ok(_)) => {
                            codex_version = record_codex_version(&value);
                            // Complete the documented handshake before any
                            // thread request: the server treats
                            // `thread/*` sent before `initialized` as premature.
                            stdin.send(&initialized_notification()).await?;
                            if self.cfg.model_catalog && self.cfg.model.is_some() {
                                validating_model = true;
                                pending_rpcs.insert(next_id, "model/list", None, handshake_deadline);
                                stdin.send(&model_list::model_list_req(next_id, None))
                                    .await?;
                                next_id += 1;
                            } else {
                                let (req, method) = self.thread_request();
                                pending_rpcs.insert(ID_THREAD_START, method, None, handshake_deadline);
                                stdin.send(&req).await?;
                            }
                        }
                        ("thread/start" | "thread/resume" | "thread/fork", Ok(result)) => {
                            let Some(info) = thread_info(&result) else {
                                anyhow::bail!("codex thread/start response missing thread id");
                            };
                            local_id.clone_from(&info.thread_id);
                            rollout_path.clone_from(&info.rollout_path);
                            let (parent_local_id, relation) =
                                child_linkage(&self.launch, self.parent_local_id.as_deref());
                            // A `CctuiAgent` call from this session arrives keyed
                            // by the launch key baked into the relay argv; the
                            // thread id it really is only exists now.
                            if let Some(agent_mcp) = &self.agent_mcp {
                                crate::agenttool::bind_session_alias(
                                    agent_mcp.session_key(),
                                    &local_id,
                                );
                                crate::adapters::agent_mcp::remember(&local_id, agent_mcp);
                            }
                            self.events
                                .send(AdapterEvent::SessionStarted {
                                    local_id: local_id.clone(),
                                    meta: SessionMeta {
                                        working_dir: info.cwd.or_else(|| Some(self.cwd.clone())),
                                        parent_local_id,
                                        extra: json!({
                                            "source": "codex-app-server",
                                            "rollout_path": info.rollout_path,
                                            "codex_version": codex_version,
                                            "spawn_key": self.spawn_key,
                                            "relation": relation,
                                        }),
                                    },
                                })
                                .await
                                .ok();
                            let remembered_name = match &self.launch {
                                SessionLaunch::Fresh { name, .. }
                                | SessionLaunch::Fork { name, .. } => name.clone(),
                                SessionLaunch::Resume { .. } => self
                                    .registry
                                    .lock()
                                    .await
                                    .get(&local_id)
                                    .and_then(|r| r.name.clone()),
                            };
                            self.registry.lock().await.insert(
                                local_id.clone(),
                                SessionRecord {
                                    cfg: self.cfg.clone(),
                                    cwd: self.cwd.clone(),
                                    name: remembered_name.clone(),
                                    env: self.env.clone(),
                                    spawn_relay: self.agent_mcp.is_some(),
                                },
                            );
                            super::persist::save(&self.registry).await;
                            self.live.lock().await.insert(local_id.clone(), cmd_tx.clone());
                            registered = true;
                            ack.ok().await;
                            // refresh the account/machine model catalog
                            // over THIS authenticated connection (the gateway
                            // credential is in env), so gateway-only machines get
                            // the current remote list instead of a stale
                            // unauthenticated fallback. Best-effort: a failure is
                            // logged, never fatal to the session.
                            if self.cfg.model_catalog && !catalog_sent {
                                pending_rpcs.insert(
                                    next_id,
                                    "model/list",
                                    None,
                                    Instant::now() + RPC_TIMEOUT,
                                );
                                if let Err(e) =
                                    stdin.send(&model_list::model_list_req(next_id, None))
                                        .await
                                {
                                    tracing::debug!(%e, "codex: model/list write failed");
                                    pending_rpcs.resolve(next_id, &json!({}));
                                }
                                next_id += 1;
                            }
                            // Surface the configured model + reasoning effort so
                            // the session list shows them (claude gets this for
                            // free via state.json; codex has no equivalent feed).
                            // Emit when either is known.
                            let model = self.cfg.model.clone();
                            let effort = self.cfg.reasoning_effort.clone();
                            if model.is_some() || effort.is_some() {
                                self.events
                                    .send(AdapterEvent::Status {
                                        local_id: local_id.clone(),
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
                                    .await
                                    .ok();
                            }

                            let mut end_after_initial = false;
                            match &self.launch {
                                SessionLaunch::Fresh { name, prompt, attachments }
                                | SessionLaunch::Fork { name, prompt, attachments, .. } => {
                                    if let Some(name) = name.as_deref() {
                                        let result = set_thread_name(
                                            &mut stdin,
                                            &mut next_id,
                                            &mut pending_rpcs,
                                            &local_id,
                                            name,
                                            &self.events,
                                            &self.registry,
                                        )
                                        .await;
                                        if let Err(e) = result {
                                            tracing::warn!(%e, "codex: initial thread/name/set failed");
                                            retry_after_hibernate =
                                                Some(SessionCommand::Rename { name: name.to_owned() });
                                            end_after_initial = true;
                                        }
                                    }
                                    // Send the first turn when there is a prompt OR
                                    // staged attachments — an image-only
                                    // spawn carries no prompt text but must still
                                    // reach codex as a `localImage` turn input.
                                    if !end_after_initial
                                        && (prompt.is_some() || !attachments.is_empty())
                                    {
                                        let prompt_text = prompt.as_deref().unwrap_or("");
                                        let req = turn_start_req(
                                            next_id,
                                            &local_id,
                                            prompt_text,
                                            attachments,
                                            override_model.as_deref(),
                                            override_effort.as_deref(),
                                        );
                                        pending_rpcs.insert(
                                            next_id,
                                            "turn/start",
                                            None,
                                            Instant::now() + RPC_TIMEOUT,
                                        );
                                        next_id += 1;
                                        if let Err(e) = stdin.send(&req).await {
                                            tracing::warn!(%e, "codex: initial prompt write failed; ending session");
                                            retry_after_hibernate =
                                                Some(SessionCommand::Send { text: prompt_text.to_owned(), command_id: None });
                                            end_after_initial = true;
                                        }
                                    }
                                }
                                SessionLaunch::Resume { initial_commands, .. } => {
                                    for command in initial_commands.clone() {
                                        match command {
                                            SessionCommand::Send { text, command_id } => {
                                                let req = turn_start_req(
                                                    next_id,
                                                    &local_id,
                                                    &text,
                                                    &[],
                                                    override_model.as_deref(),
                                                    override_effort.as_deref(),
                                                );
                                                pending_rpcs.insert(
                                                    next_id,
                                                    "turn/start",
                                                    command_id,
                                                    Instant::now() + RPC_TIMEOUT,
                                                );
                                                next_id += 1;
                                                if let Err(e) = stdin.send(&req).await {
                                                    tracing::warn!(%e, "codex: resumed turn/start write failed");
                                                    pending_rpcs.remove(next_id - 1);
                                                    retry_after_hibernate =
                                                        Some(SessionCommand::Send { text, command_id });
                                                    end_after_initial = true;
                                                    break;
                                                }
                                            }
                                            SessionCommand::Rename { name } => {
                                                if let Err(e) = set_thread_name(
                                                    &mut stdin,
                                                    &mut next_id,
                                                    &mut pending_rpcs,
                                                    &local_id,
                                                    &name,
                                                    &self.events,
                                                    &self.registry,
                                                )
                                                .await
                                                {
                                                    tracing::warn!(%e, "codex: resumed thread/name/set write failed");
                                                    retry_after_hibernate =
                                                        Some(SessionCommand::Rename { name });
                                                    end_after_initial = true;
                                                    break;
                                                }
                                            }
                                            SessionCommand::SetModel { model, effort, command_id } => {
                                                record_model_override(
                                                    &mut override_model,
                                                    &mut override_effort,
                                                    model.as_deref(),
                                                    effort.as_deref(),
                                                    &local_id,
                                                    &self.events,
                                                    &self.registry,
                                                    command_id,
                                                )
                                                .await;
                                            }
                                            other => {
                                                tracing::warn!(?other, "codex: ignoring non-resumable initial command");
                                            }
                                        }
                                    }
                                }
                            }
                            if end_after_initial {
                                break;
                            }
                        }
                        (method, Err(err)) if pending.is_handshake() => {
                            tracing::error!(%err, %method, "codex: handshake request failed; ending session");
                            let detail = format!("codex {method}: {err}{}", stderr_tail(rings));
                            self.fail_handshake(ack, &detail).await;
                            handshake_failed = true;
                            let _ = child.start_kill();
                            break;
                        }
                        ("model/list", Err(err)) if validating_model => {
                            // The catalog is best-effort; codex still rejects a bad
                            // model itself on the first turn.
                            tracing::debug!(%err, "codex: pre-start model/list failed; skipping model check");
                            validating_model = false;
                            model_catalog.clear();
                            let (req, method) = self.thread_request();
                            pending_rpcs.insert(ID_THREAD_START, method, None, handshake_deadline);
                            stdin.send(&req).await?;
                        }
                        ("model/list", Ok(result)) if validating_model => {
                            model_catalog.extend(model_list::parse_model_list(&result));
                            model_catalog_pages += 1;
                            if let model_list::PageStep::Next { cursor } =
                                model_list::page_step(model_catalog_pages, &result)
                            {
                                pending_rpcs.insert(next_id, "model/list", None, handshake_deadline);
                                stdin.send(
                                    &model_list::model_list_req(next_id, Some(&cursor)),
                                )
                                .await?;
                                next_id += 1;
                                continue;
                            }
                            validating_model = false;
                            let catalog =
                                CodexModelCatalog { models: std::mem::take(&mut model_catalog) };
                            if let Some(warning) = unknown_model(self.cfg.model.as_deref(), &catalog)
                            {
                                tracing::warn!(%warning, "codex: spawning anyway");
                            }
                            catalog_sent = true;
                            self.events.send(AdapterEvent::CodexModels { catalog }).await.ok();
                            let (req, method) = self.thread_request();
                            pending_rpcs.insert(ID_THREAD_START, method, None, handshake_deadline);
                            stdin.send(&req).await?;
                        }
                        ("model/list", Ok(result)) => {
                            model_catalog.extend(model_list::parse_model_list(&result));
                            model_catalog_pages += 1;
                            match model_list::page_step(model_catalog_pages, &result) {
                                model_list::PageStep::Next { cursor } => {
                                    pending_rpcs.insert(
                                        next_id,
                                        "model/list",
                                        None,
                                        Instant::now() + RPC_TIMEOUT,
                                    );
                                    if let Err(e) = stdin.send(
                                        &model_list::model_list_req(next_id, Some(&cursor)),
                                    )
                                    .await
                                    {
                                        tracing::debug!(%e, "codex: model/list page write failed");
                                        pending_rpcs.resolve(next_id, &json!({}));
                                    }
                                    next_id += 1;
                                }
                                model_list::PageStep::Done => {
                                    let catalog = CodexModelCatalog {
                                        models: std::mem::take(&mut model_catalog),
                                    };
                                    self.events
                                        .send(AdapterEvent::CodexModels { catalog })
                                        .await
                                        .ok();
                                }
                            }
                        }
                        ("model/list", Err(err)) => {
                            tracing::debug!(%err, "codex: model/list refresh failed");
                            model_catalog.clear();
                        }
                        ("turn/steer", Ok(_)) => {
                            steer_texts.remove(&id);
                        }
                        ("turn/steer", Err(err)) => {
                            let text = steer_texts.remove(&id);
                            match (steer_recovery(&err), text) {
                                (SteerRecovery::FallbackToStart, Some(text)) => {
                                    active_turn.clear();
                                    tracing::info!(%err, "codex: turn/steer stale; falling back to turn/start");
                                    let req = turn_start_req(
                                        next_id,
                                        &local_id,
                                        &text,
                                        &[],
                                        override_model.as_deref(),
                                        override_effort.as_deref(),
                                    );
                                    pending_rpcs.insert(next_id, "turn/start", pending.command_id, Instant::now() + RPC_TIMEOUT);
                                    next_id += 1;
                                    if let Err(e) = stdin.send(&req).await {
                                        tracing::warn!(%e, "codex: turn/start fallback write failed; ending session");
                                        pending_rpcs.remove(next_id - 1);
                                        retry_after_hibernate = Some(SessionCommand::Send { text, command_id: pending.command_id });
                                        break;
                                    }
                                }
                                (recovery, _) => {
                                    tracing::warn!(%err, ?recovery, "codex: turn/steer rejected");
                                    if let Some(command_id) = pending.command_id {
                                        let _ = self.events
                                            .send(AdapterEvent::CommandResult {
                                                command_id,
                                                ok: false,
                                                error: Some(err.clone()),
                                            })
                                            .await;
                                    }
                                    self.events
                                        .send(AdapterEvent::Status {
                                            local_id: local_id.clone(),
                                            tempo: None,
                                            state: Some("failed".to_owned()),
                                            detail: Some(err),
                                            activity: Some("failure".to_owned()),
                                            name: None,
                                            intent: None,
                                            model: None,
                                            effort: None,
                                            permission_mode: None,
                                            children: vec![],
                                        })
                                        .await
                                        .ok();
                                }
                            }
                        }
                        (method, Err(err)) => {
                            tracing::warn!(%err, %method, "codex: JSON-RPC request failed");
                            if let Some(command_id) = pending.command_id {
                                let _ = self.events
                                    .send(AdapterEvent::CommandResult {
                                        command_id,
                                        ok: false,
                                        error: Some(err.clone()),
                                    })
                                    .await;
                            }
                            if method == "turn/start" {
                                self.events
                                    .send(AdapterEvent::Status {
                                        local_id: local_id.clone(),
                                        tempo: None,
                                        state: Some("failed".to_owned()),
                                        detail: Some(err),
                                        activity: Some("failure".to_owned()),
                                        name: None,
                                        intent: None,
                                        model: None,
                                        effort: None,
                                        permission_mode: None,
                                        children: vec![],
                                    })
                                    .await
                                    .ok();
                            }
                        }
                        (_, Ok(_)) => {}
                            }
                        }
                        Incoming::Approval { rpc_id, request_id, tool, kind, input } => {
                            pending_approvals.insert(request_id.clone(), (rpc_id, kind));
                            self.events
                                .send(AdapterEvent::PermissionRequest {
                                    local_id: local_id.clone(),
                                    request_id,
                                    tool,
                                    input,
                                })
                                .await
                                .ok();
                        }
                        Incoming::Question { rpc_id, question, questions, question_ids } => {
                            pending_questions.push_back((rpc_id, question_ids));
                            self.events
                                .send(AdapterEvent::AskQuestion {
                                    local_id: local_id.clone(),
                                    question,
                                    questions: Some(questions),
                                    preamble: None,
                                })
                                .await
                                .ok();
                        }
                        Incoming::Decline { reply } => {
                            if let Err(e) = stdin.send(&reply).await {
                                tracing::warn!(%e, "codex: decline write failed; ending session");
                                break;
                            }
                        }
                        Incoming::Event(evt) => {
                            self.events.send(evt).await.ok();
                        }
                        Incoming::Traced { method, reason } => {
                            tracing::trace!(%method, reason, "codex notification consumed out-of-band");
                        }
                        Incoming::Unhandled { method, event } => {
                            tracing::warn!(%method, "unhandled codex notification");
                            rings.note_protocol_error(&format!(
                                "unhandled codex notification {method}"
                            ));
                            self.events.send(event).await.ok();
                        }
                        Incoming::Ignored => {}
                    }
                }
            }
        }

        // The pump has broken: drop the live sender NOW so new commands take
        // the Resume path instead of landing in this dead channel's buffer,
        // then drain whatever was already buffered.
        if !local_id.is_empty() {
            self.live.lock().await.remove(&local_id);
        }
        cmd_rx.close();
        let mut drained: Vec<SessionCommand> = Vec::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            drained.push(cmd);
        }

        for (id, pending) in pending_rpcs.drain() {
            tracing::warn!(rpc_id = id, method = %pending.method, "codex: cancelling pending request — app-server gone");
            if let Some(command_id) = pending.command_id {
                let _ = self
                    .events
                    .send(AdapterEvent::CommandResult {
                        command_id,
                        ok: false,
                        error: Some(format!(
                            "codex {}: app-server exited before responding",
                            pending.method
                        )),
                    })
                    .await;
            }
        }

        // Paths that hand a request to a retry (`retry_after_hibernate`) must
        // `pending_rpcs.remove` it first, or it is failed here as well.
        for (_, pending) in pending_rpcs.drain() {
            if let Some(command_id) = pending.command_id {
                self.events
                    .send(AdapterEvent::CommandResult {
                        command_id,
                        ok: false,
                        error: Some(format!(
                            "codex app-server exited before {} was acknowledged",
                            pending.method
                        )),
                    })
                    .await
                    .ok();
            }
        }

        // Reap the child and classify why the session ended. An abnormal exit
        // that we did not request is surfaced as `Crashed` with the captured
        // stderr tail — the diagnostic for the macOS "randomly dies" report.
        let status = child.wait().await;
        // Let the drain catch codex's final lines before any tail is read.
        if let Some(drain) = stderr_drain {
            let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
        }
        if local_id.is_empty() {
            if reexec_exit || handshake_failed {
                return Ok(());
            }
            let exit = status.map_or_else(|e| e.to_string(), |s| s.to_string());
            anyhow::bail!("codex app-server exited ({exit}) before the thread was started");
        }
        if reexec_exit {
            return Ok(());
        }
        let (mut retry, dropped, drained_kill) = partition_drained(drained);
        let reason = if killed || drained_kill {
            Some(EndReason::Killed)
        } else {
            match status {
                Ok(s) if s.success() => None,
                Ok(s) => Some(EndReason::Crashed {
                    detail: format!("codex app-server exited ({s}){}", stderr_tail(rings)),
                }),
                Err(e) => Some(EndReason::Crashed {
                    detail: format!("codex app-server wait failed: {e}"),
                }),
            }
        };
        if let Some(reason) = reason {
            if let EndReason::Crashed { detail } = &reason {
                tracing::error!(%detail, "codex app-server session crashed");
            }
            // A crash keeps the durable record: `thread/resume` still works,
            // so the next command revives the thread instead of going Missing.
            if removes_record(&reason) {
                self.registry.lock().await.remove(&local_id);
                super::persist::save(&self.registry).await;
            }
            for cmd in retry.into_iter().chain(dropped) {
                self.fail_dropped_command(&local_id, &cmd).await;
            }
            self.events
                .send(AdapterEvent::SessionEnded { local_id: local_id.clone(), reason })
                .await
                .ok();
        } else {
            self.events
                .send(AdapterEvent::Status {
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
                .await
                .ok();
            for cmd in dropped {
                self.fail_dropped_command(&local_id, &cmd).await;
            }
            if let Some(command) = retry_after_hibernate {
                retry.insert(0, command);
            }
            if !retry.is_empty()
                && let Some(record) = self.registry.lock().await.get(&local_id).cloned()
            {
                spawn_resumed_session(
                    record,
                    &local_id,
                    retry,
                    self.events.clone(),
                    self.live.clone(),
                    self.registry.clone(),
                    self.shutdown.clone(),
                );
            }
        }
        Ok(())
    }

    /// Fail a command that was sitting in the dead session's channel buffer,
    /// loudly: `CommandResult` when it carries an id, a failed `Status` for a
    /// user message so the drop is visible in the webui.
    async fn fail_dropped_command(&self, local_id: &str, cmd: &SessionCommand) {
        if let Some(command_id) = cmd.command_id() {
            self.events
                .send(AdapterEvent::CommandResult {
                    command_id,
                    ok: false,
                    error: Some("codex app-server exited before the command was delivered".into()),
                })
                .await
                .ok();
        }
        if matches!(cmd, SessionCommand::Send { .. }) {
            self.events
                .send(AdapterEvent::Status {
                    local_id: local_id.to_owned(),
                    tempo: None,
                    state: Some("failed".to_owned()),
                    detail: Some(
                        "message dropped: codex app-server exited before it was delivered"
                            .to_owned(),
                    ),
                    activity: Some("failure".to_owned()),
                    name: None,
                    intent: None,
                    model: None,
                    effort: None,
                    permission_mode: None,
                    children: Vec::new(),
                })
                .await
                .ok();
        }
        tracing::warn!(%local_id, ?cmd, "codex: dropping buffered command — app-server gone");
    }
}

/// Only an explicit kill removes the durable record; a crash keeps it
/// resumable.
const fn removes_record(reason: &EndReason) -> bool {
    matches!(reason, EndReason::Killed)
}

/// Split commands drained from a dead session's channel: resumable ones to
/// retry on the revived thread, the rest to fail visibly. A buffered `Kill`
/// wins over hibernation.
fn partition_drained(
    drained: Vec<SessionCommand>,
) -> (Vec<SessionCommand>, Vec<SessionCommand>, bool) {
    let mut retry = Vec::new();
    let mut dropped = Vec::new();
    let mut killed = false;
    for cmd in drained {
        if matches!(cmd, SessionCommand::Kill { .. }) {
            killed = true;
        } else if cmd.is_resumable() {
            retry.push(cmd);
        } else {
            dropped.push(cmd);
        }
    }
    (retry, dropped, killed)
}

#[allow(clippy::too_many_arguments)]
async fn set_thread_name(
    stdin: &mut RpcStdin,
    next_id: &mut i64,
    pending_rpcs: &mut PendingRpcs,
    thread_id: &str,
    name: &str,
    events: &mpsc::Sender<AdapterEvent>,
    registry: &SessionRegistry,
) -> Result<()> {
    pending_rpcs.insert(*next_id, "thread/name/set", None, Instant::now() + RPC_TIMEOUT);
    stdin.send(&thread_name_set_req(*next_id, thread_id, name)).await?;
    *next_id += 1;
    if let Some(record) = registry.lock().await.get_mut(thread_id) {
        record.name = Some(name.to_owned());
    }
    super::persist::save(registry).await;
    events
        .send(AdapterEvent::Status {
            local_id: thread_id.to_owned(),
            tempo: None,
            state: None,
            detail: None,
            activity: None,
            name: Some(name.to_owned()),
            intent: None,
            model: None,
            effort: None,
            permission_mode: None,
            children: Vec::new(),
        })
        .await
        .ok();
    Ok(())
}

/// Record an in-place model/effort change. Stashes the
/// override in `override_model`/`override_effort` (carried on the next
/// `turn/start`, which codex promotes to the later default — the stable path,
/// vs the `experimentalApi`-gated `thread/settings/update` codex 0.144.1
/// rejects) and folds it into the durable `SessionRecord` cfg so a resume
/// relaunches with matching `-c model=`/`-c model_reasoning_effort=` flags.
/// No app-server round-trip can reject it, so the chip (`Status`) and the
/// `command_id` ack (`CommandResult`) are truthful the moment they fire here.
#[allow(clippy::too_many_arguments)]
async fn record_model_override(
    override_model: &mut Option<String>,
    override_effort: &mut Option<String>,
    model: Option<&str>,
    effort: Option<&str>,
    thread_id: &str,
    events: &mpsc::Sender<AdapterEvent>,
    registry: &SessionRegistry,
    command_id: Option<Uuid>,
) {
    if let Some(model) = model {
        *override_model = Some(model.to_owned());
    }
    if let Some(effort) = effort {
        *override_effort = Some(effort.to_owned());
    }
    if let Some(record) = registry.lock().await.get_mut(thread_id) {
        if let Some(model) = model {
            record.cfg.model = Some(model.to_owned());
        }
        if let Some(effort) = effort {
            record.cfg.reasoning_effort = Some(effort.to_owned());
        }
    }
    super::persist::save(registry).await;
    events
        .send(AdapterEvent::Status {
            local_id: thread_id.to_owned(),
            tempo: None,
            state: None,
            detail: None,
            activity: None,
            name: None,
            intent: None,
            model: model.map(str::to_owned),
            effort: effort.map(str::to_owned),
            permission_mode: None,
            children: Vec::new(),
        })
        .await
        .ok();
    if let Some(command_id) = command_id {
        events.send(AdapterEvent::CommandResult { command_id, ok: true, error: None }).await.ok();
    }
}

/// The `(parent_local_id, relation)` a freshly started thread reports, so the
/// server resolves `parent_id`. Relation `"subagent"` — not `"fork"` — is what
/// the webui nests, so a `CctuiAgent` child must not be labelled a fork.
#[must_use]
fn child_linkage(
    launch: &SessionLaunch,
    parent_local_id: Option<&str>,
) -> (Option<String>, Option<&'static str>) {
    match launch {
        SessionLaunch::Fork { parent_thread_id, .. } => {
            (Some(parent_thread_id.clone()), Some("fork"))
        }
        _ => (parent_local_id.map(str::to_owned), parent_local_id.map(|_| "subagent")),
    }
}

/// The `CctuiAgent` relay a resume re-declares: this daemon's remembered launch
/// decision, else the persisted one, which is all a rediscovered thread has.
#[must_use]
pub fn resume_relay(
    thread_id: &str,
    record: &SessionRecord,
) -> Option<crate::adapters::agent_mcp::AgentMcp> {
    crate::adapters::agent_mcp::recall(thread_id).or_else(|| {
        record
            .spawn_relay
            .then(|| crate::adapters::agent_mcp::AgentMcp::for_session(thread_id))
            .flatten()
    })
}

pub fn spawn_resumed_session(
    record: SessionRecord,
    thread_id: &str,
    commands: Vec<SessionCommand>,
    events: mpsc::Sender<AdapterEvent>,
    live: LiveSessionRegistry,
    registry: SessionRegistry,
    shutdown: CancellationToken,
) {
    let commands: Vec<SessionCommand> = commands
        .into_iter()
        .filter(|command| {
            let ok = command.is_resumable();
            if !ok {
                tracing::warn!(%thread_id, ?command, "codex: command is not resumable");
            }
            ok
        })
        .collect();
    if commands.is_empty() {
        return;
    }
    let relay = resume_relay(thread_id, &record);
    let session = CodexSession::new_resume(
        record.cfg,
        record.cwd,
        record.env,
        thread_id.to_owned(),
        commands,
        events,
        live,
        registry,
        shutdown,
    )
    .with_agent_mcp(relay);
    tokio::spawn(async move {
        if let Err(err) = session.run().await {
            tracing::error!(%err, "codex resumed app-server session ended in error");
        }
    });
}

/// `Some(warning)` when `model` is set and absent from the catalog (by id or
/// underlying slug). Advisory only: a catalog can be stale or a machine-local
/// fallback, and there is no way to un-stick it from the UI, so it must never
/// block a spawn — codex rejects a genuinely bad model itself. An empty catalog
/// cannot vouch for anything and passes.
fn unknown_model(model: Option<&str>, catalog: &CodexModelCatalog) -> Option<String> {
    let model = model?;
    if catalog.models.is_empty() || catalog.models.iter().any(|m| m.id == model || m.model == model)
    {
        return None;
    }
    let mut available: Vec<&str> =
        catalog.models.iter().filter(|m| !m.hidden).map(|m| m.id.as_str()).collect();
    if available.is_empty() {
        available = catalog.models.iter().map(|m| m.id.as_str()).collect();
    }
    Some(format!("unknown model {model}; available: {}", available.join(", ")))
}

/// Format the retained stderr tail for inclusion in a crash detail. Empty
/// when nothing was captured.
fn stderr_tail(rings: &DiagnoseRings) -> String {
    let lines: Vec<String> = rings.stderr_tail().into_iter().map(|l| l.line).collect();
    if lines.is_empty() { String::new() } else { format!("; last stderr:\n{}", lines.join("\n")) }
}

/// The per-session app-server child's stdio pipes.
pub const TRANSPORT_STDIO: &str = "stdio";
/// The process-wide `codex app-server daemon` control socket.
pub const TRANSPORT_SHARED: &str = "shared";

type RingScrub = std::sync::RwLock<Arc<CompiledPatterns>>;

/// The effective detector set for the rings: the builtins, plus whatever custom
/// patterns the server last synced (see [`set_ring_scrub`]). Builtins stay on
/// unconditionally — a session's tool output is echoed into these rings, so
/// "scrubbing disabled" must not mean "tokens in the diagnose report".
fn ring_scrub_cell() -> &'static RingScrub {
    static SCRUB: OnceLock<RingScrub> = OnceLock::new();
    SCRUB.get_or_init(|| {
        std::sync::RwLock::new(Arc::new(redact::compile(true, &[], &cctui_crypto::vault_key())))
    })
}

/// Install the user-configured scrub patterns on the rings. Called by the
/// supervisor whenever the server syncs a `SecretScrubConfig`; the rings live
/// in the driver and have no other route to them.
pub fn set_ring_scrub(user: &[(String, String)]) {
    let compiled = Arc::new(redact::compile(true, user, &cctui_crypto::vault_key()));
    if let Ok(mut guard) = ring_scrub_cell().write() {
        *guard = compiled;
    }
}

fn ring_scrub() -> Arc<CompiledPatterns> {
    ring_scrub_cell().read().map_or_else(|e| Arc::clone(&e.into_inner()), |g| Arc::clone(&g))
}

fn redact_text(text: &str) -> String {
    let mut value = Value::String(text.to_owned());
    redact::redact_json(&mut value, &ring_scrub());
    match value {
        Value::String(s) => s,
        _ => text.to_owned(),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

/// Bounded, redacted observability rings for the diagnose report.
///
/// Every producer sits on the JSON-RPC write path or the stdout read loop, so
/// the locks are `try_lock` only: a contended ring drops the entry rather than
/// stalling the session.
#[derive(Debug)]
pub struct DiagnoseRings {
    /// Stamped onto every entry so a reader can tell "no frames on the shared
    /// connection" from "no frames at all".
    transport: &'static str,
    stderr: StdMutex<VecDeque<CodexStderrLine>>,
    rpc: StdMutex<VecDeque<CodexRpcFrame>>,
    errors: StdMutex<VecDeque<CodexProtocolError>>,
}

impl Default for DiagnoseRings {
    fn default() -> Self {
        Self::new(TRANSPORT_STDIO)
    }
}

/// The rings for the shared `codex app-server daemon` connection. That socket
/// is process-wide, not per-session, so its frames are collected once here and
/// merged into every session's diagnose snapshot.
pub fn shared_rings() -> &'static Arc<DiagnoseRings> {
    static RINGS: OnceLock<Arc<DiagnoseRings>> = OnceLock::new();
    RINGS.get_or_init(|| Arc::new(DiagnoseRings::new(TRANSPORT_SHARED)))
}

/// Merge a session's stdio tail with the shared connection's, oldest first.
/// Neither side is truncated against the other: the whole point of the tagging
/// is that a flood on one transport must not hide the silence of the other.
fn merge_by_ts<T: Clone, F: Fn(&T) -> i64>(a: Vec<T>, b: Vec<T>, ts: F) -> Vec<T> {
    let mut out = a;
    out.extend(b);
    out.sort_by_key(|e| ts(e));
    out
}

impl DiagnoseRings {
    #[must_use]
    pub fn new(transport: &'static str) -> Self {
        Self {
            transport,
            stderr: StdMutex::default(),
            rpc: StdMutex::default(),
            errors: StdMutex::default(),
        }
    }

    fn push<T>(ring: &StdMutex<VecDeque<T>>, cap: usize, item: T) {
        let Ok(mut guard) = ring.try_lock() else { return };
        while guard.len() >= cap {
            guard.pop_front();
        }
        guard.push_back(item);
    }

    fn snapshot<T: Clone>(ring: &StdMutex<VecDeque<T>>) -> Vec<T> {
        ring.try_lock().map(|g| g.iter().cloned().collect()).unwrap_or_default()
    }

    fn note_stderr(&self, line: &str) {
        Self::push(
            &self.stderr,
            STDERR_RING,
            CodexStderrLine { ts_ms: now_ms(), line: redact_text(line) },
        );
    }

    pub fn note_rpc(&self, direction: &'static str, value: &Value) {
        let label = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| value.get("id").map(ToString::to_string))
            .unwrap_or_else(|| "frame".to_owned());
        let raw = value.to_string();
        let json = truncate_chars(&redact_text(&truncate_chars(&raw, RPC_SCAN_MAX)), RPC_FRAME_MAX);
        Self::push(
            &self.rpc,
            RPC_RING,
            CodexRpcFrame {
                ts_ms: now_ms(),
                direction: direction.to_owned(),
                label: redact_text(&label),
                json,
                transport: self.transport.to_owned(),
            },
        );
    }

    pub fn note_protocol_error(&self, message: &str) {
        Self::push(
            &self.errors,
            PROTOCOL_ERROR_RING,
            CodexProtocolError {
                ts_ms: now_ms(),
                message: redact_text(message),
                transport: self.transport.to_owned(),
            },
        );
    }

    fn stderr_tail(&self) -> Vec<CodexStderrLine> {
        Self::snapshot(&self.stderr)
    }

    pub fn rpc_tail(&self) -> Vec<CodexRpcFrame> {
        Self::snapshot(&self.rpc)
    }

    pub fn protocol_errors(&self) -> Vec<CodexProtocolError> {
        Self::snapshot(&self.errors)
    }

    /// This ring's frames plus the shared connection's, oldest first.
    fn rpc_tail_with_shared(&self) -> Vec<CodexRpcFrame> {
        merge_by_ts(self.rpc_tail(), shared_rings().rpc_tail(), |f| f.ts_ms)
    }

    fn protocol_errors_with_shared(&self) -> Vec<CodexProtocolError> {
        merge_by_ts(self.protocol_errors(), shared_rings().protocol_errors(), |e| e.ts_ms)
    }
}

struct RpcStdin {
    inner: tokio::process::ChildStdin,
    rings: Arc<DiagnoseRings>,
}

impl RpcStdin {
    async fn send(&mut self, v: &Value) -> Result<()> {
        self.rings.note_rpc("out", v);
        write_json(&mut self.inner, v).await
    }
}

/// SIGTERM, per POSIX. The control-plane `Kill { signal }` uses raw signal
/// numbers; 15 is the one graceful case we special-case.
const SIGTERM: i32 = 15;

/// Terminate the child with the requested signal. `Some(15)` (SIGTERM)
/// gives codex a chance to flush its rollout file; anything else (incl.
/// `None`) is an immediate SIGKILL via tokio's `start_kill`.
fn terminate_child(child: &mut tokio::process::Child, signal: Option<i32>) {
    if signal == Some(SIGTERM)
        && let Some(pid) =
            child.id().and_then(|p| i32::try_from(p).ok()).and_then(rustix::process::Pid::from_raw)
    {
        // A reaped pid just yields ESRCH, which we ignore.
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        return;
    }
    let _ = child.start_kill();
}

async fn write_json<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_launch() -> SessionLaunch {
        SessionLaunch::Fresh { prompt: None, name: None, attachments: Vec::new() }
    }

    #[test]
    fn a_cctui_agent_child_reports_its_spawning_parent_as_a_subagent() {
        let (parent, relation) = child_linkage(&fresh_launch(), Some("parent-thread-1"));
        assert_eq!(parent.as_deref(), Some("parent-thread-1"));
        assert_eq!(
            relation,
            Some("subagent"),
            "the webui nests on \"subagent\"; anything else orphans the child"
        );
    }

    #[test]
    fn a_parentless_thread_reports_no_linkage() {
        assert_eq!(child_linkage(&fresh_launch(), None), (None, None));
    }

    #[test]
    fn a_fork_links_to_its_parent_thread_as_a_fork() {
        let launch = SessionLaunch::Fork {
            parent_thread_id: "thread-7".to_owned(),
            prompt: None,
            name: None,
            attachments: Vec::new(),
        };
        assert_eq!(
            child_linkage(&launch, Some("ignored")),
            (Some("thread-7".to_owned()), Some("fork"))
        );
    }

    #[test]
    fn a_codex_session_with_a_capability_launches_the_agent_relay() {
        let cap = cctui_proto::api::SpawnCapability {
            adapters: vec!["codex".to_owned()],
            ..Default::default()
        };
        let with = crate::adapters::agent_mcp::AgentMcp::for_capability("key-1", Some(&cap));
        assert!(with.is_some(), "a granted capability must register the MCP tool");
        let keys: Vec<String> =
            with.unwrap().codex_config_overrides().into_iter().map(|(k, _)| k).collect();
        assert!(
            keys.iter().any(|k| k.starts_with("mcp_servers.")),
            "the relay must ride codex `-c mcp_servers.…`; got {keys:?}"
        );
        assert!(
            crate::adapters::agent_mcp::AgentMcp::for_capability("key-1", None).is_none(),
            "a session with no capability must not see the tool at all"
        );
    }

    fn relay_record(spawn_relay: bool) -> SessionRecord {
        SessionRecord {
            cfg: AppServerConfig::default(),
            cwd: "/repo".to_owned(),
            name: Some("worker".to_owned()),
            env: std::iter::once(("OPENAI_API_KEY".to_owned(), "sk-live".to_owned())).collect(),
            spawn_relay,
        }
    }

    /// The restart path: a relay session's record goes through the real on-disk
    /// snapshot, a fresh daemon merges it into an empty registry with nothing
    /// remembered in-process, and the resume must still declare the tool.
    #[tokio::test]
    async fn a_rediscovered_thread_keeps_the_spawn_tool_across_a_daemon_restart() {
        let thread_id = "thread_0199restartrelay";
        let cap = cctui_proto::api::SpawnCapability {
            adapters: vec!["codex".to_owned()],
            ..Default::default()
        };
        let launched =
            crate::adapters::agent_mcp::AgentMcp::for_capability("launch-key-r", Some(&cap));
        assert!(
            launched.is_some(),
            "the launch must have had the relay for this test to mean anything"
        );

        let mut before = std::collections::HashMap::new();
        before.insert(thread_id.to_owned(), relay_record(launched.is_some()));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("codex-sessions.json");
        super::super::persist::save_to(&path, &before).expect("snapshot written");

        let restarted = SessionRegistry::default();
        let restored =
            super::super::persist::merge(&restarted, super::super::persist::load_from(&path)).await;
        assert_eq!(restored, 1, "the thread is rediscovered from the snapshot");
        assert!(
            crate::adapters::agent_mcp::recall(thread_id).is_none(),
            "a restart must leave nothing remembered in-process, or this proves nothing"
        );

        let record = restarted.lock().await.get(thread_id).cloned().expect("record restored");
        let relay = resume_relay(thread_id, &record)
            .expect("a rediscovered thread that had the relay must still get it");
        assert_eq!(relay.session_key(), thread_id, "the relay keys onto the real thread id");
        let keys: Vec<String> =
            relay.codex_config_overrides().into_iter().map(|(k, _)| k).collect();
        assert!(
            keys.iter().any(|k| k.starts_with("mcp_servers.")),
            "the resumed thread must re-declare the relay; got {keys:?}"
        );
        assert!(
            !record.env.contains_key("OPENAI_API_KEY"),
            "the credential still must not survive the restart"
        );
    }

    #[tokio::test]
    async fn a_thread_that_never_had_the_relay_does_not_gain_it_on_resume() {
        let mut before = std::collections::HashMap::new();
        before.insert("thread_0199norelay".to_owned(), relay_record(false));
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("codex-sessions.json");
        super::super::persist::save_to(&path, &before).expect("snapshot written");
        let record = super::super::persist::load_from(&path)
            .remove("thread_0199norelay")
            .expect("record restored");
        assert!(
            resume_relay("thread_0199norelay", &record).is_none(),
            "fail-closed: a session the server never granted spawn rights must not see the tool"
        );
    }

    #[test]
    fn diagnose_rings_redact_tool_output_secrets() {
        let token = "ghp_0123456789abcdefghijABCDEFGHIJ0123";
        let rings = DiagnoseRings::default();
        rings.note_rpc(
            "in",
            &json!({
                "method": "item/completed",
                "params": { "item": { "output": format!("export GITHUB_TOKEN={token}") } }
            }),
        );
        rings.note_stderr(&format!("tool stdout: {token}"));
        rings.note_protocol_error(&format!("turn/start: upstream rejected {token}"));

        let rpc = rings.rpc_tail();
        let stderr = rings.stderr_tail();
        let errors = rings.protocol_errors();
        assert!(!rpc[0].json.contains(token), "{}", rpc[0].json);
        assert!(rpc[0].json.contains("[REDACTED:github_token"), "{}", rpc[0].json);
        assert_eq!(rpc[0].label, "item/completed");
        assert!(!stderr[0].line.contains(token), "{}", stderr[0].line);
        assert!(!errors[0].message.contains(token), "{}", errors[0].message);
    }

    /// Without the tag a flood of stdio frames makes an entirely dead shared
    /// connection look healthy — the blind spot CCT-966 opened.
    #[test]
    fn ring_entries_carry_the_transport_that_produced_them() {
        let stdio = DiagnoseRings::default();
        let shared = DiagnoseRings::new(TRANSPORT_SHARED);
        stdio.note_rpc("out", &json!({"method": "turn/start"}));
        shared.note_rpc("out", &json!({"method": "thread/list"}));
        stdio.note_protocol_error("turn/start: boom");
        shared.note_protocol_error("connection dropped before request 4 was answered");

        assert_eq!(stdio.rpc_tail()[0].transport, "stdio");
        assert_eq!(shared.rpc_tail()[0].transport, "shared");
        assert_eq!(stdio.protocol_errors()[0].transport, "stdio");
        assert_eq!(shared.protocol_errors()[0].transport, "shared");
    }

    #[test]
    fn a_session_snapshot_merges_the_shared_connection_tail_oldest_first() {
        let stdio = DiagnoseRings::default();
        shared_rings().note_rpc("in", &json!({"method": "thread/list"}));
        stdio.note_rpc("out", &json!({"method": "turn/start"}));

        let merged = stdio.rpc_tail_with_shared();
        assert!(merged.iter().any(|f| f.transport == "shared"), "{merged:?}");
        assert!(merged.iter().any(|f| f.transport == "stdio"), "{merged:?}");
        assert!(merged.windows(2).all(|w| w[0].ts_ms <= w[1].ts_ms), "{merged:?}");
    }

    /// The builtins alone would let a user-configured secret through; the
    /// rings echo tool output, so this is a live leak path.
    #[test]
    fn diagnose_rings_apply_user_configured_scrub_patterns() {
        set_ring_scrub(&[("acme_key".to_owned(), "ACME-[0-9]{6}".to_owned())]);
        let rings = DiagnoseRings::default();
        rings.note_rpc(
            "in",
            &json!({"method": "item/completed", "params": {"output": "token ACME-424242 ok"}}),
        );
        rings.note_stderr("leaked ACME-424242 to stderr");

        let frame = &rings.rpc_tail()[0];
        assert!(!frame.json.contains("ACME-424242"), "{}", frame.json);
        assert!(frame.json.contains("[REDACTED:acme_key"), "{}", frame.json);
        assert!(!rings.stderr_tail()[0].line.contains("ACME-424242"));

        set_ring_scrub(&[]);
    }

    #[test]
    fn diagnose_rings_are_bounded_and_frames_truncated() {
        let rings = DiagnoseRings::default();
        for i in 0..(RPC_RING + 10) {
            rings.note_rpc("out", &json!({ "id": i, "method": "turn/start" }));
        }
        for i in 0..(STDERR_RING + 10) {
            rings.note_stderr(&format!("line {i}"));
        }
        for i in 0..(PROTOCOL_ERROR_RING + 10) {
            rings.note_protocol_error(&format!("err {i}"));
        }
        assert_eq!(rings.rpc_tail().len(), RPC_RING);
        assert_eq!(rings.stderr_tail().len(), STDERR_RING);
        assert_eq!(rings.protocol_errors().len(), PROTOCOL_ERROR_RING);
        assert_eq!(rings.stderr_tail()[0].line, format!("line {}", 10));

        rings.note_rpc("out", &json!({ "method": "turn/start", "text": "x".repeat(64 * 1024) }));
        let last = rings.rpc_tail().pop().unwrap();
        assert!(last.json.len() <= RPC_FRAME_MAX + 4, "{}", last.json.len());
    }

    #[test]
    fn gateway_provider_overrides_route_via_gateway_when_env_bound() {
        let env: std::collections::BTreeMap<String, String> = [
            ("OPENAI_BASE_URL".to_owned(), "https://cctui.example/gateway/openai".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "cctui_s_tok".to_owned()),
            ("KEEP".to_owned(), "1".to_owned()),
        ]
        .into_iter()
        .collect();
        let got = gateway_provider_overrides(&env);
        assert_eq!(
            got,
            vec![
                ("model_provider".to_owned(), "cctui".to_owned()),
                ("model_providers.cctui.name".to_owned(), "cctui-gateway".to_owned()),
                (
                    "model_providers.cctui.base_url".to_owned(),
                    "https://cctui.example/gateway/openai".to_owned()
                ),
                ("model_providers.cctui.env_key".to_owned(), "OPENAI_API_KEY".to_owned()),
                ("model_providers.cctui.wire_api".to_owned(), "responses".to_owned()),
                (
                    "model_providers.cctui.http_headers.\"x-openai-actor-authorization\""
                        .to_owned(),
                    "cctui-gateway".to_owned()
                ),
            ]
        );
        assert!(got.iter().map(|(k, v)| format!("{k}=\"{v}\"")).any(|arg| arg
            == "model_providers.cctui.http_headers.\"x-openai-actor-authorization\"=\"cctui-gateway\""));
    }

    #[test]
    fn gateway_provider_overrides_empty_without_a_gateway_base_url() {
        let empty = std::collections::BTreeMap::new();
        assert!(gateway_provider_overrides(&empty).is_empty());
        let key_only: std::collections::BTreeMap<String, String> =
            std::iter::once(("OPENAI_API_KEY".to_owned(), "tok".to_owned())).collect();
        assert!(gateway_provider_overrides(&key_only).is_empty());
    }

    /// A resume whose credential re-pull came back empty still has to DEFINE
    /// `model_providers.cctui`: codex reads the provider NAME back out of the
    /// rollout and fails config load (-32600) on a dangling reference.
    #[test]
    fn gateway_provider_is_defined_from_the_base_url_alone() {
        let url_only: std::collections::BTreeMap<String, String> =
            std::iter::once(("OPENAI_BASE_URL".to_owned(), "https://x/gateway".to_owned()))
                .collect();
        let got = gateway_provider_overrides(&url_only);
        assert_eq!(
            got.iter()
                .find(|(k, _)| k == "model_providers.cctui.base_url")
                .map(|(_, v)| v.as_str()),
            Some("https://x/gateway")
        );
        assert!(got.contains(&("model_provider".to_owned(), "cctui".to_owned())));
        assert!(
            got.contains(&(
                "model_providers.cctui.env_key".to_owned(),
                "OPENAI_API_KEY".to_owned()
            )),
            "the bearer still comes from the launch env at request time"
        );
    }

    #[test]
    fn classifies_response() {
        let v = json!({"id": 2, "result": {"thread": {"sessionId": "abc"}}});
        match classify("", &v) {
            Incoming::Response { id, .. } => assert_eq!(id, 2),
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn classifies_command_approval_request() {
        // Server→client request: has both method and id.
        let v = json!({
            "method": "item/commandExecution/requestApproval",
            "id": 0,
            "params": {"threadId": "t", "itemId": "call_9", "command": "rm -rf /"},
        });
        match classify("sess", &v) {
            Incoming::Approval { rpc_id, request_id, tool, kind, .. } => {
                assert_eq!(rpc_id, json!(0));
                assert_eq!(request_id, "call_9");
                assert_eq!(tool, "shell");
                assert_eq!(kind, ApprovalKind::AcceptDecline);
            }
            other => panic!("expected Approval, got {other:?}"),
        }
    }

    #[test]
    fn classifies_apply_patch_approval() {
        let v = json!({"method": "applyPatchApproval", "id": 5, "params": {"itemId": "p1"}});
        match classify("s", &v) {
            Incoming::Approval { tool, request_id, kind, .. } => {
                assert_eq!(tool, "apply_patch");
                assert_eq!(request_id, "p1");
                assert_eq!(kind, ApprovalKind::ApprovedDenied);
            }
            other => panic!("expected apply_patch Approval, got {other:?}"),
        }
    }

    #[test]
    fn file_change_and_exec_command_approvals_classify() {
        match classify(
            "s",
            &json!({"method": "item/fileChange/requestApproval", "id": 1, "params": {}}),
        ) {
            Incoming::Approval { kind, tool, .. } => {
                assert_eq!(kind, ApprovalKind::AcceptDecline);
                assert_eq!(tool, "file_change");
            }
            other => panic!("expected file-change Approval, got {other:?}"),
        }
        match classify("s", &json!({"method": "execCommandApproval", "id": 1, "params": {}})) {
            Incoming::Approval { kind, .. } => assert_eq!(kind, ApprovalKind::ApprovedDenied),
            other => panic!("expected exec-command Approval, got {other:?}"),
        }
    }

    #[test]
    fn permissions_approval_is_declined_not_left_blocking() {
        // Sandbox-permission elevation has no simple allow/deny reply, so it is
        // declined (empty grant) rather than left hanging.
        let v = json!({"method": "item/permissions/requestApproval", "id": 1, "params": {}});
        match classify("s", &v) {
            Incoming::Decline { reply } => {
                assert_eq!(reply["id"], json!(1));
                assert_eq!(reply["result"]["permissions"], json!({}));
            }
            other => panic!("expected Decline, got {other:?}"),
        }
    }

    #[test]
    fn mcp_elicitation_is_declined() {
        let v = json!({"method": "mcpServer/elicitation/request", "id": 3,
            "params": {"serverName": "s", "threadId": "t", "message": "pick", "mode": "form"}});
        match classify("s", &v) {
            Incoming::Decline { reply } => {
                assert_eq!(reply["id"], json!(3));
                assert_eq!(reply["result"]["action"], "decline");
            }
            other => panic!("expected Decline, got {other:?}"),
        }
    }

    #[test]
    fn unknown_server_request_is_declined_with_error() {
        // A dynamic tool call / future method cctui does not implement must not
        // hang codex: it is answered with a JSON-RPC method-not-found error.
        let v = json!({"method": "item/tool/call", "id": 9, "params": {}});
        match classify("s", &v) {
            Incoming::Decline { reply } => {
                assert_eq!(reply["id"], json!(9));
                assert_eq!(reply["error"]["code"], -32601);
                assert!(reply["error"]["message"].as_str().unwrap().contains("item/tool/call"));
            }
            other => panic!("expected Decline, got {other:?}"),
        }
    }

    #[test]
    fn request_user_input_maps_to_question() {
        let v = json!({"method": "item/tool/requestUserInput", "id": 4, "params": {
        "itemId": "call_42", "threadId": "t", "turnId": "u",
        "questions": [
            {"id": "q1", "header": "Deploy", "question": "Which env?",
             "options": [{"label": "prod", "description": "production"},
                         {"label": "staging", "description": "staging"}]},
        ]}});
        match classify("sess", &v) {
            Incoming::Question { rpc_id, question, questions, question_ids } => {
                assert_eq!(rpc_id, json!(4));
                assert_eq!(question, "Deploy — Which env?");
                assert_eq!(question_ids, vec!["q1".to_owned()]);
                assert_eq!(questions[0]["options"][0]["label"], "prod");
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn request_user_input_with_no_question_ids_still_maps() {
        let v = json!({"method": "item/tool/requestUserInput", "id": 7,
            "params": {"threadId": "t", "turnId": "u", "questions": []}});
        match classify("s", &v) {
            Incoming::Question { rpc_id, question_ids, .. } => {
                assert_eq!(rpc_id, json!(7));
                assert!(question_ids.is_empty());
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn user_input_reply_keys_answer_by_question_id() {
        let reply =
            user_input_reply(&json!(4), &["q1".to_owned(), "q2".to_owned()], "prod, us-east");
        assert_eq!(reply["id"], json!(4));
        assert_eq!(reply["result"]["answers"]["q1"]["answers"][0], "prod, us-east");
        assert_eq!(reply["result"]["answers"]["q2"]["answers"][0], "prod, us-east");
    }

    #[test]
    fn decline_reply_builders_shape() {
        assert_eq!(elicitation_decline(&json!(1))["result"]["action"], "decline");
        assert_eq!(permissions_decline(&json!(2))["result"]["permissions"], json!({}));
        assert_eq!(request_not_supported(&json!(3), "x/y")["error"]["code"], -32601);
    }

    #[test]
    fn approval_without_item_id_falls_back_to_rpc_id() {
        let v = json!({"method": "item/commandExecution/requestApproval", "id": 7, "params": {}});
        match classify("s", &v) {
            Incoming::Approval { request_id, .. } => assert_eq!(request_id, "codex-approval-7"),
            other => panic!("expected Approval, got {other:?}"),
        }
    }

    #[test]
    fn command_execution_item_maps_to_tool_use() {
        let v = json!({
            "method": "item/completed",
            "params": {"item": {"type": "commandExecution", "command": "ls", "status": "completed"}},
        });
        match classify("sess", &v) {
            Incoming::Event(AdapterEvent::ToolUse { local_id, .. }) => assert_eq!(local_id, "sess"),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn agent_message_item_maps_to_message() {
        let v = json!({
            "method": "item/completed",
            "params": {"item": {"type": "agentMessage", "text": "done"}},
        });
        match classify("sess", &v) {
            Incoming::Event(AdapterEvent::Message { local_id, .. }) => assert_eq!(local_id, "sess"),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn item_started_is_ignored_to_avoid_duplicates() {
        let v = json!({
            "method": "item/started",
            "params": {"item": {"type": "commandExecution", "command": "ls"}},
        });
        assert!(matches!(classify("sess", &v), Incoming::Traced { .. }));
    }

    #[test]
    fn turn_lifecycle_is_traced_not_dropped() {
        assert!(matches!(
            classify("s", &json!({"method": "turn/completed", "params": {}})),
            Incoming::Traced { .. }
        ));
        match classify("s", &json!({"method": "thread/started", "params": {}})) {
            Incoming::Event(AdapterEvent::Message { payload, .. }) => {
                assert_eq!(payload["type"], "codexNotice");
            }
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    /// The vendored protocol schema is the source of truth for what codex can
    /// send. Every `ServerNotification` method in it must resolve to a real
    /// disposition — a re-vendored schema with a new method fails here instead
    /// of silently falling through at runtime.
    #[test]
    fn every_schema_notification_has_a_disposition() {
        let raw = include_str!("schema/codex_app_server_protocol.schemas.json");
        let schema: Value = serde_json::from_str(raw).expect("schema parses");
        let variants = schema
            .pointer("/definitions/ServerNotification/oneOf")
            .and_then(Value::as_array)
            .expect("ServerNotification oneOf");
        let methods: Vec<&str> = variants
            .iter()
            .filter_map(|v| v.pointer("/properties/method/enum/0").and_then(Value::as_str))
            .collect();
        assert_eq!(methods.len(), variants.len(), "every variant pins one method");
        assert!(methods.len() >= 81, "schema shrank unexpectedly: {} methods", methods.len());

        let missing: Vec<&str> =
            methods.iter().copied().filter(|m| disposition(m) == Disposition::Unknown).collect();
        assert!(missing.is_empty(), "codex notifications with no disposition: {missing:?}");
    }

    /// The reverse guard: nothing in the table has been invented or left
    /// behind by a protocol removal.
    #[test]
    fn disposition_table_has_no_methods_the_schema_lacks() {
        let raw = include_str!("schema/codex_app_server_protocol.schemas.json");
        let schema: Value = serde_json::from_str(raw).expect("schema parses");
        let known: std::collections::HashSet<String> = schema
            .pointer("/definitions/ServerNotification/oneOf")
            .and_then(Value::as_array)
            .expect("ServerNotification oneOf")
            .iter()
            .filter_map(|v| {
                v.pointer("/properties/method/enum/0").and_then(Value::as_str).map(str::to_owned)
            })
            .collect();
        let stale: Vec<&str> = NOTIFICATION_DISPOSITIONS
            .iter()
            .map(|(m, _)| *m)
            .filter(|m| !known.contains(*m))
            .collect();
        assert!(stale.is_empty(), "table lists methods the schema does not: {stale:?}");
        let uncovered: Vec<&String> = known
            .iter()
            .filter(|m| !NOTIFICATION_DISPOSITIONS.iter().any(|(t, _)| t == m))
            .collect();
        assert!(uncovered.is_empty(), "schema methods absent from the table: {uncovered:?}");
        assert_eq!(NOTIFICATION_DISPOSITIONS.len(), known.len(), "one entry per method");
    }

    #[test]
    fn unknown_method_is_unhandled_not_dropped() {
        let v = json!({"method": "future/thing", "params": {"message": "hi"}});
        match classify("s", &v) {
            Incoming::Unhandled { method, event } => {
                assert_eq!(method, "future/thing");
                let AdapterEvent::Message { payload, .. } = event else { panic!("want Message") };
                assert_eq!(payload["level"], "unhandled");
                assert_eq!(payload["text"], "future/thing: hi");
            }
            other => panic!("expected Unhandled, got {other:?}"),
        }
    }

    #[test]
    fn warning_family_surfaces_as_warning_notices() {
        for method in
            ["warning", "guardianWarning", "configWarning", "deprecationNotice", "model/rerouted"]
        {
            let v = json!({"method": method, "params": {"message": "careful"}});
            match classify("s", &v) {
                Incoming::Event(AdapterEvent::Message { payload, .. }) => {
                    assert_eq!(payload["type"], "codexNotice");
                    assert_eq!(payload["method"], method);
                }
                other => panic!("{method}: expected a notice, got {other:?}"),
            }
        }
        let warn = json!({"method": "warning", "params": {"message": "m"}});
        let Incoming::Event(AdapterEvent::Message { payload, .. }) = classify("s", &warn) else {
            panic!("want Message")
        };
        assert_eq!(payload["level"], "warning");
    }

    #[test]
    fn plan_updated_renders_a_checklist() {
        let v = json!({"method": "turn/plan/updated", "params": {
            "threadId": "t", "turnId": "u", "explanation": "why",
            "plan": [
                {"step": "one", "status": "completed"},
                {"step": "two", "status": "in_progress"},
                {"step": "three", "status": "pending"},
            ],
        }});
        let Incoming::Event(AdapterEvent::Message { payload, .. }) = classify("s", &v) else {
            panic!("want Message")
        };
        assert_eq!(payload["type"], "plan");
        let text = payload["text"].as_str().unwrap();
        assert!(text.starts_with("why\n\n"), "{text}");
        assert!(text.contains("- [x] one"), "{text}");
        assert!(text.contains("- [~] two"), "{text}");
        assert!(text.contains("- [ ] three"), "{text}");
    }

    #[test]
    fn compacted_emits_a_context_reset_marker() {
        let v = json!({"method": "thread/compacted", "params": {"threadId": "t", "summary": "s"}});
        let Incoming::Event(AdapterEvent::Message { payload, .. }) = classify("s", &v) else {
            panic!("want Message")
        };
        assert_eq!(payload["type"], "contextCompaction");
        assert_eq!(payload["text"], "s");
    }

    #[test]
    fn thread_info_reads_session_id_and_path() {
        let result = json!({"thread": {
            "id": "019e6628-af3f-7131",
            "sessionId": "019e6628-af3f-7131",
            "cwd": "/tmp",
            "path": "/home/u/.codex/sessions/2026/05/27/rollout-x-019e6628.jsonl",
        }});
        let info = thread_info(&result).expect("thread info");
        assert_eq!(info.thread_id, "019e6628-af3f-7131");
        assert_eq!(info.cwd.as_deref(), Some("/tmp"));
        assert!(info.rollout_path.unwrap().ends_with("019e6628.jsonl"));
    }

    #[test]
    fn approval_reply_uses_correct_decision_vocabulary() {
        // command/file-change family
        let a = approval_reply(&json!(0), ApprovalKind::AcceptDecline, true);
        assert_eq!(a["id"], json!(0));
        assert_eq!(a["result"]["decision"], "accept");
        assert_eq!(
            approval_reply(&json!(0), ApprovalKind::AcceptDecline, false)["result"]["decision"],
            "decline"
        );
        // patch/exec family (ReviewDecision)
        assert_eq!(
            approval_reply(&json!(0), ApprovalKind::ApprovedDenied, true)["result"]["decision"],
            "approved"
        );
        assert_eq!(
            approval_reply(&json!(0), ApprovalKind::ApprovedDenied, false)["result"]["decision"],
            "denied"
        );
    }

    #[test]
    fn initialize_declares_capabilities_and_handshake() {
        let init = initialize_req();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], false);
        assert_eq!(init["params"]["capabilities"]["requestAttestation"], false);
        assert_eq!(init["params"]["clientInfo"]["name"], "cctui");

        let done = initialized_notification();
        assert_eq!(done["method"], "initialized");
        assert!(done.get("id").is_none(), "initialized is a notification, not a request");
    }

    #[test]
    fn record_codex_version_extracts_from_user_agent() {
        let resp = json!({
            "id": 1,
            "result": {
                "userAgent": "cctui/0.144.1 (Ubuntu 24.4.0; x86_64) xterm-256color (cctui; 0.0.0)",
                "platformOs": "linux",
            },
        });
        assert_eq!(record_codex_version(&resp).as_deref(), Some("0.144.1"));

        let missing = json!({"id": 1, "result": {"platformOs": "linux"}});
        assert_eq!(record_codex_version(&missing), None);
    }

    #[test]
    fn request_builders_shape() {
        assert_eq!(initialize_req()["method"], "initialize");
        assert_eq!(
            thread_start_req("/tmp", &std::collections::BTreeMap::default(), None)["params"]["cwd"],
            "/tmp"
        );
        let resume =
            thread_resume_req("tid", "/repo", &std::collections::BTreeMap::default(), None);
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"]["threadId"], "tid");
        assert_eq!(resume["params"]["cwd"], "/repo");
        let fork =
            thread_fork_req("parent-tid", "/repo", &std::collections::BTreeMap::default(), None);
        assert_eq!(fork["method"], "thread/fork");
        assert_eq!(fork["params"]["threadId"], "parent-tid");
        assert_eq!(fork["params"]["cwd"], "/repo");
        let rename = thread_name_set_req(101, "tid", "build fix");
        assert_eq!(rename["method"], "thread/name/set");
        assert_eq!(rename["id"], 101);
        assert_eq!(rename["params"]["threadId"], "tid");
        assert_eq!(rename["params"]["name"], "build fix");
        let turn = turn_start_req(100, "tid", "hello", &[], None, None);
        assert_eq!(turn["params"]["threadId"], "tid");
        assert_eq!(turn["params"]["input"][0]["text"], "hello");
    }

    #[test]
    fn turn_start_req_carries_only_provided_overrides() {
        // No override — model/effort keys absent so codex keeps its defaults.
        let plain = turn_start_req(100, "tid", "hi", &[], None, None);
        assert!(plain["params"].get("model").is_none());
        assert!(plain["params"].get("effort").is_none());
        // Both overrides ride the turn (per-turn model change).
        let both = turn_start_req(101, "tid", "hi", &[], Some("gpt-5-codex"), Some("high"));
        assert_eq!(both["method"], "turn/start");
        assert_eq!(both["params"]["model"], "gpt-5-codex");
        assert_eq!(both["params"]["effort"], "high");
        // Model only — effort key must be absent.
        let model_only = turn_start_req(102, "tid", "hi", &[], Some("gpt-5-codex"), None);
        assert_eq!(model_only["params"]["model"], "gpt-5-codex");
        assert!(model_only["params"].get("effort").is_none());
    }

    #[test]
    fn turn_input_items_sends_images_native_and_files_in_text() {
        let attachments = vec![
            "/tmp/cctui-uploads/s/diagram.png".to_owned(),
            "/tmp/cctui-uploads/s/report.pdf".to_owned(),
            "/tmp/cctui-uploads/s/photo.JPEG".to_owned(),
        ];
        let items = turn_input_items("look at these", &attachments);
        // Text item first: prompt plus a listing of the non-image file only.
        assert_eq!(items[0]["type"], "text");
        let text = items[0]["text"].as_str().unwrap();
        assert!(text.contains("look at these"));
        assert!(text.contains("report.pdf"));
        assert!(!text.contains("diagram.png"), "images are native, not text paths");
        // Both images become localImage inputs carrying their local paths.
        let images: Vec<&Value> = items.iter().filter(|i| i["type"] == "localImage").collect();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["path"], "/tmp/cctui-uploads/s/diagram.png");
        assert_eq!(images[1]["path"], "/tmp/cctui-uploads/s/photo.JPEG");
    }

    #[test]
    fn turn_input_items_image_only_prompt_has_no_empty_text_gap() {
        // An image-only spawn (no prompt text) still yields a valid input array:
        // just the localImage item, no stray empty text item.
        let items = turn_input_items("", &["/tmp/s/shot.png".to_owned()]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "localImage");
        // No attachments and no text falls back to a single empty text item.
        let empty = turn_input_items("", &[]);
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0]["type"], "text");
    }

    #[tokio::test]
    async fn record_model_override_updates_state_and_acks() {
        let registry = SessionRegistry::default();
        registry.lock().await.insert(
            "tid".to_owned(),
            SessionRecord {
                cfg: AppServerConfig::default(),
                cwd: "/tmp".to_owned(),
                name: None,
                env: std::collections::BTreeMap::new(),
                spawn_relay: false,
            },
        );
        let (tx, mut rx) = mpsc::channel(8);
        let mut model = None;
        let mut effort = None;
        let command_id = Uuid::new_v4();
        record_model_override(
            &mut model,
            &mut effort,
            Some("gpt-5-codex"),
            Some("high"),
            "tid",
            &tx,
            &registry,
            Some(command_id),
        )
        .await;
        // Override recorded for the next turn/start.
        assert_eq!(model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(effort.as_deref(), Some("high"));
        // Durable cfg folded in for a later resume's `-c` flags.
        let rec = registry.lock().await.get("tid").cloned().unwrap();
        assert_eq!(rec.cfg.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(rec.cfg.reasoning_effort.as_deref(), Some("high"));
        // Chip Status then a truthful ok CommandResult.
        let status = rx.recv().await.unwrap();
        assert!(
            matches!(status, AdapterEvent::Status { model: Some(m), .. } if m == "gpt-5-codex")
        );
        let ack = rx.recv().await.unwrap();
        assert!(
            matches!(ack, AdapterEvent::CommandResult { ok: true, command_id: c, .. } if c == command_id)
        );
    }

    #[test]
    fn set_model_is_resumable() {
        assert!(
            SessionCommand::SetModel { model: Some("m".into()), effort: None, command_id: None }
                .is_resumable()
        );
    }

    #[tokio::test]
    async fn route_delivers_to_live_sender() {
        let live = LiveSessionRegistry::default();
        let registry = SessionRegistry::default();
        let (tx, mut rx) = mpsc::channel(1);
        live.lock().await.insert("tid".to_owned(), tx);
        registry.lock().await.insert(
            "tid".to_owned(),
            SessionRecord {
                cfg: AppServerConfig::default(),
                cwd: "/tmp".to_owned(),
                name: Some("n".to_owned()),
                env: std::collections::BTreeMap::new(),
                spawn_relay: false,
            },
        );

        let action = route_or_prepare_resume(
            &live,
            &registry,
            "tid",
            SessionCommand::Send { text: "hi".to_owned(), command_id: None },
        )
        .await;
        assert!(matches!(action, RouteAction::Delivered));
        assert!(matches!(rx.recv().await, Some(SessionCommand::Send { text, .. }) if text == "hi"));
    }

    #[tokio::test]
    async fn route_prepares_resume_when_live_sender_is_closed() {
        let live = LiveSessionRegistry::default();
        let registry = SessionRegistry::default();
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        live.lock().await.insert("tid".to_owned(), tx);
        registry.lock().await.insert(
            "tid".to_owned(),
            SessionRecord {
                cfg: AppServerConfig::default(),
                cwd: "/repo".to_owned(),
                name: Some("stale".to_owned()),
                env: std::collections::BTreeMap::new(),
                spawn_relay: false,
            },
        );

        let action = route_or_prepare_resume(
            &live,
            &registry,
            "tid",
            SessionCommand::Rename { name: "new".to_owned() },
        )
        .await;
        match action {
            RouteAction::Resume { record, command: SessionCommand::Rename { name } } => {
                assert_eq!(record.cwd, "/repo");
                assert_eq!(record.name.as_deref(), Some("stale"));
                assert_eq!(name, "new");
            }
            other => panic!("expected resume action, got {other:?}"),
        }
        assert!(!live.lock().await.contains_key("tid"));
    }

    #[tokio::test]
    async fn route_missing_without_durable_record() {
        let live = LiveSessionRegistry::default();
        let registry = SessionRegistry::default();
        let action = route_or_prepare_resume(
            &live,
            &registry,
            "missing",
            SessionCommand::Send { text: "hi".to_owned(), command_id: None },
        )
        .await;
        assert!(matches!(action, RouteAction::Missing));
    }

    #[tokio::test]
    #[ignore = "requires `codex` installed locally; run with `--ignored`"]
    async fn real_codex_handshake_emits_session_started() {
        let (tx, mut rx) = mpsc::channel(64);
        let live = LiveSessionRegistry::default();
        let registry = SessionRegistry::default();
        let shutdown = CancellationToken::new();
        let session = CodexSession::new_fresh(
            AppServerConfig::default(),
            "/tmp".to_string(),
            std::collections::BTreeMap::new(),
            None, // no prompt → no turn/start, so no model auth needed
            None,
            Vec::new(),
            None,
            None,
            None,
            tx,
            live,
            registry.clone(),
            shutdown.clone(),
        );
        let handle = tokio::spawn(session.run());
        let evt = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
            .await
            .expect("timed out waiting for SessionStarted")
            .expect("event channel closed");
        match evt {
            AdapterEvent::SessionStarted { local_id, meta } => {
                assert!(!local_id.is_empty(), "session id should be the rollout uuid");
                assert_eq!(meta.working_dir.as_deref(), Some("/tmp"));
                assert!(registry.lock().await.contains_key(&local_id), "session must register");
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    fn fake_codex(script: &str) -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("codex");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = bin.to_string_lossy().into_owned();
        (dir, path)
    }

    /// Answers `initialize`, serves a two-model catalog, and records any
    /// `thread/start` in `marker` so a test can assert it never got there.
    fn catalog_server_script(marker: &std::path::Path) -> String {
        FAKE_CATALOG_SERVER.replace("$MARKER", &marker.to_string_lossy())
    }

    const FAKE_CATALOG_SERVER: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"codex/0.144.1"}}' ;;
    *'"method":"model/list"'*) echo '{"jsonrpc":"2.0","id":100,"result":{"data":[{"id":"gpt-5-codex","model":"gpt-5-codex","displayName":"GPT-5 Codex","hidden":false,"isDefault":true},{"id":"gpt-5-secret","model":"gpt-5-secret","displayName":"hidden","hidden":true}]}}' ;;
    *'"method":"thread/start"'*) echo started > "$MARKER"; echo '{"jsonrpc":"2.0","id":2,"result":{"thread":{"id":"t-1","cwd":"/tmp"}}}' ;;
  esac
done
"#;

    fn run_fresh_spawn(
        bin: String,
        model: Option<&str>,
    ) -> (Uuid, mpsc::Receiver<AdapterEvent>, CancellationToken) {
        let (tx, rx) = mpsc::channel(64);
        let command_id = Uuid::new_v4();
        let shutdown = CancellationToken::new();
        let session = CodexSession::new_fresh(
            AppServerConfig { bin, model: model.map(str::to_owned), ..AppServerConfig::default() },
            "/tmp".to_string(),
            std::collections::BTreeMap::new(),
            None,
            None,
            Vec::new(),
            Some(command_id),
            None,
            None,
            tx,
            LiveSessionRegistry::default(),
            SessionRegistry::default(),
            shutdown.clone(),
        );
        tokio::spawn(session.run());
        (command_id, rx, shutdown)
    }

    async fn spawn_result(rx: &mut mpsc::Receiver<AdapterEvent>) -> (bool, Option<String>) {
        let wait = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(evt) = rx.recv().await {
                if let AdapterEvent::CommandResult { ok, error, .. } = evt {
                    return (ok, error);
                }
            }
            panic!("no CommandResult");
        });
        wait.await.expect("spawn ack within 10s")
    }

    #[tokio::test]
    async fn early_exit_folds_stderr_into_spawn_failure() {
        let (_dir, bin) =
            fake_codex("#!/bin/sh\necho 'error: not logged in; run codex login' >&2\nexit 1\n");
        let (command_id, mut rx, shutdown) = run_fresh_spawn(bin, None);
        let (ok, error) = spawn_result(&mut rx).await;
        shutdown.cancel();
        assert!(!ok, "{command_id} should fail");
        let error = error.unwrap();
        assert!(error.contains("exited (exit status: 1) before the thread was started"), "{error}");
        assert!(error.contains("not logged in; run codex login"), "{error}");
    }

    #[tokio::test]
    async fn a_model_the_local_catalog_does_not_list_still_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let (_bin_dir, bin) = fake_codex(&catalog_server_script(&marker));
        let (_, mut rx, shutdown) = run_fresh_spawn(bin, Some("gpt-6-astra"));
        let (ok, error) = spawn_result(&mut rx).await;
        shutdown.cancel();
        assert!(ok, "{error:?}");
        assert!(marker.exists(), "a stale catalog must not block a valid model");
    }

    #[tokio::test]
    async fn known_model_passes_the_catalog_check() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("started");
        let (_bin_dir, bin) = fake_codex(&catalog_server_script(&marker));
        let (_, mut rx, shutdown) = run_fresh_spawn(bin, Some("gpt-5-codex"));
        let (ok, error) = spawn_result(&mut rx).await;
        shutdown.cancel();
        assert!(ok, "{error:?}");
        assert!(marker.exists());
    }

    /// Answers the handshake, then runs `$TURN` for a `turn/start`. Echoes the
    /// request's own id back so the driver's correlation table resolves.
    fn turn_server_script(turn: &str) -> String {
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/^[^}}]*"id":\([0-9]*\).*$/\1/')
  case "$line" in
    *'"method":"initialize"'*) echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"userAgent\":\"codex/0.144.1\"}}}}" ;;
    *'"method":"thread/start"'*) echo "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"thread\":{{\"id\":\"t-1\",\"cwd\":\"/tmp\"}}}}}}" ;;
    *'"method":"turn/start"'*) {turn} ;;
  esac
done
"#
        )
    }

    async fn send_to_live_session(
        bin: String,
        cmd: SessionCommand,
    ) -> (mpsc::Receiver<AdapterEvent>, CancellationToken) {
        let (tx, mut rx) = mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let live = LiveSessionRegistry::default();
        let session = CodexSession::new_fresh(
            AppServerConfig { bin, ..AppServerConfig::default() },
            "/tmp".to_string(),
            std::collections::BTreeMap::new(),
            None,
            None,
            Vec::new(),
            Some(Uuid::new_v4()),
            None,
            None,
            tx,
            live.clone(),
            SessionRegistry::default(),
            shutdown.clone(),
        );
        tokio::spawn(session.run());
        let (ok, error) = spawn_result(&mut rx).await;
        assert!(ok, "spawn failed: {error:?}");
        let sender = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let registered = live.lock().await.get("t-1").cloned();
                if let Some(tx) = registered {
                    return tx;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("session registered within 10s");
        sender.send(cmd).await.unwrap();
        (rx, shutdown)
    }

    #[tokio::test]
    async fn a_send_is_acked_once_the_app_server_accepts_the_turn() {
        let (_dir, bin) = fake_codex(&turn_server_script(
            r#"echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{}}""#,
        ));
        let command_id = Uuid::new_v4();
        let (mut rx, shutdown) = send_to_live_session(
            bin,
            SessionCommand::Send { text: "hi".to_owned(), command_id: Some(command_id) },
        )
        .await;
        let (ok, error) = spawn_result(&mut rx).await;
        shutdown.cancel();
        assert!(ok, "a delivered send must confirm, not stay unconfirmed: {error:?}");
    }

    #[tokio::test]
    async fn a_send_in_flight_when_the_app_server_dies_is_acked_as_failed() {
        let (_dir, bin) = fake_codex(&turn_server_script("exit 0"));
        let command_id = Uuid::new_v4();
        let (mut rx, shutdown) = send_to_live_session(
            bin,
            SessionCommand::Send { text: "hi".to_owned(), command_id: Some(command_id) },
        )
        .await;
        let (ok, error) = spawn_result(&mut rx).await;
        shutdown.cancel();
        assert!(!ok, "an unanswered turn/start must not resolve as success");
        let error = error.unwrap();
        assert!(error.contains("turn/start"), "{error}");
    }

    #[test]
    fn drain_takes_every_unanswered_request() {
        let mut pending = PendingRpcs::default();
        let cid = Uuid::new_v4();
        pending.insert(1, "turn/start", Some(cid), Instant::now() + RPC_TIMEOUT);
        pending.insert(2, "model/list", None, Instant::now() + RPC_TIMEOUT);
        let mut drained = pending.drain();
        drained.sort_by_key(|(id, _)| *id);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].1.command_id, Some(cid));
        assert!(pending.drain().is_empty());
    }

    #[test]
    fn unknown_model_lists_visible_ids() {
        let catalog = CodexModelCatalog {
            models: model_list::parse_model_list(&json!({"data": [
                {"id": "a", "hidden": false}, {"id": "b", "hidden": true}
            ]})),
        };
        assert_eq!(unknown_model(Some("a"), &catalog), None);
        assert_eq!(unknown_model(Some("b"), &catalog), None);
        assert_eq!(unknown_model(None, &catalog), None);
        assert_eq!(
            unknown_model(Some("x"), &catalog).as_deref(),
            Some("unknown model x; available: a")
        );
        assert_eq!(unknown_model(Some("x"), &CodexModelCatalog { models: vec![] }), None);
    }

    #[test]
    fn config_overrides_from_value() {
        let cfg = AppServerConfig::from_value(&json!({
            "codex_bin": "/opt/codex", "approval_policy": "never",
            "sandbox_mode": "danger-full-access",
        }));
        assert_eq!(cfg.bin, "/opt/codex");
        assert_eq!(cfg.approval_policy, "never");
        assert_eq!(cfg.sandbox_mode, "danger-full-access");
        // Default sandbox_mode is the safe, sandboxed mode.
        assert_eq!(AppServerConfig::default().sandbox_mode, "workspace-write");
    }

    #[test]
    fn model_catalog_toggle_defaults_on_and_reads_config() {
        assert!(AppServerConfig::default().model_catalog);
        assert!(AppServerConfig::from_value(&json!({})).model_catalog);
        assert!(!AppServerConfig::from_value(&json!({"model_catalog": false})).model_catalog);
    }

    #[test]
    fn config_overrides_carry_no_per_session_tier() {
        for cfg in [
            AppServerConfig::default(),
            AppServerConfig {
                reasoning_effort: Some("high".to_owned()),
                model: Some("gpt-5-codex".to_owned()),
                ..AppServerConfig::default()
            },
        ] {
            let overrides = cfg.config_overrides();
            let keys: Vec<&str> = overrides.iter().map(|(k, _)| k.as_str()).collect();
            assert!(
                !keys.iter().any(|k| k.to_lowercase().contains("fast")),
                "no fast-mode knob may be set, got {keys:?}"
            );
            // Only the four known, intentional knobs are ever set.
            for k in &keys {
                assert!(
                    matches!(
                        *k,
                        "approval_policy" | "sandbox_mode" | "model_reasoning_effort" | "model"
                    ),
                    "unexpected codex config knob {k:?}"
                );
            }
        }
    }

    #[test]
    fn config_overrides_default_and_with_quality_knobs() {
        let base = AppServerConfig::default().config_overrides();
        assert_eq!(base.len(), 2);
        assert!(base.contains(&("approval_policy".to_owned(), "untrusted".to_owned())));
        assert!(base.contains(&("sandbox_mode".to_owned(), "workspace-write".to_owned())));

        let with = AppServerConfig {
            reasoning_effort: Some("high".to_owned()),
            model: Some("gpt-5-codex".to_owned()),
            ..AppServerConfig::default()
        }
        .config_overrides();
        assert!(with.contains(&("model_reasoning_effort".to_owned(), "high".to_owned())));
        assert!(with.contains(&("model".to_owned(), "gpt-5-codex".to_owned())));
    }

    fn env_with_block(block: &str) -> std::collections::BTreeMap<String, String> {
        let mut env = std::collections::BTreeMap::new();
        env.insert("OPENAI_BASE_URL".to_owned(), "https://gw/openai".to_owned());
        env.insert(cctui_proto::codex_config::CONFIG_TOML_ENV.to_owned(), block.to_owned());
        env
    }

    /// A curated setting stored on `account_providers.settings_json` must reach
    /// the launched process — as a `-c` flag with a correctly typed value.
    #[test]
    fn account_settings_reach_the_launch_command_line() {
        let block = cctui_proto::codex_config::render_block(&json!({
            "web_search": true,
            "model_verbosity": "low",
            "model_context_window": 272_000,
        }))
        .expect("rendered");
        let got = launch_overrides(&AppServerConfig::default(), &env_with_block(&block));
        let flags: Vec<String> = got.iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert!(flags.contains(&"web_search=true".to_owned()), "{flags:?}");
        assert!(flags.contains(&"model_verbosity=\"low\"".to_owned()), "{flags:?}");
        assert!(flags.contains(&"model_context_window=272000".to_owned()), "{flags:?}");
        // The managed knobs still ride along, still quoted.
        assert!(flags.contains(&"approval_policy=\"untrusted\"".to_owned()), "{flags:?}");
        assert!(flags.contains(&"model_provider=\"cctui\"".to_owned()), "{flags:?}");
    }

    /// Anything outside the curated set is dropped rather than forwarded — an
    /// unknown key fails app-server startup, and a managed one would move
    /// gateway routing or the permission posture.
    #[test]
    fn uncurated_and_managed_keys_never_reach_the_command_line() {
        let hostile = "model_provider = \"evil\"\napproval_policy = \"never\"\n\
                       sandbox_mode = \"danger-full-access\"\nservice_tier = \"fast\"\n\
                       disableBundledSkills = true\ntotallyNotAKey = 1";
        let got = launch_overrides(&AppServerConfig::default(), &env_with_block(hostile));
        assert!(!got.iter().any(|(k, _)| k == "service_tier" || k == "disableBundledSkills"));
        assert!(!got.iter().any(|(k, _)| k == "totallyNotAKey"));
        // The keys that collide with managed ones survive only with cctui's values.
        for (key, want) in [
            ("model_provider", "\"cctui\""),
            ("approval_policy", "\"untrusted\""),
            ("sandbox_mode", "\"workspace-write\""),
        ] {
            let vals: Vec<&str> =
                got.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect();
            assert_eq!(vals, vec![want], "{key} must be cctui's alone");
        }
    }

    #[test]
    fn launch_overrides_are_unchanged_without_an_account_block() {
        let env = std::collections::BTreeMap::new();
        let got = launch_overrides(&AppServerConfig::default(), &env);
        assert_eq!(
            got,
            vec![
                ("approval_policy".to_owned(), "\"untrusted\"".to_owned()),
                ("sandbox_mode".to_owned(), "\"workspace-write\"".to_owned()),
            ]
        );
    }

    // --- v2 notification mapping (codex-cli 0.135 wire payloads) ----------

    #[test]
    fn status_active_with_waiting_flag_is_blocked() {
        let v = json!({"method":"thread/status/changed","params":{
            "threadId":"t","status":{"type":"active","activeFlags":["waitingOnApproval"]}}});
        match classify("t", &v) {
            Incoming::Event(AdapterEvent::Status { tempo, .. }) => {
                assert_eq!(tempo.as_deref(), Some("blocked"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn status_active_no_flags_is_active_idle_is_idle() {
        let active = json!({"method":"thread/status/changed","params":{
            "status":{"type":"active","activeFlags":[]}}});
        let Incoming::Event(AdapterEvent::Status { tempo, .. }) = classify("t", &active) else {
            panic!("expected Status")
        };
        assert_eq!(tempo.as_deref(), Some("active"));

        let idle = json!({"method":"thread/status/changed","params":{"status":{"type":"idle"}}});
        let Incoming::Event(AdapterEvent::Status { tempo, state, .. }) = classify("t", &idle)
        else {
            panic!("expected Status")
        };
        assert_eq!(tempo, None);
        assert_eq!(state.as_deref(), Some("idle"));
    }

    #[test]
    fn waiting_status_classifies_as_needs_input() {
        use cctui_proto::classifier::{Bucket, ClassifyInput, PrStatus, classify as bucket_of};
        let v = json!({"method":"thread/status/changed","params":{
            "status":{"type":"active","activeFlags":["waitingOnUserInput"]}}});
        let Incoming::Event(AdapterEvent::Status { tempo, state, activity, .. }) =
            classify("t", &v)
        else {
            panic!("expected Status")
        };
        let input = ClassifyInput {
            tempo: tempo.as_deref(),
            state: state.as_deref(),
            activity: activity.as_deref(),
            children: &[],
            q: None,
            soft_limit_blocked: None,
        };
        let empty: std::collections::HashMap<String, PrStatus> = std::collections::HashMap::new();
        assert_eq!(bucket_of(&input, &empty), Bucket::Blocked);
    }

    #[test]
    fn token_usage_maps_last_keyed_by_turn() {
        let v = json!({"method":"thread/tokenUsage/updated","params":{
            "turnId":"turn-1",
            "tokenUsage":{"last":{"totalTokens":11617,"inputTokens":11592,
                "cachedInputTokens":9600,"outputTokens":25}}}});
        match classify("t", &v) {
            Incoming::Event(AdapterEvent::TokenUsage {
                message_id,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                ..
            }) => {
                assert_eq!(message_id, "turn-1");
                assert_eq!(input_tokens, 11592 - 9600); // non-cached input
                assert_eq!(output_tokens, 25);
                assert_eq!(cache_read_tokens, 9600);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn token_usage_without_turn_id_emits_nothing() {
        let v = json!({"method":"thread/tokenUsage/updated","params":{
            "tokenUsage":{"last":{"inputTokens":1}}}});
        assert!(matches!(classify("t", &v), Incoming::Traced { .. }));
    }

    #[test]
    fn thread_name_maps_to_status_name() {
        let v = json!({"method":"thread/name/updated","params":{"name":"my-thread"}});
        match classify("t", &v) {
            Incoming::Event(AdapterEvent::Status { name, .. }) => {
                assert_eq!(name.as_deref(), Some("my-thread"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    // --- correlated JSON-RPC outcomes -----------------------------

    #[test]
    fn pending_rpcs_resolves_success_response() {
        let mut table = PendingRpcs::default();
        table.insert(100, "turn/start", None, Instant::now() + RPC_TIMEOUT);
        let resp = json!({"id": 100, "result": {"turn": {"id": "t1"}}});
        let (pending, outcome) = table.resolve(100, &resp).expect("pending entry");
        assert_eq!(pending.method, "turn/start");
        assert_eq!(outcome.unwrap().pointer("/turn/id").and_then(Value::as_str), Some("t1"));
        assert!(table.is_empty());
        assert!(table.resolve(100, &resp).is_none(), "entry is one-shot");
    }

    #[test]
    fn pending_rpcs_propagates_error_response() {
        let mut table = PendingRpcs::default();
        let cid = Uuid::new_v4();
        table.insert(2, "thread/start", Some(cid), Instant::now() + HANDSHAKE_TIMEOUT);
        let resp = json!({"id": 2, "error": {"code": -32600, "message": "bad thread", "data": {"hint": "x"}}});
        let (pending, outcome) = table.resolve(2, &resp).expect("pending entry");
        assert_eq!(pending.command_id, Some(cid));
        assert!(pending.is_handshake());
        let err = outcome.unwrap_err();
        assert!(err.contains("-32600"), "{err}");
        assert!(err.contains("bad thread"), "{err}");
        assert!(err.contains("hint"), "{err}");
    }

    #[test]
    fn pending_rpcs_expires_only_past_deadline() {
        let mut table = PendingRpcs::default();
        let now = Instant::now();
        table.insert(1, "turn/start", None, now + Duration::from_secs(5));
        table.insert(2, "turn/interrupt", Some(Uuid::new_v4()), now + Duration::from_mins(1));
        let expired = table.expire(now + Duration::from_secs(30));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, 1);
        assert_eq!(expired[0].1.method, "turn/start");
        assert!(!table.is_empty());
        assert_eq!(table.expire(now + Duration::from_mins(2)).len(), 1);
        assert!(table.is_empty());
    }

    #[test]
    fn pending_rpcs_drain_cancels_everything_on_process_exit() {
        let mut table = PendingRpcs::default();
        let now = Instant::now();
        table.insert(1, "initialize", None, now + RPC_TIMEOUT);
        table.insert(100, "turn/start", None, now + RPC_TIMEOUT);
        table.insert(101, "turn/interrupt", Some(Uuid::new_v4()), now + RPC_TIMEOUT);
        let drained = table.drain();
        assert_eq!(drained.len(), 3);
        assert!(table.is_empty());
        assert_eq!(drained.iter().filter(|(_, p)| p.command_id.is_some()).count(), 1);
    }

    #[test]
    fn response_outcome_shapes() {
        assert_eq!(
            response_outcome(&json!({"id": 1, "result": {"ok": 1}})).unwrap(),
            json!({"ok": 1})
        );
        assert_eq!(response_outcome(&json!({"id": 1})).unwrap(), Value::Null);
        let err = response_outcome(&json!({"id": 1, "error": {"message": "nope"}})).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn lifecycle_request_shapes() {
        for (op, method) in [
            (LifecycleOp::Archive, "thread/archive"),
            (LifecycleOp::Unarchive, "thread/unarchive"),
            (LifecycleOp::Delete, "thread/delete"),
        ] {
            let req = thread_lifecycle_req(7, op, "thread-abc");
            assert_eq!(req["jsonrpc"], "2.0");
            assert_eq!(req["id"], 7);
            assert_eq!(req["method"], method);
            assert_eq!(req["params"]["threadId"], "thread-abc");
        }
    }

    #[test]
    fn lifecycle_idempotency_maps_already_in_state() {
        assert!(is_idempotent_lifecycle_error(
            LifecycleOp::Archive,
            "codex app-server error: thread is already archived"
        ));
        assert!(is_idempotent_lifecycle_error(LifecycleOp::Unarchive, "thread is not archived"));
        assert!(is_idempotent_lifecycle_error(LifecycleOp::Unarchive, "already unarchived"));
        assert!(is_idempotent_lifecycle_error(LifecycleOp::Delete, "already deleted"));
    }

    #[test]
    fn lifecycle_idempotency_maps_missing_thread_for_every_op() {
        for op in [LifecycleOp::Archive, LifecycleOp::Unarchive, LifecycleOp::Delete] {
            assert!(is_idempotent_lifecycle_error(op, "thread not found"));
            assert!(is_idempotent_lifecycle_error(op, "No such thread: abc"));
            assert!(is_idempotent_lifecycle_error(op, "thread does not exist"));
        }
    }

    #[test]
    fn lifecycle_idempotency_rejects_real_errors() {
        assert!(!is_idempotent_lifecycle_error(
            LifecycleOp::Archive,
            "codex app-server error 500: internal error"
        ));
        assert!(!is_idempotent_lifecycle_error(LifecycleOp::Unarchive, "permission denied"));
        // An archive-specific "already" must not mask a genuine unarchive fault.
        assert!(!is_idempotent_lifecycle_error(LifecycleOp::Unarchive, "already archived"));
    }

    #[test]
    fn handshake_methods_are_flagged() {
        for m in ["initialize", "thread/start", "thread/resume", "thread/fork"] {
            let p = PendingRpc { method: m.to_owned(), command_id: None, deadline: Instant::now() };
            assert!(p.is_handshake(), "{m}");
        }
        let p = PendingRpc {
            method: "turn/start".to_owned(),
            command_id: None,
            deadline: Instant::now(),
        };
        assert!(!p.is_handshake());
    }

    #[test]
    fn error_notification_without_retry_maps_to_failed_status() {
        let v = json!({"method": "error", "params": {
            "threadId": "t", "turnId": "u", "willRetry": false,
            "error": {"message": "usage limit exceeded", "codexErrorInfo": "usageLimitExceeded"}}});
        match classify("t", &v) {
            Incoming::Event(AdapterEvent::Status { state, detail, activity, .. }) => {
                assert_eq!(state.as_deref(), Some("failed"));
                assert_eq!(detail.as_deref(), Some("usage limit exceeded"));
                assert_eq!(activity.as_deref(), Some("failure"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn error_notification_with_retry_surfaces_detail_only() {
        let v = json!({"method": "error", "params": {
            "threadId": "t", "turnId": "u", "willRetry": true,
            "error": {"message": "server overloaded"}}});
        match classify("t", &v) {
            Incoming::Event(AdapterEvent::Status { state, detail, activity, .. }) => {
                assert_eq!(state, None);
                assert_eq!(activity, None);
                assert_eq!(detail.as_deref(), Some("server overloaded"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn failed_turn_completed_maps_to_failed_status() {
        let v = json!({"method": "turn/completed", "params": {"threadId": "t", "turn": {
            "id": "u", "items": [], "status": "failed",
            "error": {"message": "context window exceeded"}}}});
        match classify("t", &v) {
            Incoming::Event(AdapterEvent::Status { state, detail, .. }) => {
                assert_eq!(state.as_deref(), Some("failed"));
                assert_eq!(detail.as_deref(), Some("context window exceeded"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn successful_turn_completed_emits_no_status() {
        let v = json!({"method": "turn/completed", "params": {"threadId": "t", "turn": {
            "id": "u", "items": [], "status": "completed"}}});
        assert!(matches!(classify("t", &v), Incoming::Traced { .. }));
    }

    // --- active-turn routing via turn/steer ------------------------

    #[test]
    fn turn_lifecycle_parses_started_and_completed() {
        let started = json!({"method": "turn/started", "params": {"threadId": "t", "turn": {
            "id": "turn-1", "items": [], "status": "inProgress"}}});
        assert_eq!(
            turn_lifecycle(&started),
            Some(TurnLifecycle::Started { turn_id: "turn-1".to_owned() })
        );
        let completed = json!({"method": "turn/completed", "params": {"threadId": "t", "turn": {
            "id": "turn-1", "items": [], "status": "completed"}}});
        assert_eq!(
            turn_lifecycle(&completed),
            Some(TurnLifecycle::Completed { turn_id: "turn-1".to_owned() })
        );
        assert_eq!(turn_lifecycle(&json!({"method": "thread/status/changed", "params": {}})), None);
        assert_eq!(turn_lifecycle(&json!({"method": "turn/started", "params": {}})), None);
    }

    #[test]
    fn active_turn_tracks_started_then_completed() {
        let mut active = ActiveTurn::default();
        assert_eq!(active.id(), None);
        active.apply(&TurnLifecycle::Started { turn_id: "turn-1".to_owned() });
        assert_eq!(active.id(), Some("turn-1"));
        active.apply(&TurnLifecycle::Completed { turn_id: "other".to_owned() });
        assert_eq!(active.id(), Some("turn-1"));
        active.apply(&TurnLifecycle::Completed { turn_id: "turn-1".to_owned() });
        assert_eq!(active.id(), None);
    }

    #[test]
    fn active_turn_started_supersedes_previous() {
        let mut active = ActiveTurn::default();
        active.apply(&TurnLifecycle::Started { turn_id: "turn-1".to_owned() });
        active.apply(&TurnLifecycle::Started { turn_id: "turn-2".to_owned() });
        assert_eq!(active.id(), Some("turn-2"));
        active.clear();
        assert_eq!(active.id(), None);
    }

    #[test]
    fn prompt_dispatch_selects_steer_when_turn_active() {
        let mut active = ActiveTurn::default();
        assert_eq!(prompt_dispatch(&active), PromptDispatch::Start);
        active.apply(&TurnLifecycle::Started { turn_id: "turn-9".to_owned() });
        assert_eq!(
            prompt_dispatch(&active),
            PromptDispatch::Steer { turn_id: "turn-9".to_owned() }
        );
    }

    #[test]
    fn steer_recovery_rejects_non_steerable_else_falls_back() {
        assert_eq!(
            steer_recovery("codex app-server error -32000: activeTurnNotSteerable"),
            SteerRecovery::Reject
        );
        assert_eq!(steer_recovery("turn is not steerable"), SteerRecovery::Reject);
        assert_eq!(
            steer_recovery("codex app-server error -32602: expectedTurnId mismatch"),
            SteerRecovery::FallbackToStart
        );
    }

    #[test]
    fn turn_steer_req_shape() {
        let req = turn_steer_req(100, "tid", "turn-1", "keep going", &[]);
        assert_eq!(req["method"], "turn/steer");
        assert_eq!(req["id"], 100);
        assert_eq!(req["params"]["threadId"], "tid");
        assert_eq!(req["params"]["expectedTurnId"], "turn-1");
        assert_eq!(req["params"]["input"][0]["type"], "text");
        assert_eq!(req["params"]["input"][0]["text"], "keep going");
    }

    #[test]
    fn turn_interrupt_req_shape() {
        let req = turn_interrupt_req(7, "tid", "turn-1");
        assert_eq!(req["method"], "turn/interrupt");
        assert_eq!(req["id"], 7);
        assert_eq!(req["params"]["threadId"], "tid");
        assert_eq!(req["params"]["turnId"], "turn-1");
    }

    // --- outbound builders vs retained schema `required` -----------

    /// `required` param keys for `method` from the retained
    /// `ClientRequest` schema, following the `params.$ref` one level.
    fn schema_required_params(schema: &Value, method: &str) -> Vec<String> {
        let variants = schema["definitions"]["ClientRequest"]["oneOf"]
            .as_array()
            .expect("ClientRequest.oneOf");
        let variant = variants
            .iter()
            .find(|v| v["properties"]["method"]["enum"][0] == method)
            .unwrap_or_else(|| panic!("schema has no ClientRequest variant for `{method}`"));
        let params = &variant["properties"]["params"];
        let resolved = params.get("$ref").and_then(Value::as_str).map_or(params, |r| {
            schema
                .pointer(r.trim_start_matches('#'))
                .unwrap_or_else(|| panic!("unresolvable $ref {r} for `{method}`"))
        });
        resolved["required"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Every outbound request builder must carry all `required` params of
    /// its method in the retained schema, so a protocol bump that adds a
    /// required field fails here instead of as `-32602` at runtime.
    #[test]
    fn outbound_request_builders_satisfy_schema_required_params() {
        let schema: Value =
            serde_json::from_str(include_str!("schema/codex_app_server_protocol.schemas.json"))
                .expect("retained schema bundle is valid JSON");
        let reqs = [
            initialize_req(),
            thread_start_req("/cwd", &std::collections::BTreeMap::default(), None),
            thread_resume_req("tid", "/cwd", &std::collections::BTreeMap::default(), None),
            thread_fork_req("tid", "/cwd", &std::collections::BTreeMap::default(), None),
            thread_name_set_req(1, "tid", "name"),
            thread_lifecycle_req(2, LifecycleOp::Archive, "tid"),
            thread_lifecycle_req(3, LifecycleOp::Unarchive, "tid"),
            thread_lifecycle_req(4, LifecycleOp::Delete, "tid"),
            turn_start_req(5, "tid", "hi", &[], None, None),
            turn_steer_req(6, "tid", "turn-1", "hi", &[]),
            turn_interrupt_req(7, "tid", "turn-1"),
        ];
        for req in reqs {
            let method = req["method"].as_str().expect("method");
            let params = req["params"].as_object().expect("params object");
            for key in schema_required_params(&schema, method) {
                assert!(
                    params.contains_key(&key),
                    "`{method}` builder is missing required param `{key}`"
                );
            }
        }
    }

    // --- item/started + delta accumulation, new item types ---------

    const ITEM_STREAM_FIXTURE: &str = include_str!("fixtures/item_stream.jsonl");

    fn fixture_lines() -> Vec<Value> {
        ITEM_STREAM_FIXTURE
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).expect("fixture line is valid JSON"))
            .collect()
    }

    #[test]
    fn item_started_and_deltas_emit_no_event_of_their_own() {
        for v in fixture_lines() {
            let method = v.get("method").and_then(Value::as_str).unwrap_or_default();
            if method == "item/started" || method.ends_with("Delta") || method.contains("/delta") {
                assert!(
                    matches!(classify("t", &v), Incoming::Traced { .. }),
                    "{method} must not emit its own event",
                );
            }
        }
    }

    #[test]
    fn accumulator_backfills_agent_message_text() {
        // A completed agentMessage whose `text` the server left empty is
        // back-filled from the concatenated `item/agentMessage/delta` stream.
        let mut acc = ItemAccumulator::default();
        acc.note(
            &json!({"method":"item/started","params":{"item":{"id":"m","type":"agentMessage"}}}),
        );
        acc.note(
            &json!({"method":"item/agentMessage/delta","params":{"itemId":"m","delta":"Hel"}}),
        );
        acc.note(&json!({"method":"item/agentMessage/delta","params":{"itemId":"m","delta":"lo"}}));
        let completed = json!({"method":"item/completed","params":{"item":{"id":"m","type":"agentMessage","text":""}}});
        let enriched = acc.enrich_completed(completed);
        assert_eq!(enriched.pointer("/params/item/text").and_then(Value::as_str), Some("Hello"));
    }

    #[test]
    fn accumulator_keeps_authoritative_completed_text() {
        // When the completed item already carries text, the stream is dropped
        // (the completed frame is authoritative) — no duplication.
        let mut acc = ItemAccumulator::default();
        acc.note(
            &json!({"method":"item/agentMessage/delta","params":{"itemId":"m","delta":"partial"}}),
        );
        let completed = json!({"method":"item/completed","params":{"item":{"id":"m","type":"agentMessage","text":"final answer"}}});
        let enriched = acc.enrich_completed(completed);
        assert_eq!(
            enriched.pointer("/params/item/text").and_then(Value::as_str),
            Some("final answer")
        );
    }

    #[test]
    fn accumulator_backfills_reasoning_from_text_deltas() {
        // Reasoning ships `content: []` + encrypted content on completion; the
        // visible reasoning only arrived via `item/reasoning/textDelta`.
        let mut acc = ItemAccumulator::default();
        acc.note(&json!({"method":"item/started","params":{"item":{"id":"r","type":"reasoning"}}}));
        acc.note(&json!({"method":"item/reasoning/textDelta","params":{"itemId":"r","contentIndex":0,"delta":"think "}}));
        acc.note(&json!({"method":"item/reasoning/textDelta","params":{"itemId":"r","contentIndex":0,"delta":"hard"}}));
        let completed = json!({"method":"item/completed","params":{"item":{
            "id":"r","type":"reasoning","content":[],"summary":[],"encrypted_content":"gAAAA"}}});
        let enriched = acc.enrich_completed(completed);
        let content = enriched.pointer("/params/item/content").and_then(Value::as_array).unwrap();
        assert_eq!(content[0].as_str(), Some("think hard"));
    }

    #[test]
    fn accumulator_backfills_command_output() {
        let mut acc = ItemAccumulator::default();
        acc.note(&json!({"method":"item/commandExecution/outputDelta","params":{"itemId":"c","delta":"line1\n"}}));
        let completed = json!({"method":"item/completed","params":{"item":{
            "id":"c","type":"commandExecution","command":"ls","aggregatedOutput":""}}});
        let enriched = acc.enrich_completed(completed);
        assert_eq!(
            enriched.pointer("/params/item/aggregatedOutput").and_then(Value::as_str),
            Some("line1\n")
        );
    }

    #[test]
    fn accumulator_forgets_item_after_completion() {
        // A second item reusing a fresh id must not inherit a prior buffer.
        let mut acc = ItemAccumulator::default();
        acc.note(&json!({"method":"item/agentMessage/delta","params":{"itemId":"m","delta":"x"}}));
        let _ = acc.enrich_completed(
            json!({"method":"item/completed","params":{"item":{"id":"m","type":"agentMessage","text":""}}}),
        );
        // Re-completing the same id with empty text now has nothing to inject.
        let again = acc.enrich_completed(
            json!({"method":"item/completed","params":{"item":{"id":"m","type":"agentMessage","text":""}}}),
        );
        assert_eq!(again.pointer("/params/item/text").and_then(Value::as_str), Some(""));
    }

    #[test]
    fn enrich_completed_passes_non_completed_through() {
        let mut acc = ItemAccumulator::default();
        let v = json!({"method":"turn/started","params":{"turn":{"id":"t"}}});
        assert_eq!(acc.enrich_completed(v.clone()), v);
    }

    #[test]
    fn fixture_stream_drives_full_pipeline() {
        // The full started→delta→completed sequence in the fixture: only the
        // completed items emit events, deltas are accumulated, and the empty
        // reasoning item is back-filled from its text deltas.
        let mut acc = ItemAccumulator::default();
        let mut completed_types: Vec<String> = Vec::new();
        let mut reasoning_text: Option<String> = None;
        for v in fixture_lines() {
            acc.note(&v);
            let v = acc.enrich_completed(v);
            if let Incoming::Event(evt) = classify("t", &v) {
                match evt {
                    AdapterEvent::Message { payload, .. }
                    | AdapterEvent::ToolUse { payload, .. } => {
                        if let Some(ty) = payload.get("type").and_then(Value::as_str) {
                            completed_types.push(ty.to_owned());
                            if ty == "reasoning" {
                                reasoning_text = payload
                                    .get("content")
                                    .and_then(Value::as_array)
                                    .and_then(|a| a.first())
                                    .and_then(Value::as_str)
                                    .map(str::to_owned);
                            }
                        }
                    }
                    // The failed turn/completed surfaces a failed Status.
                    AdapterEvent::Status { state, .. } => {
                        assert_eq!(state.as_deref(), Some("failed"));
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
        }
        for ty in [
            "agentMessage",
            "reasoning",
            "commandExecution",
            "plan",
            "fileChange",
            "enteredReviewMode",
            "mcpToolCall",
            "dynamicToolCall",
            "imageView",
            "contextCompaction",
        ] {
            assert!(completed_types.contains(&ty.to_owned()), "missing completed item {ty}");
        }
        assert_eq!(reasoning_text.as_deref(), Some("First I will list the files."));
    }

    #[test]
    fn new_tool_items_classify_as_tool_use() {
        for ty in
            ["dynamicToolCall", "collabAgentToolCall", "webSearch", "imageView", "imageGeneration"]
        {
            let v = json!({"method":"item/completed","params":{"item":{"type":ty,"id":"x"}}});
            assert!(
                matches!(classify("s", &v), Incoming::Event(AdapterEvent::ToolUse { .. })),
                "{ty} should be a ToolUse",
            );
        }
    }

    #[test]
    fn crash_keeps_record_kill_removes_it() {
        assert!(removes_record(&EndReason::Killed));
        assert!(!removes_record(&EndReason::Crashed { detail: "boom".to_owned() }));
    }

    #[test]
    fn partition_drained_splits_retry_dropped_and_kill() {
        let cid = Uuid::new_v4();
        let drained = vec![
            SessionCommand::Send { text: "hi".to_owned(), command_id: None },
            SessionCommand::Permission { request_id: "r".to_owned(), allow: true },
            SessionCommand::Kill { signal: None },
            SessionCommand::Rename { name: "n".to_owned() },
            SessionCommand::Interrupt { command_id: Some(cid) },
        ];
        let (retry, dropped, killed) = partition_drained(drained);
        assert!(killed);
        assert_eq!(retry.len(), 2);
        assert!(matches!(&retry[0], SessionCommand::Send { text, .. } if text == "hi"));
        assert!(matches!(&retry[1], SessionCommand::Rename { name } if name == "n"));
        assert_eq!(dropped.len(), 2);
        assert_eq!(dropped[1].command_id(), Some(cid));

        let (retry, dropped, killed) = partition_drained(Vec::new());
        assert!(retry.is_empty() && dropped.is_empty() && !killed);
    }

    #[test]
    fn command_id_only_on_correlated_commands() {
        let cid = Uuid::new_v4();
        assert_eq!(
            SessionCommand::SetModel { model: None, effort: None, command_id: Some(cid) }
                .command_id(),
            Some(cid)
        );
        assert_eq!(SessionCommand::Interrupt { command_id: Some(cid) }.command_id(), Some(cid));
        assert_eq!(
            SessionCommand::Send { text: String::new(), command_id: None }.command_id(),
            None
        );
        assert_eq!(SessionCommand::Kill { signal: None }.command_id(), None);
    }

    #[tokio::test]
    async fn terminate_child_sigterm_stops_the_process() {
        let mut child = tokio::process::Command::new("sleep").arg("300").spawn().unwrap();
        terminate_child(&mut child, Some(SIGTERM));
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .expect("child survived SIGTERM")
            .unwrap();
        assert!(!status.success());
    }

    #[test]
    fn per_thread_config_carries_a_literal_bearer_not_an_env_key() {
        let env: std::collections::BTreeMap<String, String> = [
            ("OPENAI_BASE_URL".to_owned(), "https://gw.example/v1".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "SECRET-A".to_owned()),
        ]
        .into_iter()
        .collect();
        let (provider, config) = gateway_thread_config(&env).expect("gateway-bound session");
        assert_eq!(provider, "cctui");
        let p = &config["model_providers"]["cctui"];
        assert_eq!(p["base_url"], "https://gw.example/v1");
        assert_eq!(p["wire_api"], "responses");
        assert_eq!(p["http_headers"]["authorization"], "Bearer SECRET-A");
        assert_eq!(p["http_headers"]["x-openai-actor-authorization"], "cctui-gateway");
        assert!(p.get("env_key").is_none(), "a literal bearer must not also indirect via env");
    }

    /// Without a credential the definition must still be emitted, or codex
    /// fails config load on a rollout that persisted `model_provider`.
    #[test]
    fn per_thread_config_falls_back_to_env_key_without_a_credential() {
        let env: std::collections::BTreeMap<String, String> =
            std::iter::once(("OPENAI_BASE_URL".to_owned(), "https://gw.example/v1".to_owned()))
                .collect();
        let (_, config) = gateway_thread_config(&env).expect("gateway-bound session");
        let p = &config["model_providers"]["cctui"];
        assert_eq!(p["env_key"], "OPENAI_API_KEY");
        assert!(p["http_headers"].get("authorization").is_none());
    }

    #[test]
    fn an_unbound_session_keeps_the_default_provider() {
        assert!(gateway_thread_config(&std::collections::BTreeMap::new()).is_none());
        let req = thread_start_req("/tmp", &std::collections::BTreeMap::new(), None);
        assert!(req["params"].get("config").is_none());
        assert!(req["params"].get("modelProvider").is_none());
    }

    #[test]
    fn start_resume_and_fork_all_carry_the_per_thread_credential() {
        let env: std::collections::BTreeMap<String, String> = [
            ("OPENAI_BASE_URL".to_owned(), "https://gw.example/v1".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "SECRET-B".to_owned()),
        ]
        .into_iter()
        .collect();
        for req in [
            thread_start_req("/repo", &env, None),
            thread_resume_req("tid", "/repo", &env, None),
            thread_fork_req("tid", "/repo", &env, None),
        ] {
            let params = &req["params"];
            assert_eq!(params["modelProvider"], "cctui", "{}", req["method"]);
            assert_eq!(
                params["config"]["model_providers"]["cctui"]["http_headers"]["authorization"],
                "Bearer SECRET-B",
                "{}",
                req["method"]
            );
        }
    }

    #[test]
    fn service_tier_normalization_accepts_only_the_two_codex_tiers() {
        assert_eq!(normalize_service_tier(Some("fast")).as_deref(), Some("fast"));
        assert_eq!(normalize_service_tier(Some(" FAST ")).as_deref(), Some("fast"));
        assert_eq!(normalize_service_tier(Some("default")).as_deref(), Some("default"));
        assert_eq!(normalize_service_tier(Some("priority")), None);
        assert_eq!(normalize_service_tier(Some("")), None);
        assert_eq!(normalize_service_tier(None), None);
    }

    #[test]
    fn service_tier_reads_out_of_the_served_gateway_settings() {
        assert_eq!(
            service_tier_from_settings(Some(&json!({"service_tier": "fast"}))).as_deref(),
            Some("fast")
        );
        assert_eq!(
            service_tier_from_settings(Some(&json!({"service_tier": "default"}))).as_deref(),
            Some("default")
        );
        assert_eq!(service_tier_from_settings(Some(&json!({}))), None);
        assert_eq!(service_tier_from_settings(None), None);
    }

    #[test]
    fn start_resume_and_fork_all_carry_the_per_session_service_tier() {
        let env = std::collections::BTreeMap::default();
        for tier in ["fast", "default"] {
            for req in [
                thread_start_req("/repo", &env, Some(tier)),
                thread_resume_req("tid", "/repo", &env, Some(tier)),
                thread_fork_req("tid", "/repo", &env, Some(tier)),
            ] {
                let params = &req["params"];
                assert_eq!(params["config"]["service_tier"], tier, "{}", req["method"]);
                assert_eq!(params["serviceTier"], tier, "{}", req["method"]);
            }
        }
    }

    #[test]
    fn a_session_with_no_tier_supplies_none_on_any_thread_op() {
        let env = std::collections::BTreeMap::default();
        for req in [
            thread_start_req("/repo", &env, None),
            thread_resume_req("tid", "/repo", &env, None),
            thread_fork_req("tid", "/repo", &env, None),
        ] {
            let params = &req["params"];
            assert!(params.get("serviceTier").is_none(), "{}", req["method"]);
            assert!(
                params.get("config").and_then(|c| c.get("service_tier")).is_none(),
                "{}",
                req["method"]
            );
        }
    }

    #[test]
    fn the_gateway_config_block_and_the_tier_coexist() {
        let env: std::collections::BTreeMap<String, String> = [
            ("OPENAI_BASE_URL".to_owned(), "https://gw.example/v1".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "SECRET-C".to_owned()),
        ]
        .into_iter()
        .collect();
        let params = &thread_resume_req("tid", "/repo", &env, Some("fast"))["params"];
        assert_eq!(params["config"]["service_tier"], "fast");
        assert_eq!(
            params["config"]["model_providers"]["cctui"]["http_headers"]["authorization"],
            "Bearer SECRET-C"
        );
    }

    fn session_with_tier(launch: SessionLaunch, tier: Option<&str>) -> CodexSession {
        let (events, _rx) = mpsc::channel(8);
        CodexSession {
            cfg: AppServerConfig {
                service_tier: tier.map(str::to_owned),
                ..AppServerConfig::default()
            },
            cwd: "/repo".to_owned(),
            env: std::collections::BTreeMap::default(),
            launch,
            command_id: None,
            spawn_key: None,
            parent_local_id: None,
            agent_mcp: None,
            events,
            live: LiveSessionRegistry::default(),
            registry: SessionRegistry::default(),
            shutdown: CancellationToken::new(),
        }
    }

    /// The resume trap: a `thread/resume` does not re-pull the gateway env, and
    /// codex persists no tier in the rollout, so the tier cached on the record
    /// must ride every resume or Fast lapses silently after the first one.
    #[test]
    fn a_resumed_session_re_supplies_the_cached_tier() {
        let (req, method) = session_with_tier(
            SessionLaunch::Resume { thread_id: "tid".to_owned(), initial_commands: Vec::new() },
            Some("fast"),
        )
        .thread_request();
        assert_eq!(method, "thread/resume");
        assert_eq!(req["params"]["config"]["service_tier"], "fast");
        assert_eq!(req["params"]["serviceTier"], "fast");
    }

    #[test]
    fn a_resumed_session_without_a_cached_tier_supplies_none() {
        let (req, _) = session_with_tier(
            SessionLaunch::Resume { thread_id: "tid".to_owned(), initial_commands: Vec::new() },
            None,
        )
        .thread_request();
        assert!(req["params"].get("serviceTier").is_none());
    }

    #[test]
    fn a_forked_session_re_supplies_the_cached_tier() {
        let (req, method) = session_with_tier(
            SessionLaunch::Fork {
                parent_thread_id: "parent".to_owned(),
                prompt: None,
                name: None,
                attachments: Vec::new(),
            },
            Some("fast"),
        )
        .thread_request();
        assert_eq!(method, "thread/fork");
        assert_eq!(req["params"]["config"]["service_tier"], "fast");
    }

    #[test]
    fn a_fresh_session_carries_the_tier_on_thread_start() {
        let (req, method) = session_with_tier(
            SessionLaunch::Fresh { prompt: None, name: None, attachments: Vec::new() },
            Some("default"),
        )
        .thread_request();
        assert_eq!(method, "thread/start");
        assert_eq!(req["params"]["config"]["service_tier"], "default");
        assert_eq!(req["params"]["serviceTier"], "default");
    }

    /// The record the registry stores after a launch is what a later resume
    /// relaunches from, so the tier must survive that round-trip.
    #[tokio::test]
    async fn a_hibernated_record_hands_its_tier_to_the_resume() {
        let registry = SessionRegistry::default();
        registry.lock().await.insert(
            "tid".to_owned(),
            SessionRecord {
                cfg: AppServerConfig {
                    service_tier: Some("fast".to_owned()),
                    ..AppServerConfig::default()
                },
                cwd: "/repo".to_owned(),
                name: None,
                env: std::collections::BTreeMap::default(),
                spawn_relay: false,
            },
        );
        let action = route_or_prepare_resume(
            &LiveSessionRegistry::default(),
            &registry,
            "tid",
            SessionCommand::Send { text: "hi".to_owned(), command_id: None },
        )
        .await;
        let RouteAction::Resume { record, .. } = action else { panic!("expected a resume") };
        let (req, _) = session_with_tier(
            SessionLaunch::Resume { thread_id: "tid".to_owned(), initial_commands: Vec::new() },
            record.cfg.service_tier.as_deref(),
        )
        .thread_request();
        assert_eq!(req["params"]["config"]["service_tier"], "fast");
    }

    /// The per-thread tier must never leak into the process-level `-c` flags.
    #[test]
    fn config_overrides_stay_free_of_the_tier_even_when_one_is_set() {
        let cfg =
            AppServerConfig { service_tier: Some("fast".to_owned()), ..AppServerConfig::default() };
        assert!(cfg.config_overrides().iter().all(|(k, _)| k != "service_tier"));
    }

    /// The per-op lifecycle child is gone: archive/unarchive are answered over
    /// the shared socket with no `codex` binary present to spawn.
    #[tokio::test]
    async fn lifecycle_ops_never_spawn_a_child_when_the_daemon_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("app-server.sock");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let _srv = super::super::daemon::testserver::spawn(&sock, move |method, params| {
            recorder.lock().unwrap().push(method.to_owned());
            assert_eq!(params["threadId"], "tid-1");
            json!({})
        });

        let shutdown = CancellationToken::new();
        let shared = super::super::daemon::SharedDaemon::from_endpoint(
            super::super::daemon::DaemonEndpoint { socket: sock },
            shutdown.clone(),
        );
        let mut app = AppServerConfig::from_value(&json!({}));
        app.bin = "/nonexistent/codex-must-not-be-spawned".to_owned();

        for op in [LifecycleOp::Archive, LifecycleOp::Unarchive] {
            run_thread_lifecycle(&app, Some(&shared), "tid-1", op)
                .await
                .expect("served over the ws");
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["thread/archive".to_owned(), "thread/unarchive".to_owned()]
        );
        shutdown.cancel();
    }
}
