//! Persistent connection to the shared `codex app-server` daemon.
//!
//! The control socket at `$CODEX_HOME/app-server-control/app-server-control.sock`
//! speaks **WebSocket, not newline-delimited JSON**: a bare `connect()` +
//! `write(json)` is dropped by the server with `failed to upgrade control
//! socket websocket connection`. This is undocumented upstream and is the one
//! thing that makes this module more than a socket swap — after the HTTP/1.1
//! `Upgrade` the JSON-RPC is byte-identical to what [`super::app_server`]
//! writes over stdio.
//!
//! Reads (`thread/list`, `thread/read`, `thread/turns/list`) and the
//! `thread/{archive,unarchive}` lifecycle ops all answer unauthenticated,
//! which is what lets one connection serve every session's inventory
//! regardless of which account owns the thread.
//!
//! Responses and notifications interleave on the one socket, so requests are
//! correlated by JSON-RPC id and everything else fans out to
//! [`DaemonHandle::subscribe`].
//!
//! A session can also run its turns here through a [`ThreadWire`]: a raw
//! JSON-RPC pipe scoped to one thread, byte-compatible with the stdio child.
//! Unlike a read, a thread route outlives a drop: on reconnect it rejoins its
//! thread with `thread/resume` (re-supplying its [`ThreadConfig`]) and
//! reconciles a `turn/start` whose answer the drop swallowed, so a running
//! turn is neither lost nor duplicated.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::app_server::ThreadConfig;

/// Every frame on this socket is mirrored into the shared diagnose ring,
/// tagged `shared`, so the protocol tail does not go blind on the transport
/// that carries all inventory/lifecycle/history traffic.
fn rings() -> &'static Arc<super::app_server::DiagnoseRings> {
    super::app_server::shared_rings()
}

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Sized so a slow consumer lags (and learns of it via
/// [`broadcast::error::RecvError::Lagged`]) rather than stalling the read loop
/// for every other consumer.
const NOTIFY_BUFFER: usize = 1024;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> i64 {
    i64::try_from(NEXT_ID.fetch_add(1, Ordering::Relaxed)).unwrap_or(i64::MAX)
}

/// Where a shared app-server can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonEndpoint {
    pub socket: PathBuf,
}

impl DaemonEndpoint {
    /// `codex app-server daemon start` is idempotent and prints its state as
    /// JSON, so it doubles as discovery: it yields the socket path *and*
    /// guarantees the daemon is up. Preferred over deriving the path from
    /// `CODEX_HOME`, which cannot do the latter.
    pub async fn discover(bin: &str) -> Result<Self> {
        if let Some(path) = std::env::var_os("CCTUI_CODEX_APP_SERVER_SOCK") {
            return Ok(Self { socket: PathBuf::from(path) });
        }
        let mut cmd = tokio::process::Command::new(bin);
        cmd.arg("app-server")
            .arg("daemon")
            .arg("start")
            .env("PATH", crate::childenv::child_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::childenv::ScrubChildEnv::scrub_child_env(&mut cmd);
        let out = tokio::time::timeout(RPC_TIMEOUT, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("codex app-server daemon start timed out"))??;
        anyhow::ensure!(out.status.success(), "codex app-server daemon start failed");
        parse_daemon_start(&String::from_utf8_lossy(&out.stdout))
    }
}

/// Scan for the first line that parses as JSON and carries a `socketPath`, so
/// a leading warning line does not defeat discovery.
pub fn parse_daemon_start(stdout: &str) -> Result<DaemonEndpoint> {
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if let Some(path) = v.get("socketPath").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            return Ok(DaemonEndpoint { socket: PathBuf::from(path) });
        }
    }
    anyhow::bail!("codex app-server daemon start reported no socketPath")
}

#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Notification {
        method: String,
        params: Value,
    },
    /// Generation increments on every successful connect. Consumers holding
    /// derived state must resynchronize from a full snapshot: notifications
    /// emitted while the socket was down were delivered to nobody, so only a
    /// fresh read can close the gap.
    Connected {
        generation: u64,
    },
    Disconnected {
        generation: u64,
    },
}

enum Op {
    Request { method: String, params: Value, reply: oneshot::Sender<Result<Value>> },
    Open { route: u64, frames: mpsc::UnboundedSender<Value>, config: ThreadConfig, cwd: String },
    Frame { route: u64, frame: Value },
    Close { route: u64 },
}

static NEXT_ROUTE: AtomicU64 = AtomicU64::new(1);

/// Cloneable handle; every clone talks to the same socket.
#[derive(Clone)]
pub struct DaemonHandle {
    ops: mpsc::Sender<Op>,
    events: broadcast::Sender<DaemonEvent>,
    connected: Arc<AtomicBool>,
}

/// One session's JSON-RPC pipe over the shared connection. Frames are what the
/// stdio child would have printed; `frames` closing is that child's EOF.
pub struct ThreadWire {
    pub sink: ThreadSink,
    pub frames: mpsc::UnboundedReceiver<Value>,
}

/// Dropping it closes the route.
pub struct ThreadSink {
    route: u64,
    ops: mpsc::Sender<Op>,
}

impl ThreadSink {
    pub async fn send(&self, frame: &Value) -> Result<()> {
        self.ops
            .send(Op::Frame { route: self.route, frame: frame.clone() })
            .await
            .map_err(|_| anyhow::anyhow!("codex daemon connection closed"))
    }
}

impl Drop for ThreadSink {
    fn drop(&mut self) {
        let _ = self.ops.try_send(Op::Close { route: self.route });
    }
}

