//! `PeerHttpTransport` (phase 2 of the message-bus architecture):
//! cross-replica routing + event fan-out over plain pod-to-pod HTTP.
//!
//! With multiple server replicas, a daemon/dispatcher WS is terminated by
//! exactly one pod while browser/API traffic load-balances across all of them.
//! This transport fills the [`super::Transport`] seam:
//!
//!   * **route** (commands + correlated round-trips): a local registry miss
//!     consults `ws_presence` for a live peer owning the WS and POSTs the frame
//!     to that pod's `/internal/bus/route`, returning the peer's outcome. No
//!     live owner ⇒ the same `NoDaemon`/`NoDispatcher` miss the caller would
//!     have seen locally, so the webui ack goes red honestly.
//!   * **relay** (publish fan-out): every locally-published [`BusEvent`] is
//!     queued to a background worker that batches and POSTs it to every live
//!     peer pod (from the `pods` table) at `/internal/bus/publish`. Best-effort
//!     with a short timeout — DB persistence remains the source of truth for
//!     refetch, exactly as today.
//!
//! The receiving pod's internal endpoints (`crate::routes::internal`) deliver
//! LOCALLY only (`Bus::*_local` / [`super::Bus::deliver_local`]) — the loop
//! guard: a forwarded frame or relayed event can never be re-forwarded.
//!
//! Auth is an internal shared secret minted once into `cluster_secrets` at
//! first boot and read by every replica; requests carry it as a Bearer token
//! and ingest compares it in constant time. It is never a user-facing
//! credential and no user/machine token can reach these endpoints.

use cctui_proto::adapter::BootstrapFile;
use cctui_proto::git::GitInfo;
use cctui_proto::ws::{
    AgentEvent, DaemonFrameDown, DispatcherFrameDown, DispatcherFrameUp, ReadFileErrorKind,
    ReadFileOk, ServerEvent,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{BusError, BusEvent, DaemonRequest, DaemonResponse, Transport};
use crate::presence::{self, Kind};

/// Timeout for a forwarded fire-and-forget command (the peer only has to drop
/// the frame on a local channel).
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Timeout for a forwarded stage-files round-trip: the peer's own in-bus wait
/// is `STAGE_TIMEOUT` (30s), plus headroom for the upload body transfer.
const STAGE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(40);
/// Timeout for a forwarded list-dirs round-trip (peer waits 3s).
const LIST_DIRS_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
/// Timeout for a forwarded session-diagnose round-trip (peer waits 10s).
const DIAGNOSE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Outlasts the bus's 60 s `READ_FILE_TIMEOUT`.
const READ_FILE_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(70);
/// Timeout for a forwarded dispatcher round-trip (peer waits 30s).
const DISPATCHER_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
/// Timeout for one relay POST to one peer. Publish is best-effort; a slow peer
/// must not back the relay queue up for long.
const RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Max events drained into one relay batch.
const RELAY_BATCH: usize = 64;
/// How long a fetched peer-pod list is reused before re-querying `pods`.
const PEER_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

// ---- wire types (shared with `crate::routes::internal`) ----

/// Body of `POST /internal/bus/route`: one frame/round-trip addressed at a WS
/// the receiving pod is believed to hold.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RouteRequest {
    DaemonCommand {
        machine: Uuid,
        frame: DaemonFrameDown,
    },
    DaemonStageFiles {
        machine: Uuid,
        adapter_id: String,
        local_id: String,
        uploads: Vec<BootstrapFile>,
    },
    DaemonListDirs {
        machine: Uuid,
        path: String,
    },
    DaemonGitInfo {
        machine: Uuid,
        path: String,
        include_dirty: bool,
    },
    DaemonReadFile {
        machine: Uuid,
        path: String,
        max_bytes: u64,
        cwd: Option<String>,
    },
    /// Session-diagnose round-trip: the receiving pod runs the full
    /// command-down / event-up correlation locally and returns the report.
    DaemonDiagnose {
        machine: Uuid,
        adapter_id: String,
        local_id: String,
    },
    DispatcherCommand {
        dispatcher: Uuid,
        frame: DispatcherFrameDown,
    },
    /// Correlated dispatcher round-trip; `request_id` is the caller-minted
    /// correlation id already embedded in `frame`.
    DispatcherRequest {
        dispatcher: Uuid,
        request_id: Uuid,
        frame: DispatcherFrameDown,
    },
}

