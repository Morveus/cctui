use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use cctui_proto::ws::{AgentEvent, ServerEvent, TuiCommand};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::auth::{AuthContext, Scope};
use crate::state::AppState;

/// Authorize a WS command against a session for the connected principal,
/// mirroring the HTTP ownership gate (`spawn.rs`/`admin.rs`): admins always
/// pass; everyone else must own the session (resolved via
/// `machine_uuid -> machines.user_id`). A session whose owner can't be resolved
/// is denied for non-admins rather than leaked. Returns `true` when permitted.
async fn ws_owns_session(state: &AppState, ctx: &AuthContext, session_id: &str) -> bool {
    if ctx.is_admin() {
        return true;
    }
    // Reuse the exact same ownership query as the HTTP `Resource(Session)` guard
    // (`machine_uuid -> machines.user_id`) so the two transports never drift.
    // A DB error or unknown session resolves to "not owned".
    let owner = crate::authz::session_owner(session_id, &state.pool).await.unwrap_or_else(|e| {
        tracing::error!(%session_id, "db error (ws session authz): {e}");
        None
    });
    owner == Some(ctx.user_id)
}

// --- TUI WebSocket ---

pub async fn tui_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    // CSWSH defense-in-depth: reject a cross-origin browser upgrade before auth.
    if !origin_permitted(&state.config, &headers) {
        return Err(StatusCode::FORBIDDEN);
    }

    // Browser WS upgrades are same-origin GETs that carry the `HttpOnly` auth
    // cookie automatically, so the token stays out of the query string (where it
    // would leak into access logs). `bearer_or_cookie` also accepts an
    // `Authorization` header for non-browser clients.
    let token = crate::auth::bearer_or_cookie(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let auth_ctx = state.auth_config.validate(&token).await.ok_or(StatusCode::UNAUTHORIZED)?;

    // The TUI/webui socket is for human identities (a user or admin token), not
    // a machine key. Gate on the `read` scope and the absence of a machine id.
    if auth_ctx.machine_id.is_some() || !auth_ctx.has(Scope::Read) {
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(ws.on_upgrade(move |socket| handle_tui_ws(socket, state, auth_ctx)))
}

/// A present `Origin` must be in the allowlist; an absent one is a non-browser
/// client (TUI/daemon) and is allowed. An unparseable `Origin` is rejected.
fn origin_permitted(config: &crate::config::Config, headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::ORIGIN)
        .is_none_or(|value| value.to_str().is_ok_and(|o| config.origin_allowed(o)))
}

/// Mirrors the daemon socket's cadence (`routes/daemon.rs`) and stays well
/// under the gateway's idle timeout.
const TUI_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(20);