impl DaemonHandle {
    /// Fails rather than blocking when the connection is down, so callers can
    /// fall back to their own transport.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        self.ops
            .send(Op::Request { method: method.to_owned(), params, reply: tx })
            .await
            .map_err(|_| anyhow::anyhow!("codex daemon connection closed"))?;
        rx.await.map_err(|_| anyhow::anyhow!("codex daemon dropped request `{method}`"))?
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// `config` and `cwd` are what a rejoin re-sends in its `thread/resume`.
    pub async fn open_thread(&self, config: ThreadConfig, cwd: String) -> Result<ThreadWire> {
        let route = NEXT_ROUTE.fetch_add(1, Ordering::Relaxed);
        let (frames_tx, frames) = mpsc::unbounded_channel();
        self.ops
            .send(Op::Open { route, frames: frames_tx, config, cwd })
            .await
            .map_err(|_| anyhow::anyhow!("codex daemon connection closed"))?;
        Ok(ThreadWire { sink: ThreadSink { route, ops: self.ops.clone() }, frames })
    }
}

/// Spawn the supervisor and hand back a handle. The handle is usable
/// immediately: requests issued before the first connect fail, and callers
/// fall back.
#[must_use]
pub fn connect(endpoint: DaemonEndpoint, shutdown: CancellationToken) -> DaemonHandle {
    let (ops_tx, ops_rx) = mpsc::channel(256);
    let (events_tx, _) = broadcast::channel(NOTIFY_BUFFER);
    let connected = Arc::new(AtomicBool::new(false));
    let handle =
        DaemonHandle { ops: ops_tx, events: events_tx.clone(), connected: connected.clone() };
    tokio::spawn(supervise(endpoint, ops_rx, events_tx, connected, shutdown));
    handle
}