/// Body of the `POST /internal/bus/route` response. Always HTTP 200 from the
/// handler; delivery failures ride the `Err` variant so [`BusError`] semantics
/// survive the hop.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RouteResponse {
    Ok,
    StagedFiles { paths: Vec<String> },
    Dirs { dirs: Vec<String> },
    GitInfo { info: GitInfo },
    File { file: ReadFileOk },
    Diagnose { report: Box<cctui_proto::diagnose::SessionDiagnose> },
    DispatcherReply { frame: DispatcherFrameUp },
    Err { code: WireErrorCode, message: String },
}

/// [`BusError`] variants that must survive the wire with their meaning intact
/// (callers match on them). Everything else collapses to `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorCode {
    NoDaemon,
    NoDispatcher,
    Closed,
    Disconnected,
    Timeout,
    Staging,
    ListDirs,
    GitInfo,
    ReadFile { kind: ReadFileErrorKind },
    Other,
}

/// Encode a [`BusError`] for the route response. Payload-carrying variants
/// (`Staging`/`ListDirs`) ship their inner message so [`decode_error`] can
/// rebuild the variant without double-wrapping the display prefix.
pub fn encode_error(err: &BusError) -> (WireErrorCode, String) {
    match err {
        BusError::NoDaemon(_) => (WireErrorCode::NoDaemon, err.to_string()),
        BusError::NoDispatcher(_) => (WireErrorCode::NoDispatcher, err.to_string()),
        BusError::Closed => (WireErrorCode::Closed, err.to_string()),
        BusError::Disconnected => (WireErrorCode::Disconnected, err.to_string()),
        BusError::Timeout => (WireErrorCode::Timeout, err.to_string()),
        BusError::Staging(msg) => (WireErrorCode::Staging, msg.clone()),
        BusError::ListDirs(msg) => (WireErrorCode::ListDirs, msg.clone()),
        BusError::GitInfo(msg) => (WireErrorCode::GitInfo, msg.clone()),
        BusError::ReadFile(kind, msg) => (WireErrorCode::ReadFile { kind: *kind }, msg.clone()),
        BusError::NotFound
        | BusError::NoAdapter
        | BusError::NoMachine
        | BusError::Db(_)
        | BusError::Reconcile(_)
        | BusError::Transport(_) => (WireErrorCode::Other, err.to_string()),
    }
}

/// Reconstruct a [`BusError`] from a peer's error response. `target` is the
/// machine/dispatcher uuid the caller addressed (the wire doesn't re-carry it).
pub fn decode_error(code: WireErrorCode, message: String, target: Uuid) -> BusError {
    match code {
        WireErrorCode::NoDaemon => BusError::NoDaemon(target),
        WireErrorCode::NoDispatcher => BusError::NoDispatcher(target),
        WireErrorCode::Closed => BusError::Closed,
        WireErrorCode::Disconnected => BusError::Disconnected,
        WireErrorCode::Timeout => BusError::Timeout,
        WireErrorCode::Staging => BusError::Staging(message),
        WireErrorCode::ListDirs => BusError::ListDirs(message),
        WireErrorCode::GitInfo => BusError::GitInfo(message),
        WireErrorCode::ReadFile { kind } => BusError::ReadFile(kind, message),
        // An unclassified peer-side failure still means the frame was not
        // delivered; surface the peer's message verbatim.
        WireErrorCode::Other => BusError::Transport(message),
    }
}