/// Besides forwarding events, the outbound pump sends a periodic `Ping`: a
/// browser that slept leaves a half-open socket that never errors on read, so
/// only a write failure retires this task and releases its relays. The paired
/// [`ServerEvent::Heartbeat`] carries the same tick to the browser, which cannot
/// observe a `Ping`; it is written per-socket here rather than broadcast, so it
/// reaches every client whether or not any daemon is online.
fn spawn_send_task(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<ServerEvent>,
) {
    tokio::spawn(async move {
        let mut keepalive = tokio::time::interval(TUI_KEEPALIVE);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        keepalive.tick().await;
        loop {
            tokio::select! {
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let text = match serde_json::to_string(&event) {
                        Ok(t) => t,
                        Err(err) => {
                            tracing::warn!(%err, "failed to serialize ServerEvent");
                            continue;
                        }
                    };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                _ = keepalive.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                    let beat = serde_json::to_string(&ServerEvent::Heartbeat {})
                        .expect("Heartbeat serializes");
                    if sink.send(Message::Text(beat.into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn spawn_relay_task(
    mut receiver: tokio::sync::broadcast::Receiver<AgentEvent>,
    session_id: String,
    event_tx: mpsc::Sender<ServerEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(agent_event) => {
                    let server_event =
                        ServerEvent::Stream { session_id: session_id.clone(), data: agent_event };
                    if event_tx.send(server_event).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(session_id = %session_id, skipped = n, "TUI receiver lagged");
                }
            }
        }
    })
}

/// Dispatch a client-typed reply to the session's daemon and, when the client
/// opted in with a `client_msg_id`, ack the outcome back to this socket so the
/// UI can show a precise delivery state (sending → delivered / failed) instead
/// of optimistically assuming a sent frame was delivered.
async fn handle_message(
    state: &AppState,
    event_tx: &mpsc::Sender<ServerEvent>,
    session_id: String,
    content: String,
    client_msg_id: Option<String>,
    ask_picks: Option<Vec<Vec<usize>>>,
    turn_id: Option<uuid::Uuid>,
) {
    // NoDaemon / NoAdapter are expected for sessions whose daemon is momentarily
    // offline — that is exactly the case the ack lets the client recover from.
    // Carry re-minted gateway env on the reply so a reply-driven cold-resume of
    // a hibernated worker revives it with a fresh valid token rather than empty
    // env. Ignored when the worker is already alive.
    let env = crate::routes::gateway::resume_env_for_session(state, &session_id).await;
    // A successful dispatch only means the frame was queued toward a daemon; the
    // adapter's `CommandResult` under this id is the delivery proof.
    let command_id = uuid::Uuid::new_v4();
    crate::state::track_command(
        &state.pending_commands,
        command_id,
        Some(session_id.clone()),
        None,
    );
    let dispatch = crate::bus::dispatch(
        state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::Reply {
            local_id: session_id.clone(),
            text: content,
            ask_picks,
            env,
            command_id: Some(command_id),
            turn_id,
        },
    )
    .await;
    let err_reason = dispatch.as_ref().err().map(|err| {
        use crate::bus::BusError;
        match err {
            BusError::NoDaemon(_) | BusError::NoAdapter | BusError::NotFound => {
                tracing::debug!(%session_id, ?err, "daemon dispatch skipped");
            }
            _ => tracing::warn!(%session_id, %err, "daemon dispatch failed"),
        }
        err.to_string()
    });
    if let Some(client_msg_id) = client_msg_id {
        let _ = event_tx
            .send(ServerEvent::MessageAck {
                session_id,
                client_msg_id,
                ok: err_reason.is_none(),
                error: err_reason,
                command_id: Some(command_id),
            })
            .await;
    }
}

async fn handle_subscribe(
    session_id: String,
    state: &AppState,
    event_tx: &mpsc::Sender<ServerEvent>,
    sub_handles: &mut std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
) {
    let receiver = state.bus.subscribe_session(&session_id);

    // Replay any prompt the session is currently blocked on. Asks and
    // permission requests were originally fire-and-forget broadcasts: a client
    // that wasn't subscribed at the instant one went out never learned about it,
    // and the client re-subscribes on every tab focus/visibility change
    // — so a backgrounded tab routinely missed them. The store now
    // holds them authoritatively; re-send them to *this* socket so a (re)subscribe
    // always re-surfaces the live prompt. Deduped client-side by request_id /
    // overwrite, so a replay that races the live broadcast is harmless.
    {
        let store = state.permission_store.read().await;
        for p in store.list_pending().into_iter().filter(|p| p.session_id == session_id) {
            let _ = event_tx
                .send(ServerEvent::PermissionRequest {
                    session_id: p.session_id,
                    request_id: p.request_id,
                    tool_name: p.tool_name,
                    description: p.description,
                    input_preview: p.input_preview,
                })
                .await;
        }
        if let Some(ask) = store.pending_ask(&session_id) {
            let _ = event_tx
                .send(ServerEvent::AskQuestion {
                    session_id: ask.session_id,
                    question: ask.question,
                    questions: ask.questions,
                    preamble: ask.preamble,
                })
                .await;
        }
        if let Some(plan) = store.pending_plan(&session_id) {
            let _ = event_tx
                .send(ServerEvent::PlanRequest {
                    session_id: plan.session_id,
                    plan: plan.plan,
                    preamble: plan.preamble,
                })
                .await;
        }
    }

    if let Some(receiver) = receiver {
        let handle = spawn_relay_task(receiver, session_id.clone(), event_tx.clone());
        // Abort any prior relay task for this session on this socket before
        // replacing it. The client re-subscribes on every tab focus/visibility
        // change; without this, each resubscribe leaked an extra
        // relay task that re-delivered every event, duplicating chat messages.
        if let Some(old) = sub_handles.insert(session_id, handle) {
            old.abort();
        }
    } else {
        // Historical/terminated sessions won't be in the registry — this is expected
        tracing::debug!(session_id = %session_id, "tui_ws: session not in registry (historical)");
    }
}

#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
async fn run_tui_socket(
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    state: AppState,
    ctx: AuthContext,
    event_tx: mpsc::Sender<ServerEvent>,
) {
    // Relay tasks keyed by session id, so a resubscribe replaces (not stacks)
    // the per-session relay and an unsubscribe can tear it down.
    let mut sub_handles: std::collections::HashMap<String, tokio::task::JoinHandle<()>> =
        std::collections::HashMap::new();
    // Sessions whose live terminal THIS socket is watching. Tracked
    // per-socket so a disconnect decrements the shared watcher refcount and the
    // daemon stops streaming to a browser that vanished without unwatching.
    let mut pty_watches: std::collections::HashSet<String> = std::collections::HashSet::new();

    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };

        let cmd: TuiCommand = match serde_json::from_str(&text) {
            Ok(c) => c,
            Err(err) => {
                tracing::warn!(%err, "failed to parse TuiCommand");
                continue;
            }
        };

        match cmd {
            TuiCommand::Subscribe { session_id } => {
                // Only stream a session this principal owns (admin bypasses).
                // Pending-ask/permission replay lives inside handle_subscribe,
                // so gating here keeps the whole replay path owner-scoped too.
                if !ws_owns_session(&state, &ctx, &session_id).await {
                    tracing::debug!(session_id = %session_id, user_id = %ctx.user_id, "tui_ws: subscribe denied (not owner)");
                    continue;
                }
                handle_subscribe(session_id, &state, &event_tx, &mut sub_handles).await;
            }
            TuiCommand::Unsubscribe { session_id } => {
                // Tear down this session's relay task so it stops delivering
                // events to this socket.
                if let Some(handle) = sub_handles.remove(&session_id) {
                    handle.abort();
                }
            }
            TuiCommand::WatchTerminal { session_id, watch } => {
                if !ws_owns_session(&state, &ctx, &session_id).await {
                    tracing::debug!(session_id = %session_id, user_id = %ctx.user_id, "tui_ws: watch-terminal denied (not owner)");
                    continue;
                }
                handle_watch_terminal(&state, &session_id, watch, &mut pty_watches).await;
            }
            TuiCommand::Message { session_id, content, client_msg_id, ask_picks, turn_id } => {
                if !ws_owns_session(&state, &ctx, &session_id).await {
                    tracing::debug!(session_id = %session_id, user_id = %ctx.user_id, "tui_ws: message denied (not owner)");
                    // Ack the failure when the client opted in, so it doesn't
                    // hang waiting on a delivery state for a denied send.
                    if let Some(client_msg_id) = client_msg_id {
                        let _ = event_tx
                            .send(ServerEvent::MessageAck {
                                session_id,
                                client_msg_id,
                                ok: false,
                                error: Some("forbidden".into()),
                                command_id: None,
                            })
                            .await;
                    }
                    continue;
                }
                handle_message(
                    &state,
                    &event_tx,
                    session_id,
                    content,
                    client_msg_id,
                    ask_picks,
                    turn_id,
                )
                .await;
            }
            TuiCommand::PermissionResponse { session_id, request_id, behavior } => {
                // Authorize against the session id the client supplied. The
                // store's record_decision may re-resolve to a stored session id
                // below, but the principal must own the one they're acting on.
                if !ws_owns_session(&state, &ctx, &session_id).await {
                    tracing::debug!(session_id = %session_id, user_id = %ctx.user_id, "tui_ws: permission-response denied (not owner)");
                    continue;
                }
                tracing::info!(
                    session_id = %session_id,
                    request_id = %request_id,
                    behavior = %behavior,
                    "TUI permission response received"
                );
                let allow = {
                    let b = behavior.to_ascii_lowercase();
                    b.starts_with("allow") || b == "accept" || b == "approved"
                };
                let stored_session_id =
                    state.permission_store.write().await.record_decision(&request_id, behavior);
                // Prefer the id attached at submission; fall back to the one
                // the client sent (stale / unknown request_id cases).
                let resolved_session_id =
                    if stored_session_id.is_empty() { session_id } else { stored_session_id };
                // Push the decision down to the adapter so blocking agents
                // (e.g. the codex app-server, which holds the turn open until
                // it gets a reply) are unblocked.
                let dispatch = crate::bus::dispatch(
                    &state,
                    &resolved_session_id,
                    cctui_proto::adapter::AdapterCommand::PermissionResponse {
                        local_id: resolved_session_id.clone(),
                        request_id: request_id.clone(),
                        allow,
                    },
                )
                .await;
                if let Err(err) = dispatch {
                    use crate::bus::BusError;
                    match err {
                        BusError::NoDaemon(_) | BusError::NoAdapter | BusError::NotFound => {
                            tracing::debug!(%resolved_session_id, ?err, "permission dispatch skipped");
                        }
                        _ => {
                            tracing::warn!(%resolved_session_id, %err, "permission dispatch failed");
                        }
                    }
                }
                state.bus.publish_server(ServerEvent::PermissionResolved {
                    session_id: resolved_session_id,
                    request_id,
                });
            }
        }
    }

    for (_, handle) in sub_handles {
        handle.abort();
    }
    // Decrement every terminal this socket still watched so a browser that
    // closed the tab (or dropped) releases the daemon PTY stream.
    for session_id in pty_watches {
        if state.bus.pty_watch_dec(&session_id) {
            set_daemon_pty_watch(&state, &session_id, false).await;
        }
    }
}

/// Toggle this socket's live-terminal watch of `session_id`. Ref-count
/// per session is on the bus; only the 0↔1 edge tells the daemon to start/stop
/// its viewer PTY attach. Idempotent per socket via `pty_watches`.
async fn handle_watch_terminal(
    state: &AppState,
    session_id: &str,
    watch: bool,
    pty_watches: &mut std::collections::HashSet<String>,
) {
    if watch {
        if !pty_watches.insert(session_id.to_owned()) {
            return;
        }
        if state.bus.pty_watch_inc(session_id) {
            set_daemon_pty_watch(state, session_id, true).await;
        }
    } else {
        if !pty_watches.remove(session_id) {
            return;
        }
        if state.bus.pty_watch_dec(session_id) {
            set_daemon_pty_watch(state, session_id, false).await;
        }
    }
}

/// Tell the session's daemon to start/stop relaying its PTY. Best-effort: a
/// session whose daemon is momentarily offline just gets no stream (the browser
/// re-sends `watch` on reconnect), so `NoDaemon`/`NotFound` are logged at debug.
async fn set_daemon_pty_watch(state: &AppState, session_id: &str, watch: bool) {
    let dispatch = crate::bus::dispatch(
        state,
        session_id,
        cctui_proto::adapter::AdapterCommand::WatchPty { local_id: session_id.to_owned(), watch },
    )
    .await;
    if let Err(err) = dispatch {
        tracing::debug!(%session_id, watch, %err, "watch-terminal daemon dispatch skipped");
    }
}

/// The `session_id` a server-initiated event pertains to, if any. Events with a
/// session id are owner-scoped on the relay; the rest
/// (`CommandResult`, machine-level/manifest events) are not session-scoped and
/// pass through. `MessageAck` is already point-to-point (sent only to the
/// originating socket via `event_tx`, not broadcast), but we still scope it
/// defensively.
fn event_session_id(event: &ServerEvent) -> Option<&str> {
    match event {
        ServerEvent::Stream { session_id, .. }
        | ServerEvent::Status { session_id, .. }
        | ServerEvent::SessionDeregistered { session_id }
        | ServerEvent::PermissionRequest { session_id, .. }
        | ServerEvent::PermissionResolved { session_id, .. }
        | ServerEvent::AskQuestion { session_id, .. }
        | ServerEvent::AskResolved { session_id }
        | ServerEvent::PlanRequest { session_id, .. }
        | ServerEvent::PlanResolved { session_id }
        | ServerEvent::PtyChunk { session_id, .. }
        | ServerEvent::SessionEnded { session_id, .. }
        | ServerEvent::MessageAck { session_id, .. } => Some(session_id),
        ServerEvent::SessionRegistered { session } => Some(&session.id),
        ServerEvent::CommandResult { session_id, .. } => session_id.as_deref(),
        _ => None,
    }
}

fn spawn_server_event_relay(
    mut receiver: tokio::sync::broadcast::Receiver<ServerEvent>,
    state: AppState,
    ctx: AuthContext,
    event_tx: mpsc::Sender<ServerEvent>,
) {
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    // Drop session-scoped events for sessions this principal
                    // doesn't own (admin bypasses). Non-session events pass.
                    if let Some(session_id) = event_session_id(&event)
                        && !ws_owns_session(&state, &ctx, session_id).await
                    {
                        continue;
                    }
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "TUI server-event relay lagged");
                }
            }
        }
    });
}

