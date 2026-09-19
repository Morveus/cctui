//! The single routing seam for all WS-bound traffic (phase 1 of the
//! message-bus architecture).
//!
//! The bus owns ALL WS delivery state in one place — pod-local daemon commands,
//! per-session stream broadcast, and server event fan-out:
//!
//!   * point-to-point commands toward the pod terminating a WS
//!     ([`Bus::command_daemon`], [`Bus::command_dispatcher`]) and correlated
//!     round-trips ([`Bus::request_daemon`], [`Bus::request_dispatcher`]) with
//!     the pending oneshot maps as private internals;
//!   * cluster-wide pub/sub ([`Bus::publish`], [`Bus::subscribe_session`],
//!     [`Bus::subscribe_server`]).
//!
//! Behind it sits a [`Transport`]. [`NoopTransport`] (local dev / single
//! replica) keeps single-pod semantics: routing is a local registry lookup and
//! publish is a local broadcast. With `CCTUI_POD_IP` set, main swaps in
//! [`peer::PeerHttpTransport`]: local misses are forwarded to the
//! peer pod owning the WS, and publishes fan out to every live replica —
//! replacing the retired HTTP request-replay forwarder. (NATS)
//! plugs in here the same way without touching callers.
//!
//! Persistence is NOT the bus's job: event DB writes and the permission/ask/
//! plan stores stay with their current owners — the bus moves delivery only.

pub mod peer;

use std::sync::Arc;

use cctui_proto::adapter::{AdapterCommand, BootstrapFile};
use cctui_proto::git::GitInfo;
use cctui_proto::ws::{
    AgentEvent, DaemonFrameDown, DispatcherFrameDown, DispatcherFrameUp, ReadFileErrorKind,
    ReadFileOk, ServerEvent,
};
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::state::AppState;

/// Outcome of a mid-chat file-stage request: the staged absolute
/// paths on success, or an error string on failure.
type StageFilesOutcome = Result<Vec<String>, String>;

/// Outcome of a working-dir autocomplete listing (spawn dialog): the
/// directory names on success, or an error string on failure.
type ListDirsOutcome = Result<Vec<String>, String>;

/// Outcome of a git-info lookup (spawn dialog branch badge).
type GitInfoOutcome = Result<GitInfo, String>;

/// Outcome of a file read: the payload on success, or the refusal kind + a
/// message on failure.
type ReadFileOutcome = Result<ReadFileOk, (ReadFileErrorKind, String)>;

/// How long [`Bus::request_daemon`] waits for the daemon's `StageFilesResult`
/// before giving up. Staging is local filesystem work after a small base64
/// decode, so this is generous-but-bounded.
const STAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long [`Bus::request_daemon`] waits for the daemon's `ListDirsResult`.
/// Autocomplete is interactive — better to fail fast than to hold the
/// typeahead open.
const LIST_DIRS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// How long [`Bus::request_daemon`] waits for the daemon's `ReadFileResult`.
/// A large file is PUT to the blob store before the reply, so this is sized
/// for a 32 MiB upload over a slow link.
const READ_FILE_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(1);

/// How long [`Bus::request_dispatcher`] awaits a dispatcher reply. Spawning a
/// container/pod can take a few seconds; status/cancel are quick. One generous
/// bound covers all three round-trips.
const DISPATCHER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long [`Bus::request_daemon`] waits for the adapter's diagnose report.
/// Aggregation over in-memory state plus one bounded socket probe —
/// fast, but the reply rides the adapter's command loop, so leave headroom.
const DIAGNOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Capacity of the per-session stream channels (mirrors the pre-bus
/// `registry.rs` broadcast) and of the server event channel.
const CHANNEL_CAPACITY: usize = 256;

/// Delivery failures surfaced by the bus. Superset of the retired
/// `daemon_dispatch::Error` so existing error-handling match arms keep
/// working; display strings are preserved verbatim where they reach clients.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("session not found")]
    NotFound,
    #[error("session has no adapter_id (legacy path)")]
    NoAdapter,
    #[error("session has no machine_uuid yet")]
    NoMachine,
    #[error("no daemon connected for machine {0}")]
    NoDaemon(Uuid),
    #[error("no dispatcher connected for {0}")]
    NoDispatcher(Uuid),
    #[error("daemon channel closed")]
    Closed,
    #[error("peer disconnected before replying")]
    Disconnected,
    #[error("timed out waiting for the daemon to stage files")]
    Timeout,
    #[error("daemon could not stage files: {0}")]
    Staging(String),
    #[error("daemon could not list the directory: {0}")]
    ListDirs(String),
    #[error("daemon could not read git info: {0}")]
    GitInfo(String),
    #[error("daemon could not read the file: {1}")]
    ReadFile(ReadFileErrorKind, String),
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("reconcile build error: {0}")]
    Reconcile(String),
    /// A cross-replica transport failure that doesn't map onto one of the
    /// meaning-bearing variants above: the peer replied with an
    /// unclassified error or an unreadable body. The frame was NOT delivered.
    #[error("bus transport error: {0}")]
    Transport(String),
}

/// A correlated daemon round-trip (request/response over the daemon WS). The
/// bus mints the `request_id`, parks the reply oneshot internally, and fires
/// it when the WS read loop reports the matching result frame.
#[derive(Debug)]
pub enum DaemonRequest {
    /// Stage mid-chat attachment files → `StageFilesResult`.
    StageFiles { adapter_id: String, local_id: String, uploads: Vec<BootstrapFile> },
    /// Working-directory autocomplete listing → `ListDirsResult`.
    ListDirs { path: String },
    /// Git facts for a directory → `GitInfoResult`.
    GitInfo { path: String, include_dirty: bool },
    /// One file's bytes (inline or via the blob store) → `ReadFileResult`.
    ReadFile { path: String, max_bytes: u64, cwd: Option<String> },
    /// Session diagnose snapshot. Unlike the two above this rides
    /// the generic `Command` frame into the adapter (the facts live in the
    /// adapter's driver, not the supervisor) and the reply comes back as an
    /// `AdapterEvent::Diagnose` on the event stream, correlated by the bus's
    /// request id.
    Diagnose { adapter_id: String, local_id: String },
}

/// Reply to a [`DaemonRequest`], variant-matched to the request.
#[derive(Debug)]
pub enum DaemonResponse {
    StagedFiles(Vec<String>),
    Dirs(Vec<String>),
    GitInfo(GitInfo),
    File(ReadFileOk),
    Diagnose(Box<cctui_proto::diagnose::SessionDiagnose>),
}

/// Everything the bus publishes cluster-wide: an envelope over the per-session
/// agent stream and the server event fan-out.
#[derive(Debug, Clone)]
pub enum BusEvent {
    /// Per-session agent stream, delivered to [`Bus::subscribe_session`]
    /// subscribers of `session_id`. No in-process producer publishes these yet
    /// (the daemon ingest path streams via `ServerEvent::Stream`, exactly as
    /// before — the pre-bus `stream_tx` had no producer either); the variant
    /// is the seam route through.
    #[allow(dead_code)]
    Session { session_id: String, event: AgentEvent },
    /// Cluster-wide server event (permission prompts, asks, plans, acks,
    /// liveness, session registered/deregistered, …), delivered to every
    /// [`Bus::subscribe_server`] subscriber.
    Server(ServerEvent),
}