/// Wire form of [`BusEvent`] for `POST /internal/bus/publish` (a batch of
/// these). `BusEvent` itself stays wire-agnostic; this owns the serde shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireBusEvent {
    Session { session_id: String, event: AgentEvent },
    Server { event: ServerEvent },
}

impl From<&BusEvent> for WireBusEvent {
    fn from(ev: &BusEvent) -> Self {
        match ev {
            BusEvent::Session { session_id, event } => {
                Self::Session { session_id: session_id.clone(), event: event.clone() }
            }
            BusEvent::Server(event) => Self::Server { event: event.clone() },
        }
    }
}

impl From<WireBusEvent> for BusEvent {
    fn from(ev: WireBusEvent) -> Self {
        match ev {
            WireBusEvent::Session { session_id, event } => Self::Session { session_id, event },
            WireBusEvent::Server { event } => Self::Server(event),
        }
    }
}

/// The session a down-frame belongs to, when it is session-scoped.
pub fn frame_session(frame: &DaemonFrameDown) -> Option<&str> {
    match frame {
        DaemonFrameDown::Command { command, .. } => command.local_id(),
        _ => None,
    }
}

/// Format a peer IP + port as an HTTP authority (IPv6 literals need brackets).
fn peer_base(ip: &str, port: u16) -> String {
    if ip.contains(':') { format!("http://[{ip}]:{port}") } else { format!("http://{ip}:{port}") }
}

// ---- the transport ----

/// Cross-replica transport over pod-to-pod HTTP. Constructed in `main` when
/// `CCTUI_POD_IP` is set; local dev / single replicas keep [`super::NoopTransport`].
pub struct PeerHttpTransport {
    pool: PgPool,
    client: reqwest::Client,
    /// This pod's name — excluded from peer lookups so we never forward to
    /// ourselves.
    pod: String,
    /// The port every replica's HTTP server listens on (identical across the
    /// deployment, same assumption the forwarder made).
    port: u16,
    /// The cluster-internal shared secret (Bearer on every internal call).
    secret: String,
    /// Queue into the background relay worker.
    relay_tx: mpsc::UnboundedSender<WireBusEvent>,
}

impl PeerHttpTransport {
    pub fn new(
        pool: PgPool,
        client: reqwest::Client,
        pod: String,
        port: u16,
        secret: String,
    ) -> Self {
        let (relay_tx, relay_rx) = mpsc::unbounded_channel();
        tokio::spawn(relay_worker(
            pool.clone(),
            client.clone(),
            pod.clone(),
            port,
            secret.clone(),
            relay_rx,
        ));
        Self { pool, client, pod, port, secret, relay_tx }
    }

    /// The miss error for `kind` — what the caller would have seen locally.
    const fn miss(kind: Kind, target: Uuid) -> BusError {
        match kind {
            Kind::Daemon | Kind::Session => BusError::NoDaemon(target),
            Kind::Dispatcher => BusError::NoDispatcher(target),
        }
    }

    /// The peer owning a SESSION-scoped frame: the pod that announced the
    /// session, else the pod owning the machine. A machine id can be shared by
    /// several daemons, so its owner row alone would pick an arbitrary one.
    async fn session_owner_ip(&self, machine: Uuid, session: Option<&str>) -> Option<String> {
        if let Some(session) = session.and_then(|s| Uuid::parse_str(s).ok())
            && let Some(ip) =
                presence::peer_owner_ip(&self.pool, &self.pod, Kind::Session, session).await
        {
            return Some(ip);
        }
        presence::peer_owner_ip(&self.pool, &self.pod, Kind::Daemon, machine).await
    }