async fn supervise(
    endpoint: DaemonEndpoint,
    mut ops: mpsc::Receiver<Op>,
    events: broadcast::Sender<DaemonEvent>,
    connected: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    let mut backoff = BACKOFF_MIN;
    let mut generation = 0_u64;
    let mut routes = Routes::default();
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        match handshake(&endpoint).await {
            Ok((stream, init)) => {
                routes.init = Some(init);
                generation += 1;
                backoff = BACKOFF_MIN;
                connected.store(true, Ordering::Relaxed);
                let _ = events.send(DaemonEvent::Connected { generation });
                tracing::info!(
                    socket = %endpoint.socket.display(),
                    generation,
                    "codex: shared app-server connection established"
                );
                pump(stream, &mut ops, &mut routes, &events, &shutdown).await;
                connected.store(false, Ordering::Relaxed);
                let _ = events.send(DaemonEvent::Disconnected { generation });
                if shutdown.is_cancelled() {
                    return;
                }
                tracing::warn!(generation, "codex: shared app-server connection dropped");
            }
            Err(err) => tracing::debug!(%err, "codex: shared app-server connect failed"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

type WsStream = tokio_tungstenite::WebSocketStream<UnixStream>;

/// Yields the `initialize` result too: a [`ThreadWire`] answers its session's
/// own `initialize` with it, since this connection is already initialized.
async fn handshake(endpoint: &DaemonEndpoint) -> Result<(WsStream, Value)> {
    let stream = UnixStream::connect(&endpoint.socket)
        .await
        .with_context(|| format!("connect {}", endpoint.socket.display()))?;
    // Meaningless over a unix socket, but the WebSocket client requires a URI;
    // the daemon accepts the upgrade on any path.
    let (mut ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
        .await
        .context("codex control socket websocket upgrade")?;

    let id = next_id();
    let init = initialize_req(id);
    rings().note_rpc("out", &init);
    ws.send(Message::Text(init.to_string().into())).await?;
    let resp = tokio::time::timeout(RPC_TIMEOUT, read_response(&mut ws, id))
        .await
        .map_err(|_| anyhow::anyhow!("codex daemon initialize timed out"))??;
    super::app_server::record_codex_version(&resp);
    let initialized = super::app_server::initialized_notification();
    rings().note_rpc("out", &initialized);
    ws.send(Message::Text(initialized.to_string().into())).await?;
    Ok((ws, resp.get("result").cloned().unwrap_or(Value::Null)))
}

fn initialize_req(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {"clientInfo": {"name": "cctui", "version": env!("CARGO_PKG_VERSION")}},
    })
}

/// Returns the whole JSON-RPC envelope, which is what
/// [`super::app_server::record_codex_version`] expects.
async fn read_response(ws: &mut WsStream, id: i64) -> Result<Value> {
    while let Some(frame) = ws.next().await {
        let Message::Text(text) = frame? else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        rings().note_rpc("in", &v);
        if v.get("id").and_then(Value::as_i64) == Some(id) {
            if let Some(err) = v.get("error") {
                rings().note_protocol_error(&format!("initialize: {err}"));
                anyhow::bail!("codex daemon initialize error: {err}");
            }
            return Ok(v);
        }
    }
    anyhow::bail!("codex daemon closed during initialize")
}

/// Drive one live connection until it drops. Read requests live in
/// `pending` here, so a drop fails every one of them before `supervise`
/// reconnects. Thread routes live in `routes`, which outlives the connection.
async fn pump(
    mut ws: WsStream,
    ops: &mut mpsc::Receiver<Op>,
    routes: &mut Routes,
    events: &broadcast::Sender<DaemonEvent>,
    shutdown: &CancellationToken,
) {
    let mut pending: HashMap<i64, oneshot::Sender<Result<Value>>> = HashMap::new();
    let mut alive = write_all(&mut ws, routes.on_connect()).await;
    while alive {
        tokio::select! {
            () = shutdown.cancelled() => break,
            op = ops.recv() => {
                let Some(op) = op else { break };
                match op {
                    Op::Request { method, params, reply } => {
                        let id = next_id();
                        let frame = json!({
                            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
                        });
                        rings().note_rpc("out", &frame);
                        if let Err(err) = ws.send(Message::Text(frame.to_string().into())).await {
                            rings().note_protocol_error(&format!("{method}: write failed: {err}"));
                            let _ = reply.send(Err(anyhow::anyhow!("codex daemon write failed: {err}")));
                            break;
                        }
                        pending.insert(id, reply);
                    }
                    Op::Open { route, frames, config, cwd } => routes.open(route, frames, config, cwd),
                    Op::Frame { route, frame } => {
                        alive = write_all(&mut ws, routes.outbound(route, frame)).await;
                    }
                    Op::Close { route } => routes.close(route),
                }
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let writes = dispatch(&text, &mut pending, routes, events);
                        alive = write_all(&mut ws, writes).await;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if ws.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
    for (id, reply) in pending {
        let message = format!("connection dropped before request {id} was answered");
        rings().note_protocol_error(&message);
        let _ = reply.send(Err(anyhow::anyhow!("codex daemon {message}")));
    }
    routes.on_drop();
}

async fn write_all(ws: &mut WsStream, frames: Vec<Value>) -> bool {
    for frame in frames {
        rings().note_rpc("out", &frame);
        if let Err(err) = ws.send(Message::Text(frame.to_string().into())).await {
            rings().note_protocol_error(&format!("route write failed: {err}"));
            return false;
        }
    }
    true
}

/// Returns frames the routes need written in reaction (rejoin replays).
fn dispatch(
    text: &str,
    pending: &mut HashMap<i64, oneshot::Sender<Result<Value>>>,
    routes: &mut Routes,
    events: &broadcast::Sender<DaemonEvent>,
) -> Vec<Value> {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Vec::new() };
    rings().note_rpc("in", &v);
    let method = v.get("method").and_then(Value::as_str);
    if method.is_none()
        && let Some(id) = v.get("id").and_then(Value::as_i64)
    {
        if let Some(reply) = pending.remove(&id) {
            let outcome = v.get("error").map_or_else(
                || Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                |err| {
                    rings().note_protocol_error(&format!("request {id}: {err}"));
                    Err(anyhow::anyhow!("codex daemon error: {err}"))
                },
            );
            let _ = reply.send(outcome);
            return Vec::new();
        }
        return routes.response(id, &v);
    }
    if let Some(method) = method {
        routes.incoming(method, &v);
        if v.get("id").is_none() {
            let _ = events.send(DaemonEvent::Notification {
                method: method.to_owned(),
                params: v.get("params").cloned().unwrap_or(Value::Null),
            });
        }
    }
    Vec::new()
}

/// The thread a notification or server request belongs to.
fn frame_thread(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/thread/id").and_then(Value::as_str))
}

/// `(child, parent)` when a `thread/started` announces a subagent a codex
/// session spawned (`/agents`, `spawn_agent`), so the child nests under it.
#[must_use]
pub fn subagent_parent(params: &Value) -> Option<(String, String)> {
    let thread = params.get("thread")?;
    let child = thread.get("id")?.as_str()?;
    let parent = thread
        .get("parentThreadId")
        .and_then(Value::as_str)
        .or_else(|| {
            let source = thread.get("source")?;
            source
                .pointer("/subAgent/thread_spawn/parent_thread_id")
                .or_else(|| source.pointer("/subagent/thread_spawn/parent_thread_id"))
                .and_then(Value::as_str)
        })
        .filter(|p| !p.is_empty() && *p != child)?;
    Some((child.to_owned(), parent.to_owned()))
}

struct Route {
    frames: mpsc::UnboundedSender<Value>,
    config: ThreadConfig,
    cwd: String,
    thread_id: Option<String>,
    active_turn: Option<String>,
    last_turn: Option<String>,
    rejoining: bool,
    held: Vec<Value>,
    /// `turn/start`s written before a drop and never answered: codex may or
    /// may not have accepted them, and only the rejoin can tell.
    orphaned_starts: Vec<(Value, Value)>,
}

impl Route {
    fn deliver(&self, frame: Value) {
        let _ = self.frames.send(frame);
    }

    fn note_turn(&mut self, method: &str, params: &Value) {
        let Some(turn) = params.pointer("/turn/id").and_then(Value::as_str) else { return };
        match method {
            "turn/started" => {
                self.active_turn = Some(turn.to_owned());
                self.last_turn = Some(turn.to_owned());
            }
            "turn/completed" => {
                if self.active_turn.as_deref() == Some(turn) {
                    self.active_turn = None;
                }
                self.last_turn = Some(turn.to_owned());
            }
            _ => {}
        }
    }
}

enum RoutePending {
    Session { route: u64, local_id: Value, method: String, frame: Value },
    Rejoin { route: u64 },
}

/// Thread routes and their in-flight requests. Kept by `supervise` across
/// reconnects; the socket-free half of the turn transport.
#[derive(Default)]
struct Routes {
    by_route: HashMap<u64, Route>,
    pending: HashMap<i64, RoutePending>,
    init: Option<Value>,
}

impl Routes {
    fn open(
        &mut self,
        route: u64,
        frames: mpsc::UnboundedSender<Value>,
        config: ThreadConfig,
        cwd: String,
    ) {
        self.by_route.insert(
            route,
            Route {
                frames,
                config,
                cwd,
                thread_id: None,
                active_turn: None,
                last_turn: None,
                rejoining: false,
                held: Vec::new(),
                orphaned_starts: Vec::new(),
            },
        );
    }

    fn close(&mut self, route: u64) {
        self.by_route.remove(&route);
        self.pending.retain(|_, p| match p {
            RoutePending::Session { route: r, .. } | RoutePending::Rejoin { route: r } => {
                *r != route
            }
        });
    }

    /// A session's frame, rewritten for the wire. Request ids are remapped
    /// because every session numbers its own from the same base.
    fn outbound(&mut self, route: u64, frame: Value) -> Vec<Value> {
        let Some(r) = self.by_route.get_mut(&route) else { return Vec::new() };
        match (frame.get("method").and_then(Value::as_str), frame.get("id")) {
            (Some("initialize"), Some(id)) => {
                let result = self.init.clone().unwrap_or_else(|| json!({}));
                r.deliver(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                return Vec::new();
            }
            (Some("initialized"), None) => return Vec::new(),
            _ => {}
        }
        if r.rejoining {
            r.held.push(frame);
            return Vec::new();
        }
        vec![self.rewrite(route, frame)]
    }

    fn rewrite(&mut self, route: u64, mut frame: Value) -> Value {
        let method = frame.get("method").and_then(Value::as_str).map(str::to_owned);
        let (Some(method), Some(local_id)) = (method, frame.get("id").cloned()) else {
            return frame;
        };
        let wire = next_id();
        let original = frame.clone();
        frame["id"] = json!(wire);
        self.pending
            .insert(wire, RoutePending::Session { route, local_id, method, frame: original });
        frame
    }

    fn response(&mut self, wire: i64, v: &Value) -> Vec<Value> {
        match self.pending.remove(&wire) {
            Some(RoutePending::Session { route, local_id, method, .. }) => {
                let Some(r) = self.by_route.get_mut(&route) else { return Vec::new() };
                if let Some(result) = v.get("result") {
                    if matches!(method.as_str(), "thread/start" | "thread/resume" | "thread/fork")
                        && let Some(tid) = result.pointer("/thread/id").and_then(Value::as_str)
                    {
                        r.thread_id = Some(tid.to_owned());
                    }
                    if method == "turn/start"
                        && let Some(turn) = result.pointer("/turn/id").and_then(Value::as_str)
                    {
                        r.last_turn = Some(turn.to_owned());
                    }
                }
                let mut frame = v.clone();
                frame["id"] = local_id;
                r.deliver(frame);
                Vec::new()
            }
            Some(RoutePending::Rejoin { route }) => self.rejoined(route, v),
            None => Vec::new(),
        }
    }

    fn incoming(&mut self, method: &str, v: &Value) {
        let params = v.get("params").unwrap_or(&Value::Null);
        let thread = frame_thread(params).map(str::to_owned);
        let parent =
            (method == "thread/started").then(|| subagent_parent(params).map(|(_, p)| p)).flatten();
        for r in self.by_route.values_mut() {
            let Some(tid) = r.thread_id.as_deref() else { continue };
            if thread.as_deref() == Some(tid) {
                r.note_turn(method, params);
                r.deliver(v.clone());
            } else if parent.as_deref() == Some(tid) {
                r.deliver(v.clone());
            }
        }
    }

    /// Everything but an orphaned `turn/start` fails as on stdio EOF-less
    /// error; a route that already owns a thread waits to rejoin it.
    fn on_drop(&mut self) {
        for (_, pending) in self.pending.drain() {
            let RoutePending::Session { route, local_id, method, frame } = pending else {
                continue;
            };
            let Some(r) = self.by_route.get_mut(&route) else { continue };
            if method == "turn/start" && r.thread_id.is_some() {
                r.orphaned_starts.push((local_id, frame));
                continue;
            }
            r.deliver(json!({
                "jsonrpc": "2.0",
                "id": local_id,
                "error": {"code": -32000, "message": format!("codex daemon connection dropped before {method} was answered")},
            }));
        }
        for r in self.by_route.values_mut() {
            r.rejoining = r.thread_id.is_some();
        }
    }

    fn on_connect(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        for (route, r) in &mut self.by_route {
            let Some(tid) = r.thread_id.as_deref() else { continue };
            r.rejoining = true;
            let wire = next_id();
            self.pending.insert(wire, RoutePending::Rejoin { route: *route });
            frames.push(json!({
                "jsonrpc": "2.0",
                "id": wire,
                "method": "thread/resume",
                "params": r.config.resume_params(tid, &r.cwd),
            }));
        }
        frames
    }

    /// Reconcile a rejoined thread against what the route last saw: answer or
    /// replay an orphaned `turn/start`, and replay the turn lifecycle the gap
    /// swallowed so the session never waits on a turn that already ended.
    fn rejoined(&mut self, route: u64, v: &Value) -> Vec<Value> {
        let Some(result) = v.get("result") else {
            let detail = v.get("error").cloned().unwrap_or(Value::Null);
            rings().note_protocol_error(&format!("rejoin failed: {detail}"));
            self.close(route);
            return Vec::new();
        };
        let Some(r) = self.by_route.get_mut(&route) else { return Vec::new() };
        let tid = r.thread_id.clone().unwrap_or_default();
        let turns =
            result.pointer("/thread/turns").and_then(Value::as_array).cloned().unwrap_or_default();
        let latest = turns.last();
        let latest_id = latest.and_then(|t| t.get("id")).and_then(Value::as_str);
        let latest_running =
            latest.and_then(|t| t.get("status")).and_then(Value::as_str) == Some("inProgress");

        let mut replay = Vec::new();
        for (local_id, frame) in std::mem::take(&mut r.orphaned_starts) {
            let accepted = latest_id.is_some() && latest_id != r.last_turn.as_deref();
            if accepted && let Some(turn) = latest {
                r.deliver(json!({"jsonrpc": "2.0", "id": local_id, "result": {"turn": turn}}));
                r.deliver(json!({
                    "jsonrpc": "2.0", "method": "turn/started",
                    "params": {"threadId": tid, "turn": turn},
                }));
                r.last_turn = latest_id.map(str::to_owned);
                r.active_turn = latest_id.map(str::to_owned);
            } else {
                replay.push(frame);
            }
        }
        if latest_running && r.active_turn.as_deref() != latest_id {
            if let Some(turn) = latest {
                r.deliver(json!({
                    "jsonrpc": "2.0", "method": "turn/started",
                    "params": {"threadId": tid, "turn": turn},
                }));
            }
            r.active_turn = latest_id.map(str::to_owned);
            r.last_turn = latest_id.map(str::to_owned);
        }
        if let Some(active) = r.active_turn.clone()
            && !(latest_running && latest_id == Some(active.as_str()))
        {
            let turn = turns
                .iter()
                .find(|t| t.get("id").and_then(Value::as_str) == Some(active.as_str()))
                .cloned()
                .unwrap_or_else(|| json!({"id": active, "status": "interrupted", "items": []}));
            r.deliver(json!({
                "jsonrpc": "2.0", "method": "turn/completed",
                "params": {"threadId": tid, "turn": turn},
            }));
            r.active_turn = None;
        }
        r.rejoining = false;
        replay.extend(std::mem::take(&mut r.held));
        replay.into_iter().map(|frame| self.rewrite(route, frame)).collect()
    }
}

/// Lazily-built shared connection. A discovery failure is cached as "no daemon
/// here" so callers fall back to stdio without re-probing on every RPC.
#[derive(Clone)]
pub struct SharedDaemon {
    inner: Arc<tokio::sync::OnceCell<Option<DaemonHandle>>>,
    bin: String,
    shutdown: CancellationToken,
}

impl SharedDaemon {
    #[must_use]
    pub fn new(bin: String, shutdown: CancellationToken) -> Self {
        Self { inner: Arc::new(tokio::sync::OnceCell::new()), bin, shutdown }
    }

    /// Bypass discovery for a known endpoint.
    #[must_use]
    pub fn from_endpoint(endpoint: DaemonEndpoint, shutdown: CancellationToken) -> Self {
        let cell = tokio::sync::OnceCell::new();
        let _ = cell.set(Some(connect(endpoint, shutdown.clone())));
        Self { inner: Arc::new(cell), bin: String::new(), shutdown }
    }

    pub async fn handle(&self) -> Option<DaemonHandle> {
        self.inner
            .get_or_init(|| async {
                match DaemonEndpoint::discover(&self.bin).await {
                    Ok(endpoint) => Some(connect(endpoint, self.shutdown.clone())),
                    Err(err) => {
                        tracing::info!(
                            %err,
                            "codex: no shared app-server daemon; falling back to per-op stdio"
                        );
                        None
                    }
                }
            })
            .await
            .clone()
    }
}

/// Turns ride the shared connection only when this machine opts in; stdio
/// stays the default because one app-server crash would otherwise take every
/// live codex session on the machine with it.
pub const SHARED_TURNS_ENV: &str = "CCTUI_CODEX_SHARED_TURNS";

#[must_use]
pub fn shared_turns_enabled(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1" | "true" | "yes" | "on"))
}

static TURN_TRANSPORT: std::sync::OnceLock<SharedDaemon> = std::sync::OnceLock::new();

/// Called once by the adapter; a no-op unless [`SHARED_TURNS_ENV`] is set.
pub fn register_turn_transport(shared: &SharedDaemon) {
    if shared_turns_enabled(std::env::var(SHARED_TURNS_ENV).ok().as_deref()) {
        let _ = TURN_TRANSPORT.set(shared.clone());
        tracing::info!("codex: turns ride the shared app-server when it is up");
    }
}

/// `None` means use stdio: opted out, no daemon, or the connection is down
/// right now. A route is only opened on a live connection so a session never
/// starts by waiting out a reconnect backoff.
pub async fn turn_transport() -> Option<DaemonHandle> {
    let handle = TURN_TRANSPORT.get()?.handle().await?;
    handle.is_connected().then_some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_start_report_yields_the_socket_path() {
        let out = r#"{"status":"started","backend":"pid","pid":1,"socketPath":"/tmp/a.sock"}"#;
        assert_eq!(parse_daemon_start(out).unwrap().socket, PathBuf::from("/tmp/a.sock"));
    }

    #[test]
    fn daemon_start_report_tolerates_leading_log_lines() {
        let out = "warning: experimental\n{\"socketPath\":\"/x/y.sock\"}\n";
        assert_eq!(parse_daemon_start(out).unwrap().socket, PathBuf::from("/x/y.sock"));
    }

    #[test]
    fn daemon_start_without_socket_path_is_an_error() {
        assert!(parse_daemon_start("{\"status\":\"started\"}").is_err());
        assert!(parse_daemon_start("not json").is_err());
    }

    #[test]
    fn rpc_ids_are_unique() {
        assert_ne!(next_id(), next_id());
    }

    #[tokio::test]
    async fn dispatch_completes_a_pending_request() {
        let (tx, rx) = oneshot::channel();
        let mut pending = HashMap::from([(7, tx)]);
        let (events, _guard) = broadcast::channel(8);
        dispatch(r#"{"id":7,"result":{"ok":true}}"#, &mut pending, &mut Routes::default(), &events);
        assert!(pending.is_empty());
        assert_eq!(rx.await.unwrap().unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn dispatch_surfaces_a_jsonrpc_error() {
        let (tx, rx) = oneshot::channel();
        let mut pending = HashMap::from([(7, tx)]);
        let (events, _guard) = broadcast::channel(8);
        dispatch(
            r#"{"id":7,"error":{"code":-32601,"message":"nope"}}"#,
            &mut pending,
            &mut Routes::default(),
            &events,
        );
        let err = rx.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn dispatch_fans_out_notifications() {
        let mut pending = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);
        dispatch(
            r#"{"method":"thread/started","params":{"threadId":"t1"}}"#,
            &mut pending,
            &mut Routes::default(),
            &events,
        );
        match rx.try_recv().unwrap() {
            DaemonEvent::Notification { method, params } => {
                assert_eq!(method, "thread/started");
                assert_eq!(params["threadId"], "t1");
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    /// The end-to-end proof that the shared socket is no longer a blind spot:
    /// a real request over a real WS connection must land in the ring tagged
    /// `shared`, in both directions.
    #[tokio::test]
    async fn frames_on_the_shared_connection_are_ringed_and_tagged_shared() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("ctl.sock");
        let _server = testserver::spawn(&sock, |_method, _params| json!({"threads": []}));

        let shutdown = CancellationToken::new();
        let handle = connect(DaemonEndpoint { socket: sock }, shutdown.clone());
        handle.request("thread/list/ringprobe", json!({})).await.expect("request");
        shutdown.cancel();

        let tail = super::super::app_server::shared_rings().rpc_tail();
        let sent = tail
            .iter()
            .find(|f| f.label == "thread/list/ringprobe")
            .expect("the outbound frame is in the ring");
        assert_eq!(sent.transport, "shared");
        assert_eq!(sent.direction, "out");
        assert!(
            tail.iter().any(|f| f.direction == "in" && f.transport == "shared"),
            "the receive loop feeds the ring too: {tail:?}"
        );
    }

    #[test]
    fn shared_turns_are_opt_in() {
        assert!(!shared_turns_enabled(None));
        assert!(!shared_turns_enabled(Some("")));
        assert!(!shared_turns_enabled(Some("0")));
        assert!(!shared_turns_enabled(Some("false")));
        assert!(shared_turns_enabled(Some("1")));
        assert!(shared_turns_enabled(Some(" true ")));
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    fn tiered() -> ThreadConfig {
        ThreadConfig::new(&std::collections::BTreeMap::new(), Some("fast"))
    }

    /// A route that already owns thread `tid`, with its start answered.
    fn bound(routes: &mut Routes, route: u64, tid: &str) -> mpsc::UnboundedReceiver<Value> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        routes.open(route, tx, tiered(), "/repo".to_owned());
        let out = routes.outbound(route, json!({"id": 2, "method": "thread/start", "params": {}}));
        let wire = out[0]["id"].as_i64().unwrap();
        routes.response(wire, &json!({"id": wire, "result": {"thread": {"id": tid}}}));
        drain(&mut rx);
        rx
    }

    fn send_turn(routes: &mut Routes, route: u64, local: i64) -> i64 {
        let out = routes.outbound(
            route,
            json!({"id": local, "method": "turn/start", "params": {"threadId": "t1"}}),
        );
        out[0]["id"].as_i64().unwrap()
    }

    fn rejoin(routes: &mut Routes, turns: &Value) -> Vec<Value> {
        let frames = routes.on_connect();
        let wire = frames[0]["id"].as_i64().unwrap();
        routes.response(
            wire,
            &json!({"id": wire, "result": {"thread": {"id": "t1", "turns": turns}}}),
        )
    }

    #[test]
    fn session_ids_are_remapped_on_the_wire_and_restored_on_the_answer() {
        let mut routes = Routes::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        routes.open(1, tx, tiered(), "/repo".to_owned());
        let out = routes.outbound(1, json!({"id": 2, "method": "thread/start", "params": {}}));
        let wire = out[0]["id"].as_i64().unwrap();
        assert_ne!(wire, 2);
        routes.response(wire, &json!({"id": wire, "result": {"thread": {"id": "t1"}}}));
        assert_eq!(drain(&mut rx)[0]["id"], 2);
        assert_eq!(routes.by_route[&1].thread_id.as_deref(), Some("t1"));
    }

    #[test]
    fn the_session_handshake_is_answered_from_the_shared_connection() {
        let mut routes =
            Routes { init: Some(json!({"userAgent": "codex/9"})), ..Routes::default() };
        let (tx, mut rx) = mpsc::unbounded_channel();
        routes.open(1, tx, tiered(), "/repo".to_owned());
        assert!(
            routes.outbound(1, json!({"id": 1, "method": "initialize", "params": {}})).is_empty()
        );
        assert!(routes.outbound(1, json!({"method": "initialized"})).is_empty());
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["id"], 1);
        assert_eq!(got[0]["result"]["userAgent"], "codex/9");
    }

    #[test]
    fn a_reply_to_a_server_request_passes_through_untouched() {
        let mut routes = Routes::default();
        let _rx = bound(&mut routes, 1, "t1");
        let reply = json!({"id": 555, "result": {"decision": "accept"}});
        assert_eq!(routes.outbound(1, reply.clone()), vec![reply]);
    }

    #[test]
    fn notifications_reach_only_the_route_owning_their_thread() {
        let mut routes = Routes::default();
        let mut a = bound(&mut routes, 1, "t1");
        let mut b = bound(&mut routes, 2, "t2");
        routes.incoming(
            "item/started",
            &json!({"method": "item/started", "params": {"threadId": "t1"}}),
        );
        assert_eq!(drain(&mut a).len(), 1);
        assert!(drain(&mut b).is_empty());
    }

    #[test]
    fn a_drop_fails_plain_requests_but_keeps_a_turn_start_for_the_rejoin() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        routes.outbound(1, json!({"id": 5, "method": "thread/name/set", "params": {}}));
        send_turn(&mut routes, 1, 6);
        routes.on_drop();
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0]["id"], 5);
        assert!(got[0]["error"]["message"].as_str().unwrap().contains("thread/name/set"));
        assert_eq!(routes.by_route[&1].orphaned_starts.len(), 1);
        assert!(routes.by_route[&1].rejoining);
    }

    #[test]
    fn a_reconnect_rejoins_every_bound_thread_resupplying_its_config() {
        let mut routes = Routes::default();
        let _a = bound(&mut routes, 1, "t1");
        let (tx, _unbound) = mpsc::unbounded_channel();
        routes.open(2, tx, tiered(), "/repo".to_owned());
        routes.on_drop();
        let frames = routes.on_connect();
        assert_eq!(frames.len(), 1, "an unbound route has nothing to rejoin");
        assert_eq!(frames[0]["method"], "thread/resume");
        assert_eq!(frames[0]["params"]["threadId"], "t1");
        assert_eq!(frames[0]["params"]["serviceTier"], "fast");
        assert_eq!(frames[0]["params"]["config"]["service_tier"], "fast");
    }

    #[test]
    fn an_orphaned_turn_start_codex_accepted_is_answered_not_replayed() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        send_turn(&mut routes, 1, 6);
        routes.on_drop();
        let writes =
            rejoin(&mut routes, &json!([{"id": "turn-new", "status": "inProgress", "items": []}]));
        assert!(writes.is_empty(), "no duplicate turn/start: {writes:?}");
        let got = drain(&mut rx);
        assert_eq!(got[0]["id"], 6);
        assert_eq!(got[0]["result"]["turn"]["id"], "turn-new");
        assert_eq!(got[1]["method"], "turn/started");
        assert_eq!(got.len(), 2, "a still-running turn is not completed: {got:?}");
        assert_eq!(routes.by_route[&1].active_turn.as_deref(), Some("turn-new"));
    }

    #[test]
    fn an_orphaned_turn_start_codex_never_saw_is_replayed_once() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        routes.incoming(
            "turn/completed",
            &json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": "turn-old"}}}),
        );
        drain(&mut rx);
        send_turn(&mut routes, 1, 6);
        routes.on_drop();
        let writes =
            rejoin(&mut routes, &json!([{"id": "turn-old", "status": "completed", "items": []}]));
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0]["method"], "turn/start");
        assert!(drain(&mut rx).is_empty());
        let wire = writes[0]["id"].as_i64().unwrap();
        routes.response(wire, &json!({"id": wire, "result": {"turn": {"id": "turn-2"}}}));
        assert_eq!(drain(&mut rx)[0]["id"], 6, "the replay answers the original request");
    }

    #[test]
    fn a_turn_that_finished_during_the_gap_is_completed_for_the_session() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        routes.incoming(
            "turn/started",
            &json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "turn-a"}}}),
        );
        drain(&mut rx);
        routes.on_drop();
        rejoin(&mut routes, &json!([{"id": "turn-a", "status": "completed", "items": []}]));
        let got = drain(&mut rx);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["method"], "turn/completed");
        assert_eq!(got[0]["params"]["turn"]["status"], "completed");
        assert_eq!(routes.by_route[&1].active_turn, None);
    }

    #[test]
    fn a_turn_still_running_after_the_rejoin_keeps_streaming_untouched() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        routes.incoming(
            "turn/started",
            &json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "turn-a"}}}),
        );
        drain(&mut rx);
        routes.on_drop();
        rejoin(&mut routes, &json!([{"id": "turn-a", "status": "inProgress", "items": []}]));
        assert!(drain(&mut rx).is_empty());
        assert_eq!(routes.by_route[&1].active_turn.as_deref(), Some("turn-a"));
        routes.incoming(
            "item/completed",
            &json!({"method": "item/completed", "params": {"threadId": "t1"}}),
        );
        assert_eq!(drain(&mut rx).len(), 1, "the rejoined route still receives the stream");
    }

    /// A daemon restart loses the running turn with the process; a session
    /// must see it end rather than spin forever.
    #[test]
    fn a_turn_the_restarted_daemon_no_longer_knows_ends_as_interrupted() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        routes.incoming(
            "turn/started",
            &json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": "turn-a"}}}),
        );
        drain(&mut rx);
        routes.on_drop();
        rejoin(&mut routes, &json!([]));
        let got = drain(&mut rx);
        assert_eq!(got[0]["method"], "turn/completed");
        assert_eq!(got[0]["params"]["turn"]["id"], "turn-a");
        assert_eq!(got[0]["params"]["turn"]["status"], "interrupted");
    }

    #[test]
    fn frames_sent_while_rejoining_are_held_then_flushed_in_order() {
        let mut routes = Routes::default();
        let _rx = bound(&mut routes, 1, "t1");
        routes.on_drop();
        let frames = routes.on_connect();
        assert!(
            routes
                .outbound(1, json!({"id": 9, "method": "turn/interrupt", "params": {}}))
                .is_empty()
        );
        assert!(
            routes.outbound(1, json!({"id": 10, "method": "turn/start", "params": {}})).is_empty()
        );
        let wire = frames[0]["id"].as_i64().unwrap();
        let writes =
            routes.response(wire, &json!({"id": wire, "result": {"thread": {"turns": []}}}));
        let methods: Vec<_> = writes.iter().map(|w| w["method"].clone()).collect();
        assert_eq!(methods, vec![json!("turn/interrupt"), json!("turn/start")]);
        assert!(!routes.by_route[&1].rejoining);
    }

    #[test]
    fn a_failed_rejoin_closes_the_route_like_a_child_eof() {
        let mut routes = Routes::default();
        let mut rx = bound(&mut routes, 1, "t1");
        routes.on_drop();
        let frames = routes.on_connect();
        let wire = frames[0]["id"].as_i64().unwrap();
        routes.response(wire, &json!({"id": wire, "error": {"message": "no such thread"}}));
        assert!(routes.by_route.is_empty());
        assert!(matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)));
    }

    #[test]
    fn a_subagent_thread_names_its_parent() {
        let params = json!({"thread": {"id": "child", "parentThreadId": "parent"}});
        assert_eq!(subagent_parent(&params), Some(("child".to_owned(), "parent".to_owned())));
        let spawned = json!({"thread": {"id": "child", "source": {"subAgent": {"thread_spawn": {"parent_thread_id": "p2", "depth": 1}}}}});
        assert_eq!(subagent_parent(&spawned).unwrap().1, "p2");
        assert_eq!(subagent_parent(&json!({"thread": {"id": "solo"}})), None);
        assert_eq!(subagent_parent(&json!({"thread": {"id": "x", "parentThreadId": "x"}})), None);
    }

    #[test]
    fn a_subagent_start_is_delivered_to_its_parent_route() {
        let mut routes = Routes::default();
        let mut parent = bound(&mut routes, 1, "t1");
        let mut other = bound(&mut routes, 2, "t2");
        routes.incoming(
            "thread/started",
            &json!({"method": "thread/started", "params": {"thread": {"id": "kid", "parentThreadId": "t1"}}}),
        );
        assert_eq!(drain(&mut parent).len(), 1);
        assert!(drain(&mut other).is_empty());
    }

    #[test]
    fn closing_a_route_forgets_its_pending_requests() {
        let mut routes = Routes::default();
        let _rx = bound(&mut routes, 1, "t1");
        send_turn(&mut routes, 1, 6);
        routes.close(1);
        assert!(routes.pending.is_empty());
    }

    #[tokio::test]
    async fn a_thread_wire_round_trips_over_a_real_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("ctl.sock");
        let _server = testserver::spawn(&sock, |_method, _params| json!({"thread": {"id": "t9"}}));
        let shutdown = CancellationToken::new();
        let handle = connect(DaemonEndpoint { socket: sock }, shutdown.clone());
        let mut wire = handle.open_thread(tiered(), "/repo".to_owned()).await.expect("open");
        wire.sink
            .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
            .await
            .unwrap();
        let init = wire.frames.recv().await.unwrap();
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["userAgent"], "codex/0.153.4");
        wire.sink
            .send(&json!({"jsonrpc": "2.0", "id": 2, "method": "thread/start", "params": {}}))
            .await
            .unwrap();
        let started = wire.frames.recv().await.unwrap();
        assert_eq!(started["id"], 2);
        assert_eq!(started["result"]["thread"]["id"], "t9");
        shutdown.cancel();
    }

    /// A response nobody awaits has no `method` and must not be mistaken for a
    /// notification.
    #[test]
    fn dispatch_ignores_an_unmatched_response() {
        let mut pending = HashMap::new();
        let (events, mut rx) = broadcast::channel(8);
        dispatch(r#"{"id":99,"result":{}}"#, &mut pending, &mut Routes::default(), &events);
        assert!(rx.try_recv().is_err());
    }
}

/// A minimal app-server stand-in: answers `initialize` and whatever canned
/// responses the case needs, so the transport can be exercised without a
/// real codex.
#[cfg(test)]
pub(super) mod testserver {
    use super::*;
    use tokio::net::UnixListener;

    /// Serves one connection, replying to every request with
    /// `responses(method) -> result`.
    pub fn spawn<F>(path: &std::path::Path, responses: F) -> tokio::task::JoinHandle<()>
    where
        F: Fn(&str, &Value) -> Value + Send + 'static,
    {
        let listener = UnixListener::bind(path).expect("bind test socket");
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else { return };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else { return };
            while let Some(Ok(frame)) = ws.next().await {
                let Message::Text(text) = frame else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                let Some(id) = v.get("id").and_then(Value::as_i64) else { continue };
                let method = v.get("method").and_then(Value::as_str).unwrap_or("");
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let result = if method == "initialize" {
                    json!({"userAgent": "codex/0.153.4"})
                } else {
                    responses(method, &params)
                };
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                    return;
                }
            }
        })
    }
}