async fn handle_tui_ws(socket: WebSocket, state: AppState, ctx: AuthContext) {
    let (sink, stream) = socket.split();
    let (tx, rx) = mpsc::channel::<ServerEvent>(256);

    // Relay server-initiated events (e.g. permission requests) to this TUI
    // client, scoped to sessions the principal owns (admin sees all).
    spawn_server_event_relay(state.bus.subscribe_server(), state.clone(), ctx.clone(), tx.clone());

    spawn_send_task(sink, rx);
    run_tui_socket(stream, state, ctx, tx).await;

    tracing::debug!("TUI WebSocket disconnected");
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use cctui_proto::models::{Session, SessionStatus};
    use cctui_proto::ws::ServerEvent;

    use super::{event_session_id, origin_permitted};
    use crate::config::Config;

    fn cfg() -> Config {
        Config::for_test(vec!["https://cctui.example.com".to_owned()])
    }

    #[test]
    fn absent_origin_is_allowed() {
        assert!(origin_permitted(&cfg(), &HeaderMap::new()));
    }

    #[test]
    fn allowlisted_origin_is_allowed() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://cctui.example.com"));
        assert!(origin_permitted(&cfg(), &headers));
    }

    #[test]
    fn foreign_origin_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://evil.example.com"));
        assert!(!origin_permitted(&cfg(), &headers));
    }

    fn session(id: &str) -> Session {
        let now = chrono::Utc::now();
        Session {
            id: id.to_owned(),
            parent_id: None,
            account_id: None,
            machine_id: "m1".to_owned(),
            working_dir: "/tmp".to_owned(),
            status: SessionStatus::Active,
            registered_at: now,
            last_heartbeat: now,
            metadata: serde_json::json!({}),
            adapter_id: None,
        }
    }

    /// The relay drops session-scoped events for non-owners, so a registration
    /// that reported no session id would fan out to every connected principal.
    #[test]
    fn session_registered_is_owner_scoped() {
        let event = ServerEvent::SessionRegistered { session: session("sess-1") };
        assert_eq!(event_session_id(&event), Some("sess-1"));
    }

    #[test]
    fn session_deregistered_is_owner_scoped() {
        let event = ServerEvent::SessionDeregistered { session_id: "sess-2".to_owned() };
        assert_eq!(event_session_id(&event), Some("sess-2"));
    }
}