    /// Look up the live peer owning `target`'s WS and POST the route request to
    /// it. No live owner, an unreachable peer, or a moved connection all map to
    /// the honest miss error; peer-side delivery failures are decoded back into
    /// their [`BusError`] variants.
    async fn route(
        &self,
        kind: Kind,
        target: Uuid,
        request: &RouteRequest,
        timeout: std::time::Duration,
    ) -> Result<RouteResponse, BusError> {
        let owner = presence::peer_owner_ip(&self.pool, &self.pod, kind, target).await;
        self.route_to(owner, kind, target, request, timeout).await
    }

    /// [`Self::route`] against an already-resolved owner.
    async fn route_to(
        &self,
        owner: Option<String>,
        kind: Kind,
        target: Uuid,
        request: &RouteRequest,
        timeout: std::time::Duration,
    ) -> Result<RouteResponse, BusError> {
        let Some(owner) = owner else {
            return Err(Self::miss(kind, target));
        };
        let url = format!("{}/internal/bus/route", peer_base(&owner, self.port));
        let response = match self
            .client
            .post(&url)
            .bearer_auth(&self.secret)
            .timeout(timeout)
            .json(request)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(%err, %owner, %target, kind = kind.as_str(), "peer bus route failed");
                return Err(Self::miss(kind, target));
            }
        };
        if !response.status().is_success() {
            tracing::warn!(status = %response.status(), %owner, %target, "peer bus route rejected");
            return Err(Self::miss(kind, target));
        }
        match response.json::<RouteResponse>().await {
            Ok(RouteResponse::Err { code, message }) => Err(decode_error(code, message, target)),
            Ok(ok) => Ok(ok),
            Err(err) => {
                tracing::warn!(%err, %owner, "peer bus route response unreadable");
                Err(BusError::Transport(format!("peer response unreadable: {err}")))
            }
        }
    }
}

#[async_trait::async_trait]
impl Transport for PeerHttpTransport {
    async fn forward_daemon(&self, machine: Uuid, frame: DaemonFrameDown) -> Result<(), BusError> {
        let owner = self.session_owner_ip(machine, frame_session(&frame)).await;
        let request = RouteRequest::DaemonCommand { machine, frame };
        match self.route_to(owner, Kind::Daemon, machine, &request, COMMAND_TIMEOUT).await? {
            RouteResponse::Ok => Ok(()),
            other => Err(BusError::Transport(format!("unexpected peer reply: {other:?}"))),
        }
    }