/// The routing backend behind the [`Bus`]. The bus always tries local
/// delivery first (this pod's connection registries / broadcast channels);
/// the transport is its escape hatch to the rest of the cluster.
///
/// Contract for future transports (peer-HTTP, NATS):
///   * `forward_daemon` / `forward_dispatcher` / `request_*` are invoked ONLY
///     when the local lookup missed. A cross-replica transport forwards the
///     frame/request to the peer pod owning the WS and returns its outcome;
///     when no peer owns it either, return the same `NoDaemon`/`NoDispatcher`
///     miss the caller would have seen locally.
///   * `relay` is invoked on EVERY publish, alongside local broadcast. A
///     cross-replica transport re-publishes the event to peer pods (which
///     deliver it to their local subscribers only — no re-relay, or events
///     would loop).
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Route a fire-and-forget daemon command that found no local WS.
    async fn forward_daemon(&self, machine: Uuid, frame: DaemonFrameDown) -> Result<(), BusError>;

    /// Route a fire-and-forget dispatcher command that found no local WS.
    async fn forward_dispatcher(
        &self,
        dispatcher: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<(), BusError>;

    /// Route a correlated daemon round-trip that found no local WS.
    async fn request_daemon(
        &self,
        machine: Uuid,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError>;

    /// Route a correlated dispatcher round-trip that found no local WS.
    async fn request_dispatcher(
        &self,
        dispatcher: Uuid,
        request_id: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<DispatcherFrameUp, BusError>;

    /// Relay a locally-published event to peer pods (local delivery already
    /// happened). Fire-and-forget: publish is infallible for callers.
    fn relay(&self, event: &BusEvent);
}

/// Local-only transport: single-pod semantics. A local lookup miss is a miss —
/// exactly the pre-bus `NoDaemon`/"dispatcher offline" behavior — and publish
/// reaches only this pod's subscribers. Used when `CCTUI_POD_IP` is unset
/// (local dev / single replica); multi-replica deployments swap in
/// [`peer::PeerHttpTransport`].
pub struct NoopTransport;

#[async_trait::async_trait]
impl Transport for NoopTransport {
    async fn forward_daemon(&self, machine: Uuid, _frame: DaemonFrameDown) -> Result<(), BusError> {
        Err(BusError::NoDaemon(machine))
    }

    async fn forward_dispatcher(
        &self,
        dispatcher: Uuid,
        _frame: DispatcherFrameDown,
    ) -> Result<(), BusError> {
        Err(BusError::NoDispatcher(dispatcher))
    }

    async fn request_daemon(
        &self,
        machine: Uuid,
        _request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        Err(BusError::NoDaemon(machine))
    }

    async fn request_dispatcher(
        &self,
        dispatcher: Uuid,
        _request_id: Uuid,
        _frame: DispatcherFrameDown,
    ) -> Result<DispatcherFrameUp, BusError> {
        Err(BusError::NoDispatcher(dispatcher))
    }

    fn relay(&self, _event: &BusEvent) {}
}

struct Inner {
    /// Per-machine outbound channel into the connected daemon's WS task.
    /// Absent entry = daemon not terminated by this pod. A machine id names a
    /// GROUP, not a connection — every dispatched worker pod authenticates as
    /// the user's shared `dispatch` machine — so this is only sound for
    /// machine-scoped frames (Reconcile, discovery, update). Session-scoped
    /// frames must go through [`Inner::session_conn`].
    daemons: DashMap<Uuid, mpsc::Sender<DaemonFrameDown>>,
    /// Every live daemon WS this pod terminates, keyed by its own connection
    /// id, with the machine it authenticated as.
    conns: DashMap<Uuid, (Uuid, mpsc::Sender<DaemonFrameDown>)>,
    /// Connection ids per machine, so a shared identity can be recognised as
    /// ambiguous instead of silently resolving to whichever pod connected last.
    machine_conns: DashMap<Uuid, std::collections::HashSet<Uuid>>,
    /// Which connection announced each session — the routing address for every
    /// session-scoped frame.
    session_conn: DashMap<String, Uuid>,
    /// Per-dispatcher outbound channel into the connected enrolled
    /// dispatcher's WS task. Peer of `daemons`.
    dispatchers: DashMap<Uuid, mpsc::Sender<DispatcherFrameDown>>,
    /// In-flight stage-files round-trips awaiting a daemon `StageFilesResult`,
    /// keyed by the request id the bus minted.
    pending_stage: DashMap<Uuid, oneshot::Sender<StageFilesOutcome>>,
    /// In-flight autocomplete listings awaiting a daemon `ListDirsResult`.
    pending_listdirs: DashMap<Uuid, oneshot::Sender<ListDirsOutcome>>,
    /// In-flight git-info lookups awaiting a daemon `GitInfoResult`.
    pending_gitinfo: DashMap<Uuid, oneshot::Sender<GitInfoOutcome>>,
    /// In-flight file reads awaiting a daemon `ReadFileResult`.
    pending_readfile: DashMap<Uuid, oneshot::Sender<ReadFileOutcome>>,
    /// In-flight session-diagnose round-trips awaiting the adapter's
    /// `AdapterEvent::Diagnose` reply.
    pending_diagnose: DashMap<Uuid, oneshot::Sender<Box<cctui_proto::diagnose::SessionDiagnose>>>,
    /// In-flight Dispatch/Status/Cancel round-trips awaiting a
    /// [`DispatcherFrameUp`] reply.
    pending_dispatcher: DashMap<Uuid, oneshot::Sender<DispatcherFrameUp>>,
    /// Cluster-wide server event fan-out (the former `state.tui_tx`).
    server_tx: broadcast::Sender<ServerEvent>,
    /// Per-session agent stream channels (the former
    /// `SessionHandle::stream_tx`). Entries live exactly as long as the
    /// session's registry handle: created on register, removed on deregister.
    session_streams: DashMap<String, broadcast::Sender<AgentEvent>>,
    /// Live count of browsers watching each session's read-only terminal.
    /// Gates the daemon PTY stream: on the 0↔1 transition the ws
    /// handler tells the daemon to start/stop the viewer attach, so an unwatched
    /// session carries no extra stream. Process-local — a multi-pod deploy would
    /// need this on the transport, but the fan-out already assumes single-pod.
    pty_watchers: DashMap<String, usize>,
    transport: Box<dyn Transport>,
}

/// Cheap-to-clone handle to the delivery seam. One per process, on `AppState`.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

impl Bus {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self {
            inner: Arc::new(Inner {
                daemons: DashMap::new(),
                conns: DashMap::new(),
                machine_conns: DashMap::new(),
                session_conn: DashMap::new(),
                dispatchers: DashMap::new(),
                pending_stage: DashMap::new(),
                pending_listdirs: DashMap::new(),
                pending_gitinfo: DashMap::new(),
                pending_readfile: DashMap::new(),
                pending_diagnose: DashMap::new(),
                pending_dispatcher: DashMap::new(),
                server_tx: broadcast::channel(CHANNEL_CAPACITY).0,
                session_streams: DashMap::new(),
                pty_watchers: DashMap::new(),
                transport,
            }),
        }
    }

    // ---- connection registry (daemon / dispatcher WS handlers) ----

    /// Register the outbound channel of a freshly connected daemon WS under its
    /// own `conn_id`. The machine entry is overwritten (newest connection
    /// wins); the per-connection entry is additive, so sibling connections
    /// sharing the machine identity stay addressable.
    pub fn register_daemon(&self, machine: Uuid, conn_id: Uuid, tx: mpsc::Sender<DaemonFrameDown>) {
        self.inner.conns.insert(conn_id, (machine, tx.clone()));
        self.inner.machine_conns.entry(machine).or_default().insert(conn_id);
        self.inner.daemons.insert(machine, tx);
    }

    /// Drop a closing connection: its own entry and its session bindings
    /// unconditionally (nothing else can own them), but `machine`'s entry only
    /// if it is STILL `tx` — during a reconnect race the daemon's new
    /// connection may already have overwritten the map with its own channel,
    /// and an unconditional remove would delete that live channel. Returns
    /// whether the machine entry was removed, so the caller can mirror it into
    /// presence.
    pub fn unregister_daemon(
        &self,
        machine: Uuid,
        conn_id: Uuid,
        tx: &mpsc::Sender<DaemonFrameDown>,
    ) -> bool {
        self.inner.conns.remove(&conn_id);
        self.inner.machine_conns.remove_if_mut(&machine, |_, conns| {
            conns.remove(&conn_id);
            conns.is_empty()
        });
        self.inner.session_conn.retain(|_, owner| *owner != conn_id);
        self.inner.daemons.remove_if(&machine, |_, current| current.same_channel(tx)).is_some()
    }

    /// Bind `session_id` to the connection that announced it. Called from the
    /// daemon WS read loop on every `SessionRegistered`/`SessionStarted`, so a
    /// session that migrates between pods follows its newest announcement.
    pub fn bind_session_conn(&self, session_id: &str, conn_id: Uuid) {
        self.inner.session_conn.insert(session_id.to_owned(), conn_id);
    }

    /// The channel a SESSION-scoped frame must take. The connection that
    /// announced the session when known; otherwise the machine entry, but only
    /// while that machine has at most one live connection here. With several
    /// connections and no binding, any choice is a coin flip that silently
    /// delivers one session's command to another's worker, so refuse instead.
    fn session_channel(
        &self,
        machine: Uuid,
        session_id: &str,
    ) -> Option<mpsc::Sender<DaemonFrameDown>> {
        if let Some(conn_id) = self.inner.session_conn.get(session_id).map(|r| *r)
            && let Some(entry) = self.inner.conns.get(&conn_id)
        {
            return Some(entry.1.clone());
        }
        let ambiguous = self.inner.machine_conns.get(&machine).is_some_and(|conns| conns.len() > 1);
        if ambiguous {
            tracing::warn!(
                %machine,
                %session_id,
                "refusing to route a session command: the machine identity is shared by \
                 several live daemon connections and this session announced none of them",
            );
            return None;
        }
        self.inner.daemons.get(&machine).map(|r| r.clone())
    }

    /// Whether THIS pod terminates `machine`'s daemon WS.
    pub fn daemon_connected(&self, machine: Uuid) -> bool {
        self.inner.daemons.contains_key(&machine)
    }

    /// Register the outbound channel of a freshly connected enrolled
    /// dispatcher WS (newest connection wins, mirroring the daemon hub).
    pub fn register_dispatcher(&self, dispatcher: Uuid, tx: mpsc::Sender<DispatcherFrameDown>) {
        self.inner.dispatchers.insert(dispatcher, tx);
    }

    /// [`Self::unregister_daemon`] for dispatchers: same-channel guard,
    /// returns whether an entry was removed.
    pub fn unregister_dispatcher(
        &self,
        dispatcher: Uuid,
        tx: &mpsc::Sender<DispatcherFrameDown>,
    ) -> bool {
        self.inner
            .dispatchers
            .remove_if(&dispatcher, |_, current| current.same_channel(tx))
            .is_some()
    }

    /// Unconditionally drop a dispatcher's live connection — used when the
    /// dispatcher identity is deleted so it can't keep operating under a
    /// removed identity (it'll fail to re-auth on reconnect).
    pub fn evict_dispatcher(&self, dispatcher: Uuid) {
        self.inner.dispatchers.remove(&dispatcher);
    }

    /// Whether THIS pod terminates `dispatcher`'s WS.
    pub fn dispatcher_connected(&self, dispatcher: Uuid) -> bool {
        self.inner.dispatchers.contains_key(&dispatcher)
    }

    // ---- live-view PTY watcher refcount ----

    /// Register a browser as watching `session`'s live terminal. Returns `true`
    /// only on the 0→1 transition — the caller then tells the daemon to start
    /// streaming.
    pub fn pty_watch_inc(&self, session: &str) -> bool {
        let mut entry = self.inner.pty_watchers.entry(session.to_owned()).or_insert(0);
        *entry += 1;
        *entry == 1
    }

    /// Drop a watcher. Returns `true` only on the →0 transition (last watcher
    /// left) — the caller then tells the daemon to stop streaming.
    pub fn pty_watch_dec(&self, session: &str) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.inner.pty_watchers.entry(session.to_owned()) {
            Entry::Occupied(mut e) => {
                let v = e.get_mut();
                *v = v.saturating_sub(1);
                if *v == 0 {
                    e.remove();
                    true
                } else {
                    false
                }
            }
            Entry::Vacant(_) => false,
        }
    }

    // ---- point-to-point commands / round-trips ----

    /// Fire-and-forget a [`DaemonFrameDown`] toward `machine`'s daemon WS
    /// (`Reply`, `PermissionResponse`, `Kill`, `Interrupt`, `SetModel`,
    /// `Rename` write-through, `Reconcile` push, …). Local lookup first; a
    /// miss goes to the [`Transport`] (a hard miss under [`NoopTransport`]).
    pub async fn command_daemon(
        &self,
        machine: Uuid,
        frame: DaemonFrameDown,
    ) -> Result<(), BusError> {
        let Some(tx) = self.inner.daemons.get(&machine).map(|r| r.clone()) else {
            return self.inner.transport.forward_daemon(machine, frame).await;
        };
        tx.send(frame).await.map_err(|_| BusError::Closed)
    }

    /// [`Self::command_daemon`] for a frame that belongs to ONE session:
    /// resolved against the announcing connection rather than the machine.
    pub async fn command_daemon_for_session(
        &self,
        machine: Uuid,
        session_id: &str,
        frame: DaemonFrameDown,
    ) -> Result<(), BusError> {
        let Some(tx) = self.session_channel(machine, session_id) else {
            return self.inner.transport.forward_daemon(machine, frame).await;
        };
        tx.send(frame).await.map_err(|_| BusError::Closed)
    }

    /// [`Self::request_daemon`] for a round-trip that belongs to ONE session.
    pub async fn request_daemon_for_session(
        &self,
        machine: Uuid,
        session_id: &str,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        let Some(tx) = self.session_channel(machine, session_id) else {
            return self.inner.transport.request_daemon(machine, request).await;
        };
        self.request_daemon_via(tx, request).await
    }

    /// [`Self::command_daemon_local`] for a frame that belongs to ONE session.
    pub async fn command_daemon_local_for_session(
        &self,
        machine: Uuid,
        session_id: &str,
        frame: DaemonFrameDown,
    ) -> Result<(), BusError> {
        let Some(tx) = self.session_channel(machine, session_id) else {
            return Err(BusError::NoDaemon(machine));
        };
        tx.send(frame).await.map_err(|_| BusError::Closed)
    }

    /// [`Self::request_daemon_local`] for a round-trip that belongs to ONE
    /// session.
    pub async fn request_daemon_local_for_session(
        &self,
        machine: Uuid,
        session_id: &str,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        let Some(tx) = self.session_channel(machine, session_id) else {
            return Err(BusError::NoDaemon(machine));
        };
        self.request_daemon_via(tx, request).await
    }

    /// [`Self::command_daemon`] restricted to THIS pod's registry — a miss is a
    /// hard [`BusError::NoDaemon`], never the transport. Used by the internal
    /// peer-ingest endpoints, whose loop guard is exactly "deliver
    /// locally or fail; never re-forward".
    pub async fn command_daemon_local(
        &self,
        machine: Uuid,
        frame: DaemonFrameDown,
    ) -> Result<(), BusError> {
        let Some(tx) = self.inner.daemons.get(&machine).map(|r| r.clone()) else {
            return Err(BusError::NoDaemon(machine));
        };
        tx.send(frame).await.map_err(|_| BusError::Closed)
    }

    /// Fire-and-forget a [`DispatcherFrameDown`] toward the enrolled
    /// dispatcher's WS. Peer of [`Self::command_daemon`].
    #[allow(dead_code)] // dispatcher traffic is all correlated today; here for API symmetry
    pub async fn command_dispatcher(
        &self,
        dispatcher: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<(), BusError> {
        let Some(tx) = self.inner.dispatchers.get(&dispatcher).map(|r| r.clone()) else {
            return self.inner.transport.forward_dispatcher(dispatcher, frame).await;
        };
        tx.send(frame).await.map_err(|_| BusError::Closed)
    }

    /// [`Self::command_dispatcher`] restricted to THIS pod's registry
    /// (peer-ingest loop guard).
    pub async fn command_dispatcher_local(
        &self,
        dispatcher: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<(), BusError> {
        let Some(tx) = self.inner.dispatchers.get(&dispatcher).map(|r| r.clone()) else {
            return Err(BusError::NoDispatcher(dispatcher));
        };
        tx.send(frame).await.map_err(|_| BusError::Closed)
    }

    /// Correlated daemon round-trip: mint a request id, park the reply
    /// oneshot, send the frame, and await the matching result (fired by the
    /// daemon WS read loop via [`Self::resolve_stage_files`] /
    /// [`Self::resolve_list_dirs`]) within the request's timeout.
    pub async fn request_daemon(
        &self,
        machine: Uuid,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        let Some(tx) = self.inner.daemons.get(&machine).map(|r| r.clone()) else {
            return self.inner.transport.request_daemon(machine, request).await;
        };
        self.request_daemon_via(tx, request).await
    }

    /// [`Self::request_daemon`] restricted to THIS pod's registry (peer-ingest
    /// loop guard) — a miss is a hard [`BusError::NoDaemon`].
    pub async fn request_daemon_local(
        &self,
        machine: Uuid,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        let Some(tx) = self.inner.daemons.get(&machine).map(|r| r.clone()) else {
            return Err(BusError::NoDaemon(machine));
        };
        self.request_daemon_via(tx, request).await
    }

    /// The correlated round-trip against an already-resolved local daemon
    /// channel: mint the request id, park the oneshot, send, await with the
    /// request's timeout.
    async fn request_daemon_via(
        &self,
        tx: mpsc::Sender<DaemonFrameDown>,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        let request_id = Uuid::new_v4();
        match request {
            DaemonRequest::StageFiles { adapter_id, local_id, uploads } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                self.inner.pending_stage.insert(request_id, reply_tx);
                let frame =
                    DaemonFrameDown::StageFiles { request_id, adapter_id, local_id, uploads };
                if tx.send(frame).await.is_err() {
                    self.inner.pending_stage.remove(&request_id);
                    return Err(BusError::Closed);
                }
                match tokio::time::timeout(STAGE_TIMEOUT, reply_rx).await {
                    Ok(Ok(Ok(paths))) => Ok(DaemonResponse::StagedFiles(paths)),
                    Ok(Ok(Err(msg))) => Err(BusError::Staging(msg)),
                    // Sender dropped (daemon disconnected mid-request) — clean
                    // up defensively.
                    Ok(Err(_)) => {
                        self.inner.pending_stage.remove(&request_id);
                        Err(BusError::Closed)
                    }
                    Err(_) => {
                        self.inner.pending_stage.remove(&request_id);
                        Err(BusError::Timeout)
                    }
                }
            }
            DaemonRequest::ListDirs { path } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                self.inner.pending_listdirs.insert(request_id, reply_tx);
                if tx.send(DaemonFrameDown::ListDirs { request_id, path }).await.is_err() {
                    self.inner.pending_listdirs.remove(&request_id);
                    return Err(BusError::Closed);
                }
                match tokio::time::timeout(LIST_DIRS_TIMEOUT, reply_rx).await {
                    Ok(Ok(Ok(dirs))) => Ok(DaemonResponse::Dirs(dirs)),
                    Ok(Ok(Err(msg))) => Err(BusError::ListDirs(msg)),
                    Ok(Err(_)) => {
                        self.inner.pending_listdirs.remove(&request_id);
                        Err(BusError::Closed)
                    }
                    Err(_) => {
                        self.inner.pending_listdirs.remove(&request_id);
                        Err(BusError::Timeout)
                    }
                }
            }
            DaemonRequest::GitInfo { path, include_dirty } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                self.inner.pending_gitinfo.insert(request_id, reply_tx);
                let frame = DaemonFrameDown::GitInfo { request_id, path, include_dirty };
                if tx.send(frame).await.is_err() {
                    self.inner.pending_gitinfo.remove(&request_id);
                    return Err(BusError::Closed);
                }
                match tokio::time::timeout(LIST_DIRS_TIMEOUT, reply_rx).await {
                    Ok(Ok(Ok(info))) => Ok(DaemonResponse::GitInfo(info)),
                    Ok(Ok(Err(msg))) => Err(BusError::GitInfo(msg)),
                    Ok(Err(_)) => {
                        self.inner.pending_gitinfo.remove(&request_id);
                        Err(BusError::Closed)
                    }
                    Err(_) => {
                        self.inner.pending_gitinfo.remove(&request_id);
                        Err(BusError::Timeout)
                    }
                }
            }
            DaemonRequest::ReadFile { path, max_bytes, cwd } => {
                self.read_file_via(tx, request_id, path, max_bytes, cwd).await
            }
            DaemonRequest::Diagnose { adapter_id, local_id } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                self.inner.pending_diagnose.insert(request_id, reply_tx);
                // Rides the generic Command frame: only the adapter's driver
                // can see the per-session facts. The reply arrives as an
                // `AdapterEvent::Diagnose` (routes::daemon fires
                // `resolve_diagnose` with the echoed request id).
                let frame = DaemonFrameDown::Command {
                    adapter_id,
                    command: Box::new(AdapterCommand::Diagnose { local_id, request_id }),
                };
                if tx.send(frame).await.is_err() {
                    self.inner.pending_diagnose.remove(&request_id);
                    return Err(BusError::Closed);
                }
                match tokio::time::timeout(DIAGNOSE_TIMEOUT, reply_rx).await {
                    Ok(Ok(report)) => Ok(DaemonResponse::Diagnose(report)),
                    Ok(Err(_)) => {
                        self.inner.pending_diagnose.remove(&request_id);
                        Err(BusError::Closed)
                    }
                    Err(_) => {
                        self.inner.pending_diagnose.remove(&request_id);
                        Err(BusError::Timeout)
                    }
                }
            }
        }
    }

    async fn read_file_via(
        &self,
        tx: mpsc::Sender<DaemonFrameDown>,
        request_id: Uuid,
        path: String,
        max_bytes: u64,
        cwd: Option<String>,
    ) -> Result<DaemonResponse, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner.pending_readfile.insert(request_id, reply_tx);
        let frame = DaemonFrameDown::ReadFile { request_id, path, max_bytes, cwd };
        if tx.send(frame).await.is_err() {
            self.inner.pending_readfile.remove(&request_id);
            return Err(BusError::Closed);
        }
        match tokio::time::timeout(READ_FILE_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(file))) => Ok(DaemonResponse::File(file)),
            Ok(Ok(Err((kind, msg)))) => Err(BusError::ReadFile(kind, msg)),
            Ok(Err(_)) => {
                self.inner.pending_readfile.remove(&request_id);
                Err(BusError::Closed)
            }
            Err(_) => {
                self.inner.pending_readfile.remove(&request_id);
                Err(BusError::Timeout)
            }
        }
    }

    /// Correlated dispatcher round-trip (Dispatch/Status/Cancel).
    /// The caller mints `request_id` and embeds it in `frame`; the bus parks
    /// the reply oneshot under it and awaits the matching
    /// [`DispatcherFrameUp`] (fired by the dispatcher WS read loop via
    /// [`Self::resolve_dispatcher_reply`]).
    pub async fn request_dispatcher(
        &self,
        dispatcher: Uuid,
        request_id: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<DispatcherFrameUp, BusError> {
        let Some(tx) = self.inner.dispatchers.get(&dispatcher).map(|r| r.clone()) else {
            return self.inner.transport.request_dispatcher(dispatcher, request_id, frame).await;
        };
        self.request_dispatcher_via(tx, request_id, frame).await
    }

    /// [`Self::request_dispatcher`] restricted to THIS pod's registry
    /// (peer-ingest loop guard) — a miss is a hard
    /// [`BusError::NoDispatcher`].
    pub async fn request_dispatcher_local(
        &self,
        dispatcher: Uuid,
        request_id: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<DispatcherFrameUp, BusError> {
        let Some(tx) = self.inner.dispatchers.get(&dispatcher).map(|r| r.clone()) else {
            return Err(BusError::NoDispatcher(dispatcher));
        };
        self.request_dispatcher_via(tx, request_id, frame).await
    }

    async fn request_dispatcher_via(
        &self,
        tx: mpsc::Sender<DispatcherFrameDown>,
        request_id: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<DispatcherFrameUp, BusError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner.pending_dispatcher.insert(request_id, reply_tx);

        if tx.send(frame).await.is_err() {
            self.inner.pending_dispatcher.remove(&request_id);
            return Err(BusError::Closed);
        }

        match tokio::time::timeout(DISPATCHER_TIMEOUT, reply_rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(BusError::Disconnected),
            Err(_) => {
                self.inner.pending_dispatcher.remove(&request_id);
                Err(BusError::Timeout)
            }
        }
    }

    // ---- inbound correlation (WS read loops fire the parked oneshots) ----

    /// Fire the oneshot a stage-files round-trip is awaiting. Returns `false`
    /// for an unknown request id (the route already timed out).
    pub fn resolve_stage_files(&self, request_id: Uuid, outcome: StageFilesOutcome) -> bool {
        // Receiver gone (route timed out already) → drop silently.
        self.inner
            .pending_stage
            .remove(&request_id)
            .map(|(_, reply_tx)| {
                let _ = reply_tx.send(outcome);
            })
            .is_some()
    }

    /// Fire the oneshot a list-dirs round-trip is awaiting. Returns `false`
    /// for an unknown request id.
    pub fn resolve_list_dirs(&self, request_id: Uuid, outcome: ListDirsOutcome) -> bool {
        self.inner
            .pending_listdirs
            .remove(&request_id)
            .map(|(_, reply_tx)| {
                let _ = reply_tx.send(outcome);
            })
            .is_some()
    }

    /// Fire the oneshot a git-info round-trip is awaiting. Returns `false`
    /// for an unknown request id.
    pub fn resolve_git_info(&self, request_id: Uuid, outcome: GitInfoOutcome) -> bool {
        self.inner
            .pending_gitinfo
            .remove(&request_id)
            .map(|(_, reply_tx)| {
                let _ = reply_tx.send(outcome);
            })
            .is_some()
    }

    /// Fire the oneshot a read-file round-trip is awaiting. Returns `false`
    /// for an unknown request id.
    pub fn resolve_read_file(&self, request_id: Uuid, outcome: ReadFileOutcome) -> bool {
        self.inner
            .pending_readfile
            .remove(&request_id)
            .map(|(_, reply_tx)| {
                let _ = reply_tx.send(outcome);
            })
            .is_some()
    }

    /// Fire the oneshot a session-diagnose round-trip is awaiting.
    /// Returns `false` for an unknown request id (the route already timed
    /// out, or a spooled reply was replayed after a daemon reconnect).
    pub fn resolve_diagnose(
        &self,
        request_id: Uuid,
        report: Box<cctui_proto::diagnose::SessionDiagnose>,
    ) -> bool {
        self.inner
            .pending_diagnose
            .remove(&request_id)
            .map(|(_, reply_tx)| {
                let _ = reply_tx.send(report);
            })
            .is_some()
    }

    /// Fire the oneshot a dispatcher round-trip is awaiting. Returns `false`
    /// for an unknown request id.
    pub fn resolve_dispatcher_reply(&self, request_id: Uuid, frame: DispatcherFrameUp) -> bool {
        self.inner
            .pending_dispatcher
            .remove(&request_id)
            .map(|(_, reply_tx)| {
                let _ = reply_tx.send(frame);
            })
            .is_some()
    }

    // ---- pub/sub ----

    /// Publish an event to this pod's subscribers, handing it to the
    /// transport for cross-pod relay (a no-op under [`NoopTransport`]).
    /// Infallible for callers: "nobody listening" is not an error, matching
    /// the pre-bus `let _ = tx.send(..)` discipline.
    pub fn publish(&self, event: BusEvent) {
        self.inner.transport.relay(&event);
        self.deliver_local(event);
    }

    /// Deliver an event to THIS pod's subscribers only, without handing it to
    /// the transport. This is the peer-ingest half of [`Self::publish`]:
    /// events relayed from another pod land here, so they can never
    /// be re-relayed and loop around the mesh.
    pub fn deliver_local(&self, event: BusEvent) {
        match event {
            BusEvent::Session { session_id, event: agent_event } => {
                if let Some(tx) = self.inner.session_streams.get(&session_id) {
                    let _ = tx.send(agent_event);
                }
            }
            BusEvent::Server(server_event) => {
                let _ = self.inner.server_tx.send(server_event);
            }
        }
    }

    /// Convenience for the dominant call shape: publish a [`ServerEvent`].
    pub fn publish_server(&self, event: ServerEvent) {
        self.publish(BusEvent::Server(event));
    }

    /// Subscribe to a registered session's agent stream. `None` for sessions
    /// with no live stream channel (historical/terminated) — this is expected,
    /// mirroring the pre-bus `registry.subscribe`.
    pub fn subscribe_session(&self, session_id: &str) -> Option<broadcast::Receiver<AgentEvent>> {
        self.inner.session_streams.get(session_id).map(|tx| tx.subscribe())
    }

    /// Subscribe to the cluster-wide server event stream.
    pub fn subscribe_server(&self) -> broadcast::Receiver<ServerEvent> {
        self.inner.server_tx.subscribe()
    }

    /// Create (or reuse) the stream channel for a registering session and
    /// return its sender. Reuse-on-reregister keeps current WS subscribers
    /// from seeing the broadcast channel close and losing their stream until
    /// they manually reopen the pane — the pre-bus `registry.register`
    /// semantics.
    pub fn register_session_stream(&self, session_id: &str) -> broadcast::Sender<AgentEvent> {
        self.inner
            .session_streams
            .entry(session_id.to_owned())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone()
    }

    /// Drop a deregistering session's stream channel; live subscribers see
    /// `Closed` once the last sender is gone, as they did when the registry
    /// handle owned the channel.
    pub fn deregister_session_stream(&self, session_id: &str) {
        self.inner.session_streams.remove(session_id);
    }
}

// ---- session/machine-addressed helpers (the retired `daemon_dispatch.rs`) ----
//
// These resolve DB-level addressing (session → machine/adapter) and then use
// the bus primitives. Best-effort: a call that finds no matching session, no
// `adapter_id`, or no connected daemon returns an error which callers can log
// and ignore.

/// Send `command` to the daemon owning `session_id`. Looks up
/// `(machine_uuid, adapter_id)` from the `sessions` table.
pub async fn dispatch(
    state: &AppState,
    session_id: &str,
    command: AdapterCommand,
) -> Result<(), BusError> {
    let (adapter_id, machine_uuid) = resolve_session(state, session_id).await?;
    state
        .bus
        .command_daemon_for_session(
            machine_uuid,
            session_id,
            DaemonFrameDown::Command { adapter_id, command: Box::new(command) },
        )
        .await
}

/// Rebuild and live-push a fresh [`DaemonFrameDown::Reconcile`] to `machine_id`'s
/// connected daemon. Used when a per-user setting the reconcile derives
/// from (e.g. `harnessMode`) changes, so a daemon picks up the new config without
/// waiting for a reconnect. Best-effort: same `NoDaemon`/`Closed` handling as
/// [`dispatch`] — a machine with no live WS is a no-op error the caller ignores.
pub async fn push_reconcile(state: &AppState, machine_id: Uuid) -> Result<(), BusError> {
    let adapters = crate::routes::daemon::load_reconcile(state, machine_id)
        .await
        .map_err(|e| BusError::Reconcile(e.to_string()))?;
    let secret_scrub = crate::routes::daemon::load_scrub_config(state, machine_id).await;
    state
        .bus
        .command_daemon(machine_id, DaemonFrameDown::Reconcile { adapters, secret_scrub })
        .await
}

/// Stage mid-chat attachment `files` for `session_id` and return the
/// staged absolute paths reported by the owning daemon. Same session
/// resolution as [`dispatch`]; the correlated round-trip (request id, parked
/// oneshot, timeout) lives inside [`Bus::request_daemon`].
pub async fn stage_files(
    state: &AppState,
    session_id: &str,
    files: Vec<BootstrapFile>,
) -> Result<Vec<String>, BusError> {
    let (adapter_id, machine_uuid) = resolve_session(state, session_id).await?;
    let response = state
        .bus
        .request_daemon_for_session(
            machine_uuid,
            session_id,
            DaemonRequest::StageFiles {
                adapter_id,
                local_id: session_id.to_owned(),
                uploads: files,
            },
        )
        .await?;
    match response {
        DaemonResponse::StagedFiles(paths) => Ok(paths),
        _ => Err(BusError::Closed), // unreachable: variant-matched
    }
}

/// Ask the daemon owning `session_id` for its session-diagnose report.
/// Same session resolution as [`dispatch`]; the correlated
/// round-trip (request id, parked oneshot, timeout) lives inside
/// [`Bus::request_daemon`].
pub async fn diagnose(
    state: &AppState,
    session_id: &str,
) -> Result<Box<cctui_proto::diagnose::SessionDiagnose>, BusError> {
    let (adapter_id, machine_uuid) = resolve_session(state, session_id).await?;
    let response = state
        .bus
        .request_daemon_for_session(
            machine_uuid,
            session_id,
            DaemonRequest::Diagnose { adapter_id, local_id: session_id.to_owned() },
        )
        .await?;
    match response {
        DaemonResponse::Diagnose(report) => Ok(report),
        _ => Err(BusError::Closed), // unreachable: variant-matched
    }
}

/// Ask the daemon on `machine_uuid` for the sub-directories of `path`
/// (working-directory autocomplete). Machine-addressed — no session involved.
pub async fn list_dirs(
    state: &AppState,
    machine_uuid: Uuid,
    path: String,
) -> Result<Vec<String>, BusError> {
    match state.bus.request_daemon(machine_uuid, DaemonRequest::ListDirs { path }).await? {
        DaemonResponse::Dirs(dirs) => Ok(dirs),
        _ => Err(BusError::Closed), // unreachable: variant-matched
    }
}

/// Ask the daemon on `machine_uuid` for the git facts of `path` (spawn
/// dialog branch badge). Machine-addressed — no session involved.
pub async fn git_info(
    state: &AppState,
    machine_uuid: Uuid,
    path: String,
    include_dirty: bool,
) -> Result<GitInfo, BusError> {
    let request = DaemonRequest::GitInfo { path, include_dirty };
    match state.bus.request_daemon(machine_uuid, request).await? {
        DaemonResponse::GitInfo(info) => Ok(info),
        _ => Err(BusError::Closed), // unreachable: variant-matched
    }
}

/// Ask the daemon on `machine_uuid` for the bytes of `path` (a file an agent
/// linked in a message). `cwd` widens the daemon's allow-list to the
/// session's working directory.
pub async fn read_file(
    state: &AppState,
    machine_uuid: Uuid,
    path: String,
    max_bytes: u64,
    cwd: Option<String>,
) -> Result<ReadFileOk, BusError> {
    let request = DaemonRequest::ReadFile { path, max_bytes, cwd };
    match state.bus.request_daemon(machine_uuid, request).await? {
        DaemonResponse::File(file) => Ok(file),
        _ => Err(BusError::Closed), // unreachable: variant-matched
    }
}

/// Resolve a session id to its `(adapter_id, machine_uuid)` addressing pair.
async fn resolve_session(state: &AppState, session_id: &str) -> Result<(String, Uuid), BusError> {
    let row: Option<(Option<String>, Option<Uuid>)> =
        sqlx::query_as("SELECT adapter_id, machine_uuid FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await?;
    let (adapter_id, machine_uuid) = row.ok_or(BusError::NotFound)?;
    let adapter_id = adapter_id.ok_or(BusError::NoAdapter)?;
    let machine_uuid = machine_uuid.ok_or(BusError::NoMachine)?;
    Ok((adapter_id, machine_uuid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> Bus {
        Bus::new(Box::new(NoopTransport))
    }

    fn text_event(content: &str) -> AgentEvent {
        AgentEvent::Text {
            content: content.into(),
            meta: false,
            kind: None,
            ts: 0,
            message_id: None,
            usage: None,
            seq: None,
        }
    }

    #[tokio::test]
    async fn command_daemon_delivers_locally() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        bus.register_daemon(machine, Uuid::new_v4(), tx);

        bus.command_daemon(
            machine,
            DaemonFrameDown::Reconcile {
                adapters: Vec::new(),
                secret_scrub: cctui_proto::ws::SecretScrubConfig::default(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(rx.recv().await, Some(DaemonFrameDown::Reconcile { .. })));
    }

    fn reply(local_id: &str) -> DaemonFrameDown {
        DaemonFrameDown::Command {
            adapter_id: "claude-code".into(),
            command: Box::new(AdapterCommand::Reply {
                local_id: local_id.into(),
                text: "hi".into(),
                ask_picks: None,
                env: std::collections::BTreeMap::new(),
                command_id: None,
            }),
        }
    }

    fn replied_to(frame: &DaemonFrameDown) -> String {
        let DaemonFrameDown::Command { command, .. } = frame else { panic!("expected Command") };
        let AdapterCommand::Reply { local_id, .. } = &**command else { panic!("expected Reply") };
        local_id.clone()
    }

    /// Two dispatched worker pods share one `dispatch` machine identity: each
    /// session's command must reach the connection that announced it, never
    /// whichever pod connected last.
    #[tokio::test]
    async fn session_commands_route_to_the_announcing_connection() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        let conn_a = Uuid::new_v4();
        let conn_b = Uuid::new_v4();
        bus.register_daemon(machine, conn_a, tx_a);
        bus.register_daemon(machine, conn_b, tx_b);
        bus.bind_session_conn("sess-a", conn_a);
        bus.bind_session_conn("sess-b", conn_b);

        bus.command_daemon_for_session(machine, "sess-a", reply("sess-a")).await.unwrap();
        bus.command_daemon_for_session(machine, "sess-b", reply("sess-b")).await.unwrap();

        assert_eq!(replied_to(&rx_a.recv().await.unwrap()), "sess-a");
        assert_eq!(replied_to(&rx_b.recv().await.unwrap()), "sess-b");
        assert!(rx_a.try_recv().is_err(), "conn A must not see conn B's session");
        assert!(rx_b.try_recv().is_err(), "conn B must not see conn A's session");
    }

    /// The ordinary single-connection machine: an unannounced session still
    /// routes exactly as it did before per-connection routing existed.
    #[tokio::test]
    async fn single_connection_machine_keeps_the_machine_fallback() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        bus.register_daemon(machine, Uuid::new_v4(), tx);

        bus.command_daemon_for_session(machine, "never-announced", reply("never-announced"))
            .await
            .unwrap();
        assert_eq!(replied_to(&rx.recv().await.unwrap()), "never-announced");
    }

    /// A shared identity with no binding is ambiguous: refuse loudly rather
    /// than deliver one worker's command to another's.
    #[tokio::test]
    async fn unbound_session_on_a_shared_machine_is_no_daemon() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        bus.register_daemon(machine, Uuid::new_v4(), tx_a);
        bus.register_daemon(machine, Uuid::new_v4(), tx_b);

        let err = bus
            .command_daemon_for_session(machine, "never-announced", reply("never-announced"))
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::NoDaemon(m) if m == machine));
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());
    }

    fn child_spawn(parent: &str) -> DaemonFrameDown {
        DaemonFrameDown::Command {
            adapter_id: "codex".into(),
            command: Box::new(AdapterCommand::Spawn {
                spec: cctui_proto::adapter::SessionSpec {
                    adapter_id: cctui_proto::adapter::AdapterId::new("codex"),
                    working_dir: Some("/workspace".into()),
                    prompt: Some("review".into()),
                    name: None,
                    permission_mode: None,
                    effort: None,
                    model: None,
                    service_tier: None,
                    env: std::collections::BTreeMap::new(),
                    bootstrap: serde_json::Value::Null,
                    parent_local_id: Some(parent.to_owned()),
                },
                command_id: Some(Uuid::new_v4()),
                session_id: Some(Uuid::new_v4()),
            }),
        }
    }

    /// A `CctuiAgent` child must boot next to its parent: the spawn frame
    /// follows the parent's announcing connection, not the pod that connected
    /// last under the shared `dispatch` identity.
    #[tokio::test]
    async fn a_child_spawn_lands_on_the_parents_connection() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx_parent, mut rx_parent) = mpsc::channel(8);
        let (tx_newest, mut rx_newest) = mpsc::channel(8);
        let conn_parent = Uuid::new_v4();
        bus.register_daemon(machine, conn_parent, tx_parent);
        bus.bind_session_conn("parent", conn_parent);
        bus.register_daemon(machine, Uuid::new_v4(), tx_newest);

        let frame = child_spawn("parent");
        assert_eq!(crate::bus::peer::frame_session(&frame), Some("parent"));
        bus.command_daemon_for_session(machine, "parent", frame).await.unwrap();
        assert!(matches!(rx_parent.recv().await, Some(DaemonFrameDown::Command { .. })));
        assert!(rx_newest.try_recv().is_err());
    }

    /// One pod's exit must not unbind its neighbour's sessions, and the
    /// surviving connection becomes the machine's sole — hence unambiguous —
    /// daemon again.
    #[tokio::test]
    async fn closing_a_connection_drops_only_its_own_bindings() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx_a, _rx_a) = mpsc::channel::<DaemonFrameDown>(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        let conn_a = Uuid::new_v4();
        let conn_b = Uuid::new_v4();
        bus.register_daemon(machine, conn_a, tx_a.clone());
        bus.register_daemon(machine, conn_b, tx_b.clone());
        bus.bind_session_conn("sess-a", conn_a);
        bus.bind_session_conn("sess-b", conn_b);

        // A's WS closes. B is the newest connection, so the machine entry is
        // still B's and A's cleanup must leave it alone.
        assert!(!bus.unregister_daemon(machine, conn_a, &tx_a));

        bus.command_daemon_for_session(machine, "sess-b", reply("sess-b")).await.unwrap();
        assert_eq!(replied_to(&rx_b.recv().await.unwrap()), "sess-b");

        // A's binding is gone, but the machine now has one live connection, so
        // the fallback applies again.
        bus.command_daemon_for_session(machine, "sess-a", reply("sess-a")).await.unwrap();
        assert_eq!(replied_to(&rx_b.recv().await.unwrap()), "sess-a");

        assert!(bus.unregister_daemon(machine, conn_b, &tx_b));
        let err =
            bus.command_daemon_for_session(machine, "sess-b", reply("sess-b")).await.unwrap_err();
        assert!(matches!(err, BusError::NoDaemon(m) if m == machine));
    }

    /// A session that migrates to a new pod follows its newest announcement.
    #[tokio::test]
    async fn rebinding_a_session_moves_it_to_the_new_connection() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        let conn_a = Uuid::new_v4();
        let conn_b = Uuid::new_v4();
        bus.register_daemon(machine, conn_a, tx_a);
        bus.register_daemon(machine, conn_b, tx_b);

        bus.bind_session_conn("sess", conn_a);
        bus.command_daemon_for_session(machine, "sess", reply("sess")).await.unwrap();
        assert!(rx_a.recv().await.is_some());

        bus.bind_session_conn("sess", conn_b);
        bus.command_daemon_for_session(machine, "sess", reply("sess")).await.unwrap();
        assert!(rx_b.recv().await.is_some());
        assert!(rx_a.try_recv().is_err());
    }

    /// Correlated round-trips take the same per-connection address.
    #[tokio::test]
    async fn session_round_trips_route_to_the_announcing_connection() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel::<DaemonFrameDown>(8);
        let conn_a = Uuid::new_v4();
        bus.register_daemon(machine, conn_a, tx_a);
        bus.register_daemon(machine, Uuid::new_v4(), tx_b);
        bus.bind_session_conn("sess-a", conn_a);

        let bus2 = bus.clone();
        let fake = tokio::spawn(async move {
            let Some(DaemonFrameDown::StageFiles { request_id, .. }) = rx_a.recv().await else {
                panic!("expected StageFiles on conn A");
            };
            assert!(bus2.resolve_stage_files(request_id, Ok(vec!["/tmp/a.txt".into()])));
        });

        let response = bus
            .request_daemon_for_session(
                machine,
                "sess-a",
                DaemonRequest::StageFiles {
                    adapter_id: "claude-code".into(),
                    local_id: "sess-a".into(),
                    uploads: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(response, DaemonResponse::StagedFiles(p) if p == vec!["/tmp/a.txt"]));
        fake.await.unwrap();
        assert!(rx_b.try_recv().is_err(), "the sibling connection must see nothing");
    }

    #[test]
    fn pty_watch_refcount_signals_only_on_edge_transitions() {
        let bus = bus();
        assert!(bus.pty_watch_inc("s"), "0→1 must signal start");
        assert!(!bus.pty_watch_inc("s"), "1→2 must not re-signal");
        assert!(!bus.pty_watch_inc("s"), "2→3 must not re-signal");
        assert!(!bus.pty_watch_dec("s"), "3→2 must not signal stop");
        assert!(!bus.pty_watch_dec("s"), "2→1 must not signal stop");
        assert!(bus.pty_watch_dec("s"), "1→0 must signal stop");
        assert!(!bus.pty_watch_dec("s"), "stray dec on unwatched must not signal");
        assert!(bus.pty_watch_inc("s"), "0→1 again must signal start");
    }

    #[tokio::test]
    async fn command_daemon_miss_is_no_daemon_under_noop() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let err = bus
            .command_daemon(
                machine,
                DaemonFrameDown::Reconcile {
                    adapters: Vec::new(),
                    secret_scrub: cctui_proto::ws::SecretScrubConfig::default(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::NoDaemon(m) if m == machine));
    }

    #[tokio::test]
    async fn command_daemon_closed_channel_errors() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(1);
        bus.register_daemon(machine, Uuid::new_v4(), tx);
        drop(rx);
        let err = bus
            .command_daemon(
                machine,
                DaemonFrameDown::Reconcile {
                    adapters: Vec::new(),
                    secret_scrub: cctui_proto::ws::SecretScrubConfig::default(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::Closed));
    }

    /// disconnect cleanup only removes the entry when it is still the
    /// same channel — a reconnect's newer channel must survive the old WS
    /// task's cleanup.
    #[tokio::test]
    async fn unregister_daemon_guards_reconnect_race() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (old_tx, _old_rx) = mpsc::channel::<DaemonFrameDown>(1);
        let (new_tx, mut new_rx) = mpsc::channel::<DaemonFrameDown>(8);

        let old_conn = Uuid::new_v4();
        bus.register_daemon(machine, old_conn, old_tx.clone());
        // Reconnect: newest connection wins.
        bus.register_daemon(machine, Uuid::new_v4(), new_tx);
        // Old task's cleanup: entry is no longer ours — must NOT remove it.
        assert!(!bus.unregister_daemon(machine, old_conn, &old_tx));
        assert!(bus.daemon_connected(machine));

        bus.command_daemon(
            machine,
            DaemonFrameDown::Reconcile {
                adapters: Vec::new(),
                secret_scrub: cctui_proto::ws::SecretScrubConfig::default(),
            },
        )
        .await
        .unwrap();
        assert!(new_rx.recv().await.is_some());
    }

    /// The full stage-files round-trip against a fake daemon: the WS read loop
    /// side is `resolve_stage_files`, exactly what `routes::daemon` does.
    #[tokio::test]
    async fn request_daemon_stage_files_round_trip() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        bus.register_daemon(machine, Uuid::new_v4(), tx);

        let bus2 = bus.clone();
        let fake_daemon = tokio::spawn(async move {
            let Some(DaemonFrameDown::StageFiles { request_id, .. }) = rx.recv().await else {
                panic!("expected StageFiles");
            };
            assert!(bus2.resolve_stage_files(request_id, Ok(vec!["/tmp/a.txt".into()])));
        });

        let response = bus
            .request_daemon(
                machine,
                DaemonRequest::StageFiles {
                    adapter_id: "claude-code".into(),
                    local_id: "sess-1".into(),
                    uploads: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(response, DaemonResponse::StagedFiles(p) if p == vec!["/tmp/a.txt"]));
        fake_daemon.await.unwrap();
    }

    #[tokio::test]
    async fn request_daemon_list_dirs_error_surfaces() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        bus.register_daemon(machine, Uuid::new_v4(), tx);

        let bus2 = bus.clone();
        tokio::spawn(async move {
            let Some(DaemonFrameDown::ListDirs { request_id, .. }) = rx.recv().await else {
                panic!("expected ListDirs");
            };
            bus2.resolve_list_dirs(request_id, Err("no such directory".into()));
        });

        let err = bus
            .request_daemon(machine, DaemonRequest::ListDirs { path: "/nope".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::ListDirs(m) if m == "no such directory"));
    }

    #[tokio::test]
    async fn resolve_unknown_request_id_reports_false() {
        let bus = bus();
        assert!(!bus.resolve_stage_files(Uuid::new_v4(), Ok(Vec::new())));
        assert!(!bus.resolve_list_dirs(Uuid::new_v4(), Ok(Vec::new())));
        assert!(!bus.resolve_diagnose(Uuid::new_v4(), Box::new(dummy_report("s1"))));
    }

    fn dummy_report(local_id: &str) -> cctui_proto::diagnose::SessionDiagnose {
        use cctui_proto::diagnose::{DiagnoseFact, SessionDiagnose};
        SessionDiagnose {
            local_id: local_id.into(),
            short: None,
            generated_at_ms: 1,
            adapter: "claude-code".into(),
            effective_state: DiagnoseFact::missing("activity", "n/a"),
            last_hook_event: DiagnoseFact::missing("hook", "n/a"),
            attach: DiagnoseFact::missing("attach", "n/a"),
            pty_output: DiagnoseFact::missing("pty", "n/a"),
            claude_socket: DiagnoseFact::missing("discovery", "n/a"),
            transcript: DiagnoseFact::missing("filesystem", "n/a"),
            prompts: DiagnoseFact::missing("hook", "n/a"),
            permission_mode: DiagnoseFact::missing("spawn", "n/a"),
            dispatch: DiagnoseFact::missing("dispatch", "n/a"),
            gateway: DiagnoseFact::missing("daemon-config", "n/a"),
            codex: None,
        }
    }

    /// The diagnose round-trip: the request goes down as a generic
    /// `Command` frame carrying `AdapterCommand::Diagnose` with the bus-minted
    /// request id, and `resolve_diagnose` (fired by the WS read loop on the
    /// echoed `AdapterEvent::Diagnose`) completes it.
    #[tokio::test]
    async fn request_daemon_diagnose_round_trip() {
        let bus = bus();
        let machine = Uuid::new_v4();
        let (tx, mut rx) = mpsc::channel(8);
        bus.register_daemon(machine, Uuid::new_v4(), tx);

        let bus2 = bus.clone();
        let fake_daemon = tokio::spawn(async move {
            let Some(DaemonFrameDown::Command { adapter_id, command }) = rx.recv().await else {
                panic!("expected Command");
            };
            assert_eq!(adapter_id, "claude-code");
            let AdapterCommand::Diagnose { local_id, request_id } = *command else {
                panic!("expected AdapterCommand::Diagnose");
            };
            assert_eq!(local_id, "sess-1");
            assert!(bus2.resolve_diagnose(request_id, Box::new(dummy_report(&local_id))));
        });

        let response = bus
            .request_daemon(
                machine,
                DaemonRequest::Diagnose {
                    adapter_id: "claude-code".into(),
                    local_id: "sess-1".into(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(response, DaemonResponse::Diagnose(r) if r.local_id == "sess-1"));
        fake_daemon.await.unwrap();
    }

    #[tokio::test]
    async fn dispatcher_round_trip_and_offline() {
        let bus = bus();
        let dispatcher = Uuid::new_v4();

        // Offline: fails fast with the miss error.
        let request_id = Uuid::new_v4();
        let err = bus
            .request_dispatcher(
                dispatcher,
                request_id,
                DispatcherFrameDown::Status { request_id, handle: "h".into() },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BusError::NoDispatcher(d) if d == dispatcher));

        // Online: reply correlates by request id.
        let (tx, mut rx) = mpsc::channel(8);
        bus.register_dispatcher(dispatcher, tx);
        let bus2 = bus.clone();
        let fake = tokio::spawn(async move {
            let Some(DispatcherFrameDown::Status { request_id, .. }) = rx.recv().await else {
                panic!("expected Status");
            };
            assert!(bus2.resolve_dispatcher_reply(
                request_id,
                DispatcherFrameUp::StatusResult {
                    request_id,
                    handle: "h".into(),
                    state: Some("complete".into()),
                    error: None,
                },
            ));
        });
        let request_id = Uuid::new_v4();
        let reply = bus
            .request_dispatcher(
                dispatcher,
                request_id,
                DispatcherFrameDown::Status { request_id, handle: "h".into() },
            )
            .await
            .unwrap();
        assert!(
            matches!(reply, DispatcherFrameUp::StatusResult { state: Some(s), .. } if s == "complete")
        );
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn publish_server_reaches_subscribers() {
        let bus = bus();
        let mut rx = bus.subscribe_server();
        bus.publish(BusEvent::Server(ServerEvent::SessionDeregistered {
            session_id: "sess-1".into(),
        }));
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServerEvent::SessionDeregistered { session_id } if session_id == "sess-1"
        ));
    }

    #[tokio::test]
    async fn session_stream_register_subscribe_publish() {
        let bus = bus();
        // No stream registered → historical session → None.
        assert!(bus.subscribe_session("sess-1").is_none());

        bus.register_session_stream("sess-1");
        let mut rx = bus.subscribe_session("sess-1").unwrap();
        bus.publish(BusEvent::Session { session_id: "sess-1".into(), event: text_event("hello") });
        assert!(
            matches!(rx.try_recv().unwrap(), AgentEvent::Text { content, .. } if content == "hello")
        );

        // Publishing to an unknown session is a silent no-op.
        bus.publish(BusEvent::Session { session_id: "ghost".into(), event: text_event("x") });
    }

    /// Re-registration reuses the existing channel so live subscribers keep
    /// their stream (the pre-bus `registry.register` semantics); deregistration
    /// closes it.
    #[tokio::test]
    async fn session_stream_reuse_and_close() {
        let bus = bus();
        let first = bus.register_session_stream("sess-1");
        let mut rx = bus.subscribe_session("sess-1").unwrap();

        let again = bus.register_session_stream("sess-1");
        assert!(first.same_channel(&again));
        again.send(text_event("still here")).unwrap();
        assert!(rx.try_recv().is_ok());

        drop(first);
        drop(again);
        bus.deregister_session_stream("sess-1");
        assert!(bus.subscribe_session("sess-1").is_none());
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Closed)));
    }
}