    async fn forward_dispatcher(
        &self,
        dispatcher: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<(), BusError> {
        let request = RouteRequest::DispatcherCommand { dispatcher, frame };
        match self.route(Kind::Dispatcher, dispatcher, &request, COMMAND_TIMEOUT).await? {
            RouteResponse::Ok => Ok(()),
            other => Err(BusError::Transport(format!("unexpected peer reply: {other:?}"))),
        }
    }

    async fn request_daemon(
        &self,
        machine: Uuid,
        request: DaemonRequest,
    ) -> Result<DaemonResponse, BusError> {
        let session = match &request {
            DaemonRequest::StageFiles { local_id, .. }
            | DaemonRequest::Diagnose { local_id, .. } => Some(local_id.clone()),
            _ => None,
        };
        let owner = self.session_owner_ip(machine, session.as_deref()).await;
        let (request, timeout) = match request {
            DaemonRequest::StageFiles { adapter_id, local_id, uploads } => (
                RouteRequest::DaemonStageFiles { machine, adapter_id, local_id, uploads },
                STAGE_FORWARD_TIMEOUT,
            ),
            DaemonRequest::ListDirs { path } => {
                (RouteRequest::DaemonListDirs { machine, path }, LIST_DIRS_FORWARD_TIMEOUT)
            }
            DaemonRequest::GitInfo { path, include_dirty } => (
                RouteRequest::DaemonGitInfo { machine, path, include_dirty },
                LIST_DIRS_FORWARD_TIMEOUT,
            ),
            DaemonRequest::ReadFile { path, max_bytes, cwd } => (
                RouteRequest::DaemonReadFile { machine, path, max_bytes, cwd },
                READ_FILE_FORWARD_TIMEOUT,
            ),
            DaemonRequest::Diagnose { adapter_id, local_id } => (
                RouteRequest::DaemonDiagnose { machine, adapter_id, local_id },
                DIAGNOSE_FORWARD_TIMEOUT,
            ),
        };
        match self.route_to(owner, Kind::Daemon, machine, &request, timeout).await? {
            RouteResponse::StagedFiles { paths } => Ok(DaemonResponse::StagedFiles(paths)),
            RouteResponse::Dirs { dirs } => Ok(DaemonResponse::Dirs(dirs)),
            RouteResponse::GitInfo { info } => Ok(DaemonResponse::GitInfo(info)),
            RouteResponse::File { file } => Ok(DaemonResponse::File(file)),
            RouteResponse::Diagnose { report } => Ok(DaemonResponse::Diagnose(report)),
            other => Err(BusError::Transport(format!("unexpected peer reply: {other:?}"))),
        }
    }

    async fn request_dispatcher(
        &self,
        dispatcher: Uuid,
        request_id: Uuid,
        frame: DispatcherFrameDown,
    ) -> Result<DispatcherFrameUp, BusError> {
        let request = RouteRequest::DispatcherRequest { dispatcher, request_id, frame };
        match self.route(Kind::Dispatcher, dispatcher, &request, DISPATCHER_FORWARD_TIMEOUT).await?
        {
            RouteResponse::DispatcherReply { frame } => Ok(frame),
            other => Err(BusError::Transport(format!("unexpected peer reply: {other:?}"))),
        }
    }

    fn relay(&self, event: &BusEvent) {
        // Unbounded so `publish` stays sync + infallible for callers; the
        // worker drains in batches. Send only fails after worker death (process
        // teardown) — nothing to do then.
        let _ = self.relay_tx.send(WireBusEvent::from(event));
    }
}

/// Background fan-out: drain relayed events in batches and POST each batch to
/// every live peer pod. Best-effort — a failed or slow peer is warned about and
/// skipped; the DB remains the recovery path (clients refetch on resubscribe).
async fn relay_worker(
    pool: PgPool,
    client: reqwest::Client,
    pod: String,
    port: u16,
    secret: String,
    mut rx: mpsc::UnboundedReceiver<WireBusEvent>,
) {
    let mut peers: Vec<String> = Vec::new();
    let mut peers_fetched_at: Option<std::time::Instant> = None;
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < RELAY_BATCH {
            match rx.try_recv() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
        }
        // Refresh the peer list past its TTL. An empty list (single replica /
        // peers down) short-circuits: events are dropped here, delivered
        // locally already, persisted in the DB.
        if peers_fetched_at.is_none_or(|t| t.elapsed() > PEER_CACHE_TTL) {
            peers = presence::live_peer_pods(&pool, &pod).await;
            peers_fetched_at = Some(std::time::Instant::now());
        }
        if peers.is_empty() {
            continue;
        }
        let posts = peers.iter().map(|ip| {
            let url = format!("{}/internal/bus/publish", peer_base(ip, port));
            let fut = client
                .post(url)
                .bearer_auth(&secret)
                .timeout(RELAY_TIMEOUT)
                .json(&batch)
                .send();
            async move {
                match fut.await {
                    Ok(r) if r.status().is_success() => {}
                    Ok(r) => {
                        tracing::warn!(peer = %ip, status = %r.status(), "peer bus publish rejected");
                    }
                    Err(err) => tracing::warn!(peer = %ip, %err, "peer bus publish failed"),
                }
            }
        });
        futures_util::future::join_all(posts).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-replica forwarding keys on the session, so a machine-scoped
    /// frame and a session-scoped one must be told apart from the frame alone.
    #[test]
    fn frame_session_reads_the_target_of_session_scoped_frames() {
        let reply = DaemonFrameDown::Command {
            adapter_id: "claude-code".into(),
            command: Box::new(cctui_proto::adapter::AdapterCommand::Reply {
                local_id: "sess-1".into(),
                text: "hi".into(),
                ask_picks: None,
                env: std::collections::BTreeMap::new(),
                command_id: None,
                turn_id: None,
            }),
        };
        assert_eq!(frame_session(&reply), Some("sess-1"));

        let reconcile = DaemonFrameDown::Reconcile {
            adapters: Vec::new(),
            secret_scrub: cctui_proto::ws::SecretScrubConfig::default(),
        };
        assert_eq!(frame_session(&reconcile), None);

        let spawn = DaemonFrameDown::Command {
            adapter_id: "claude-code".into(),
            command: Box::new(cctui_proto::adapter::AdapterCommand::ResumeMarks {
                marks: Vec::new(),
            }),
        };
        assert_eq!(frame_session(&spawn), None, "adapter-wide commands stay machine-scoped");
    }

    #[test]
    fn error_codes_round_trip_meaningfully() {
        let machine = Uuid::new_v4();
        let cases: Vec<BusError> = vec![
            BusError::NoDaemon(machine),
            BusError::Closed,
            BusError::Disconnected,
            BusError::Timeout,
            BusError::Staging("disk full".into()),
            BusError::ListDirs("no such directory".into()),
        ];
        for err in cases {
            let (code, message) = encode_error(&err);
            let back = decode_error(code, message, machine);
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&back),
                "{err:?} -> {back:?}"
            );
            assert_eq!(err.to_string(), back.to_string());
        }
    }

    #[test]
    fn dispatcher_miss_decodes_with_target() {
        let dispatcher = Uuid::new_v4();
        let (code, message) = encode_error(&BusError::NoDispatcher(dispatcher));
        let back = decode_error(code, message, dispatcher);
        assert!(matches!(back, BusError::NoDispatcher(d) if d == dispatcher));
    }

    #[test]
    fn unclassified_errors_collapse_to_transport() {
        let (code, message) = encode_error(&BusError::Reconcile("boom".into()));
        assert_eq!(code, WireErrorCode::Other);
        let back = decode_error(code, message, Uuid::new_v4());
        assert!(matches!(back, BusError::Transport(m) if m.contains("boom")));
    }

    #[test]
    fn wire_bus_event_round_trips() {
        let ev = BusEvent::Server(ServerEvent::AskResolved { session_id: "sess-1".into() });
        let wire = WireBusEvent::from(&ev);
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains(r#""kind":"server""#));
        let back: WireBusEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            BusEvent::from(back),
            BusEvent::Server(ServerEvent::AskResolved { session_id }) if session_id == "sess-1"
        ));

        let ev = BusEvent::Session {
            session_id: "sess-2".into(),
            event: AgentEvent::TurnEnd { ts: 7, seq: None },
        };
        let wire = WireBusEvent::from(&ev);
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains(r#""kind":"session""#));
        let back: WireBusEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            BusEvent::from(back),
            BusEvent::Session { session_id, event: AgentEvent::TurnEnd { ts: 7, .. } }
                if session_id == "sess-2"
        ));
    }

    #[test]
    fn peer_base_brackets_ipv6() {
        assert_eq!(peer_base("10.0.0.1", 8700), "http://10.0.0.1:8700");
        assert_eq!(peer_base("fd00::1", 8700), "http://[fd00::1]:8700");
    }

    #[test]
    fn route_request_serde_shape() {
        let req = RouteRequest::DaemonListDirs { machine: Uuid::nil(), path: "/home".into() };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"daemon_list_dirs""#));
        let _back: RouteRequest = serde_json::from_str(&json).unwrap();

        let resp = RouteResponse::Err {
            code: WireErrorCode::NoDaemon,
            message: "no daemon connected".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""outcome":"err""#));
        assert!(json.contains(r#""code":"no_daemon""#));
        let _back: RouteResponse = serde_json::from_str(&json).unwrap();
    }
}
