//! Daemon supervisor.
//!
//! Owns the WS connection, the per-adapter channels, and the reconnect
//! loop. Reconnect backoff: 5s → 10s → 20s → 60s (capped). On every
//! successful (re)connect we honour the freshest `Reconcile` from the
//! server — adapters not in the new manifest are shut down; new adapters
//! are spawned.

use std::collections::HashMap;
use std::time::Duration;

use cctui_crypto::redact::{self, CompiledPatterns};
use cctui_proto::adapter::{AdapterCommand, AdapterEvent};
use cctui_proto::api::DaemonAdapterConfig;
use cctui_proto::ws::{DaemonFrameDown, DaemonFrameUp, SecretScrubConfig};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::adapter_runtime::AdapterFactory;
use crate::bus::build_ctx;
use crate::client::ServerClient;
use crate::counters::{BandwidthCounters, Subsystem};
use crate::sendguard::{MAX_ATTEMPTS, MAX_PAYLOAD_BYTES, SendGuard};

/// Backoff schedule. Capped at the last entry on subsequent failures.
const BACKOFF_SECS: &[u64] = &[5, 10, 20, 60];
/// Gap before the first retry of a refused job removal. It doubles on
/// every further refusal (see [`purge_gap`]).
const PURGE_RETRY: Duration = Duration::from_mins(10);
/// Ceiling of the doubling gap. A removal that can never succeed (an
/// `EACCES` on a root-owned file, a worktree that is the cwd of a live
/// session) is still retried, once a day, instead of every ten minutes
/// forever: a 14-job backlog at ten minutes is ~2000 log lines a day.
const PURGE_RETRY_MAX: Duration = Duration::from_hours(24);

/// WS keepalive ping interval. Must be shorter than any idle timeout on
/// the path (ingress, NAT, load balancer). 20s is comfortably below the
/// typical 60s defaults.
const PING_INTERVAL: Duration = Duration::from_secs(20);

/// If no frame (incl. the server's auto-Pong to our Ping) arrives within
/// this window, the connection is treated as half-open and torn down so the
/// reconnect loop re-establishes it. ~3 missed pings. Without this the
/// daemon can sit forever on a dead TCP socket (`sink.send` buffers into the
/// kernel without erroring, `stream.next` blocks) and the web UI reports
/// "daemon offline" until a manual restart.
const LIVENESS_TIMEOUT: Duration = Duration::from_mins(1);

/// Micro-batch window: adapter events queued within this window are
/// coalesced into one frame before compress+chunk, so cross-event redundancy
/// compresses far better. Heartbeats and control frames bypass it.
const BATCH_WINDOW: Duration = Duration::from_millis(250);

/// Flush the batch early once the buffered (uncompressed) bytes reach this, so a
/// burst doesn't grow the pre-compression buffer unbounded.
const BATCH_MAX_BYTES: usize = 1024 * 1024;

/// On shutdown, keep draining adapter events this long past the last one so an
/// in-flight final tail (the driver's teardown flush) still reaches the wire
/// before the WS closes. Hard-capped by [`SHUTDOWN_DRAIN_MAX`].
const SHUTDOWN_DRAIN_QUIET: Duration = Duration::from_millis(500);

/// Absolute ceiling on the shutdown drain so a busy adapter can't hold teardown
/// open indefinitely.
const SHUTDOWN_DRAIN_MAX: Duration = Duration::from_secs(3);

/// Ceiling on how long a cancelled adapter may take to exit before the next
/// connection rebuilds it anyway.
const ADAPTER_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect-edge broadcast depth. An adapter that lags past this sees `Lagged`,
/// which is still an edge — the signal carries no payload.
const CONNECT_SIGNAL_BUFFER: usize = 8;

/// Sleep until `deadline`, or never when there's nothing buffered to flush.
async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

pub struct Supervisor {
    client: ServerClient,
    machine_key: String,
    factories: Vec<Box<dyn AdapterFactory>>,
    /// A chunked transfer interrupted by a disconnect, kept across reconnects so
    /// the next connection resumes from the last acked chunk rather than byte
    /// zero.
    pending_transfer: std::sync::Mutex<Option<PendingTransfer>>,
    /// Per-content-hash give-up tracker: size/attempts caps + tombstones so a
    /// poison transfer can't wedge the pipeline.
    guard: std::sync::Mutex<SendGuard>,
    /// Per-subsystem byte counters, shared with the HTTP client.
    counters: BandwidthCounters,
    /// Host CPU / memory / disk sampler for the heartbeat's `resources` block.
    resources: std::sync::Mutex<crate::resources::ResourceSampler>,
    /// Broadcast of connect edges; every adapter ctx holds a subscription.
    connected: tokio::sync::broadcast::Sender<()>,
    /// WS keepalive cadence; [`PING_INTERVAL`] outside tests.
    ping_interval: Duration,
    /// The in-flight purge of leaked claude jobs (see `purge_leaked_jobs`),
    /// aborted and restarted whenever a fresh archived list arrives.
    purge: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
    /// When each session's job removal was last queued and how many times
    /// it was found still on disk afterwards, so a refused `claude rm` is
    /// retried on a doubling gap (see [`purge_gap`]), not every heartbeat.
    purge_attempts: std::sync::Mutex<HashMap<String, PurgeAttempt>>,
}

/// One archived session's removal history, kept while its job dir is
/// still on disk and dropped as soon as it is gone.
#[derive(Debug, Clone, Copy)]
struct PurgeAttempt {
    /// When the last `Remove` was queued.
    at: tokio::time::Instant,
    /// How many earlier attempts left the job dir in place.
    failures: u32,
}

/// Gap to wait after `failures` unsuccessful attempts: [`PURGE_RETRY`]
/// doubled per failure, capped at [`PURGE_RETRY_MAX`].
fn purge_gap(failures: u32) -> Duration {
    let doubled = PURGE_RETRY.saturating_mul(1u32 << failures.min(16));
    doubled.min(PURGE_RETRY_MAX)
}

/// Split `ids` (the archived sessions whose job dir is still on disk) into
/// the ones due for a removal attempt now, recording that attempt. Ids no
/// longer listed have been removed: their history is forgotten, so a job
/// that leaks again starts from the first gap.
fn purge_due(
    attempts: &mut HashMap<String, PurgeAttempt>,
    ids: Vec<String>,
    now: tokio::time::Instant,
) -> Vec<String> {
    attempts.retain(|id, _| ids.contains(id));
    let mut due = Vec::new();
    for id in ids {
        match attempts.get_mut(&id) {
            None => {
                attempts.insert(id.clone(), PurgeAttempt { at: now, failures: 0 });
                due.push(id);
            }
            Some(attempt) if now.duration_since(attempt.at) >= purge_gap(attempt.failures) => {
                attempt.failures += 1;
                attempt.at = now;
                tracing::info!(
                    id = %id,
                    failures = attempt.failures,
                    next_gap_secs = purge_gap(attempt.failures).as_secs(),
                    "claude job still on disk after removal; retrying"
                );
                due.push(id);
            }
            Some(_) => {}
        }
    }
    due
}

impl Supervisor {
    #[must_use]
    pub fn new(
        client: ServerClient,
        machine_key: String,
        factories: Vec<Box<dyn AdapterFactory>>,
    ) -> Self {
        let counters = client.counters();
        Self {
            client,
            machine_key,
            factories,
            pending_transfer: std::sync::Mutex::new(None),
            guard: std::sync::Mutex::new(SendGuard::open_default()),
            counters,
            resources: std::sync::Mutex::new(crate::resources::ResourceSampler::new()),
            connected: tokio::sync::broadcast::Sender::new(CONNECT_SIGNAL_BUFFER),
            ping_interval: PING_INTERVAL,
            purge: std::sync::Mutex::new(None),
            purge_attempts: std::sync::Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    const fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }

    /// Run the connect/reconnect loop until `shutdown` fires.
    pub async fn run(self, shutdown: CancellationToken) {
        // The `CctuiAgent` tool socket outlives individual WS connections: a
        // session's tool call must not fail just because the daemon is between
        // reconnects.
        if crate::agenttool::is_available(&self.machine_key) {
            let path = crate::agenttool::socket_path();
            let client = self.client.clone();
            let machine_key = self.machine_key.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                if let Err(err) = crate::agenttool::serve(path, client, machine_key, shutdown).await
                {
                    tracing::warn!(%err, "CctuiAgent tool listener exited");
                }
            });
        }
        crate::configsweep::spawn_loop(shutdown.clone());

        // Adapters live for the life of the process, not of a connection: their
        // tokens own live session subprocesses (`opencode serve`, `codex
        // app-server`, headless claude), so tearing them down on a WS drop would
        // kill running work. The WS is only the transport.
        let mut running: HashMap<String, AdapterRunning> = HashMap::new();
        let (event_tx, mut event_rx) = mpsc::channel::<(String, AdapterEvent)>(256);

        let mut attempt = 0usize;
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            match self.run_once(shutdown.clone(), &mut running, &event_tx, &mut event_rx).await {
                Ok(()) => {
                    tracing::info!("daemon WS closed cleanly, reconnecting");
                    attempt = 0;
                }
                Err(err) => {
                    let delay = BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)];
                    tracing::warn!(%err, attempt, "daemon connection failed; retry in {delay}s");
                    attempt = attempt.saturating_add(1);
                    tokio::select! {
                        () = tokio::time::sleep(Duration::from_secs(delay)) => {}
                        () = shutdown.cancelled() => break,
                    }
                }
            }
        }

        stop_adapters(&mut running).await;
    }

    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn run_once(
        &self,
        shutdown: CancellationToken,
        running: &mut HashMap<String, AdapterRunning>,
        event_tx: &mpsc::Sender<(String, AdapterEvent)>,
        event_rx: &mut mpsc::Receiver<(String, AdapterEvent)>,
    ) -> anyhow::Result<()> {
        let url = self.client.daemon_ws_url();
        tracing::info!(%url, "connecting to daemon WS");
        let request = crate::client::daemon_ws_request(&url, &self.machine_key)?;
        let (ws, _) = tokio_tungstenite::connect_async(request).await?;
        let (mut sink, mut stream) = ws.split();

        // Out-of-band frames the supervisor itself produces (currently the
        // `StageFilesResult` reply to a mid-chat attachment request),
        // fanned onto the same WS sink as adapter events.
        let (frame_up_tx, mut frame_up_rx) = mpsc::channel::<DaemonFrameUp>(64);

        let mut scrub = CompiledPatterns::disabled();

        let mut announced = false;

        let mut ping = tokio::time::interval(self.ping_interval);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Discard the immediate first tick — we just connected.
        ping.tick().await;

        // Last time we heard *anything* from the server (text frame or the
        // auto-Pong to our Ping). Drives half-open detection on ping ticks.
        let mut last_rx = tokio::time::Instant::now();

        // Resume an interrupted transfer from its last acked chunk.
        let mut active: Option<PendingTransfer> = self.pending_transfer.lock().unwrap().take();
        if let Some(t) = active.as_mut() {
            t.rewind_to_ack();
        }

        // Micro-batch buffer: adapter events accumulate here for up to
        // BATCH_WINDOW, then flush as one compress+chunk frame.
        let mut batch: Vec<DaemonFrameUp> = Vec::new();
        let mut batch_bytes = 0usize;
        let mut batch_deadline: Option<tokio::time::Instant> = None;

        let outcome: anyhow::Result<()> = async {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => {
                        let frames = drain_for_shutdown(
                            std::mem::take(&mut batch),
                            event_rx,
                            &scrub,
                        )
                        .await;
                        if !frames.is_empty()
                            && let Ok(Prepared::Frame(text)) = prepare_send(&coalesce(frames))
                        {
                            self.counters.add(Subsystem::Forward, text.len() as u64);
                            let _ = sink.send(Message::Text(text.into())).await;
                        }
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(());
                    }
                    msg = stream.next() => {
                        let Some(msg) = msg else { return Ok(()); };
                        let msg = msg?;
                        last_rx = tokio::time::Instant::now();
                        if let Some(frame) = parse_frame(msg)? {
                            if let DaemonFrameDown::ChunkAck { transfer_id, highest_contiguous_chunk } = &frame {
                                if let Some(t) = active.as_mut()
                                    && t.id == *transfer_id {
                                        t.record_ack(*highest_contiguous_chunk);
                                        if t.is_complete() {
                                            {
                                                let mut guard = self.guard.lock().unwrap();
                                                guard.complete(&t.id);
                                                guard.flush();
                                            }
                                            active = None;
                                        }
                                    }
                            } else {
                                let reconciled =
                                    matches!(frame, DaemonFrameDown::Reconcile { .. });
                                self.handle_frame(frame, running, event_tx, &frame_up_tx, &mut scrub, &shutdown).await;
                                // Announce the edge once the connection's first
                                // reconcile has built (and subscribed) the adapters.
                                if reconciled && !announced {
                                    announced = true;
                                    let _ = self.connected.send(());
                                }
                            }
                        }
                    }
                    () = std::future::ready(()), if active.as_ref().is_some_and(PendingTransfer::has_unsent) => {
                        if let Some(t) = active.as_mut() {
                            let (frame, retransmit) = t.next_frame();
                            let payload = serde_json::to_string(&frame)?;
                            let subsystem =
                                if retransmit { Subsystem::Retransmit } else { Subsystem::Forward };
                            self.counters.add(subsystem, payload.len() as u64);
                            sink.send(Message::Text(payload.into())).await?;
                        }
                    }
                    Some(frame) = frame_up_rx.recv() => {
                        let payload = serde_json::to_string(&frame)?;
                        sink.send(Message::Text(payload.into())).await?;
                    }
                    // Pause new events while a chunked transfer is in flight so a
                    // single WS carries one large transfer at a time.
                    Some((adapter_id, event)) = event_rx.recv(), if active.is_none() => {
                        tracing::debug!(
                            %adapter_id,
                            kind = event_kind(&event),
                            local_id = event_local_id(&event),
                            "queued event",
                        );
                        // A `CctuiAgent` caller is parked on its child's
                        // completion; this is the one point every adapter's
                        // events pass through.
                        crate::childwatch::global().observe(&event);
                        // Must stay after `observe`: the turn-tail signal needs
                        // the event even though it has nothing to persist.
                        if is_textless_thinking(&event) {
                            continue;
                        }
                        // Redact secrets before the event reaches the wire / DB.
                        let event = scrub_event(event, &scrub);
                        let up = DaemonFrameUp::Event { adapter_id, event };
                        batch_bytes = batch_bytes.saturating_add(frame_size(&up));
                        batch.push(up);
                        batch_deadline.get_or_insert_with(|| {
                            tokio::time::Instant::now() + BATCH_WINDOW
                        });
                        if batch_bytes >= BATCH_MAX_BYTES {
                            batch_deadline = Some(tokio::time::Instant::now());
                        }
                    }
                    // Flush the coalesced batch once its window elapses.
                    () = wait_deadline(batch_deadline), if active.is_none() => {
                        batch_deadline = None;
                        batch_bytes = 0;
                        let frames = std::mem::take(&mut batch);
                        if !frames.is_empty() {
                            match prepare_send(&coalesce(frames))? {
                                Prepared::Chunked(t) => {
                                    if self.guard.lock().unwrap().is_tombstoned(&t.id) {
                                        tracing::warn!(
                                            transfer_id = %t.id,
                                            sessions = ?t.session_ids(),
                                            "give-up: skipping tombstoned poison transfer",
                                        );
                                    } else {
                                        active = Some(t);
                                    }
                                }
                                Prepared::Frame(text) => {
                                    self.counters.add(Subsystem::Forward, text.len() as u64);
                                    sink.send(Message::Text(text.into())).await?;
                                }
                                Prepared::Oversized(len) => {
                                    tracing::warn!(
                                        bytes = len,
                                        cap = MAX_PAYLOAD_BYTES,
                                        "give-up: dropping payload over the size cap unsent",
                                    );
                                }
                            }
                        }
                    }
                    _ = ping.tick() => {
                        // Detect a half-open connection: if the server hasn't sent
                        // anything (not even a Pong) within LIVENESS_TIMEOUT, tear
                        // down so the reconnect loop takes over.
                        if last_rx.elapsed() > LIVENESS_TIMEOUT {
                            anyhow::bail!(
                                "no server traffic for {}s — WS half-open, reconnecting",
                                last_rx.elapsed().as_secs()
                            );
                        }
                        sink.send(Message::Ping(Vec::new().into())).await?;
                        // App-level liveness heartbeat. The WS Ping above
                        // keeps the socket warm, but the server only advances
                        // `machines.last_seen_at` on an application frame; this
                        // Heartbeat gives it a per-cadence signal to derive the
                        // machine online/stale/offline tier from.
                        let hb = DaemonFrameUp::Heartbeat {
                            sent_at: chrono::Utc::now(),
                            bandwidth: Some(self.counters.summary()),
                            // Re-read per heartbeat: an operator who configures
                            // the hook and restarts the daemon is picked up on
                            // the next ping, with nothing to do server-side.
                            update_hook: Some(crate::updatehook::configured()),
                            // Sampled per heartbeat; `None` until the second
                            // ping (CPU needs an interval) or off-Linux.
                            resources: self
                                .resources
                                .lock()
                                .map_or(None, |mut s| s.sample()),
                            claude_jobs: claude_jobs_root(running).map(|root| jobs_on_disk(&root)),
                        };
                        let payload = serde_json::to_string(&hb)?;
                        self.counters.add(Subsystem::Heartbeat, payload.len() as u64);
                        sink.send(Message::Text(payload.into())).await?;
                        self.counters.persist();
                    }
                }
            }
        }
        .await;

        // An unfinished transfer resumes next connection, unless it has burned
        // MAX_ATTEMPTS without progress: then tombstone + drop it.
        if let Some(t) = active
            && !t.is_complete()
        {
            let progress = u64::from(t.resume_index());
            let mut guard = self.guard.lock().unwrap();
            let give_up = guard.note_failed_attempt(&t.id, progress);
            guard.flush();
            drop(guard);
            if give_up {
                tracing::warn!(
                    transfer_id = %t.id,
                    attempts = MAX_ATTEMPTS,
                    sessions = ?t.session_ids(),
                    "give-up: transfer never acked; tombstoned and dropped, pipeline advances",
                );
            } else {
                *self.pending_transfer.lock().unwrap() = Some(t);
            }
        }
        outcome
    }

    // Dispatch over every `DaemonFrameDown` variant (reconcile / spawn / command /
    // …); complexity is the breadth of the match arms, not nesting. Per-arm helpers
    // would be churn and obscure the frame-handling overview.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn handle_frame(
        &self,
        frame: DaemonFrameDown,
        running: &mut HashMap<String, AdapterRunning>,
        event_tx: &mpsc::Sender<(String, AdapterEvent)>,
        frame_up_tx: &mpsc::Sender<DaemonFrameUp>,
        scrub: &mut CompiledPatterns,
        shutdown: &CancellationToken,
    ) {
        match frame {
            DaemonFrameDown::Reconcile { adapters, secret_scrub } => {
                *scrub = compile_scrub(&secret_scrub);
                self.reconcile(adapters, running, event_tx, shutdown);
            }
            DaemonFrameDown::Command { adapter_id, command } => {
                // `try_send`, never `send().await`: this runs inside the
                // transport `select!`, alongside the keepalive ping and the
                // socket read. Awaiting a full adapter channel here stalls
                // both, and the server evicts the daemon as dead.
                #[allow(clippy::option_if_let_else)] // `command` moves into one arm only
                let outcome = match running.get(&adapter_id) {
                    None => Err(("is not running on this machine", *command)),
                    Some(running) => {
                        running.commands_tx.try_send(*command).map_err(|err| match err {
                            mpsc::error::TrySendError::Full(command) => {
                                ("command queue is full", command)
                            }
                            mpsc::error::TrySendError::Closed(command) => {
                                ("is shutting down", command)
                            }
                        })
                    }
                };
                let error = outcome.err().and_then(|(reason, command)| {
                    tracing::warn!(%adapter_id, reason, "rejecting command");
                    command.command_id().map(|id| (id, format!("adapter {adapter_id} {reason}")))
                });
                // Silent drop would leave the server-side waiter hanging.
                // Best-effort for the same reason as above: the event channel
                // is drained by this very loop.
                if let Some((command_id, error)) = error {
                    let _ = event_tx.try_send((
                        adapter_id,
                        AdapterEvent::CommandResult { command_id, ok: false, error: Some(error) },
                    ));
                }
            }
            DaemonFrameDown::ResumeMarks { session_marks, archived } => {
                crate::configsweep::note_server_sessions(session_marks.iter().map(|(id, _)| id));
                // Fan the marks to every running adapter; each clamps the
                // sessions it owns and ignores ids it doesn't know. Best-effort
                // (`try_send`): the next connect resends them, and blocking the
                // transport loop on a busy adapter is never acceptable.
                for (adapter_id, running) in running.iter() {
                    if running
                        .commands_tx
                        .try_send(AdapterCommand::ResumeMarks { marks: session_marks.clone() })
                        .is_err()
                    {
                        tracing::warn!(%adapter_id, "adapter busy; resume marks dropped until next connect");
                    }
                }
                self.purge_archived_jobs(running, &archived);
            }
            DaemonFrameDown::ArchivedJobs { session_ids } => {
                self.purge_archived_jobs(running, &session_ids);
            }
            DaemonFrameDown::StageFiles { request_id, adapter_id, local_id, uploads } => {
                let up = stage_files_result(request_id, &adapter_id, &local_id, &uploads);
                if frame_up_tx.send(up).await.is_err() {
                    tracing::warn!("frame_up channel closed; dropping StageFilesResult");
                }
            }
            DaemonFrameDown::ListDirs { request_id, path } => {
                let up = match crate::listdirs::list_dirs(&path) {
                    Ok(dirs) => {
                        DaemonFrameUp::ListDirsResult { request_id, ok: true, dirs, error: None }
                    }
                    Err(err) => DaemonFrameUp::ListDirsResult {
                        request_id,
                        ok: false,
                        dirs: Vec::new(),
                        error: Some(err.to_string()),
                    },
                };
                if frame_up_tx.send(up).await.is_err() {
                    tracing::warn!("frame_up channel closed; dropping ListDirsResult");
                }
            }
            DaemonFrameDown::GitInfo { request_id, path, include_dirty } => {
                let roots = crate::git::default_roots();
                let up = match crate::git::resolve_git_info(&path, &roots, include_dirty).await {
                    Ok(info) => DaemonFrameUp::GitInfoResult {
                        request_id,
                        ok: true,
                        info: Some(info),
                        error: None,
                    },
                    Err(err) => DaemonFrameUp::GitInfoResult {
                        request_id,
                        ok: false,
                        info: None,
                        error: Some(err.to_string()),
                    },
                };
                if frame_up_tx.send(up).await.is_err() {
                    tracing::warn!("frame_up channel closed; dropping GitInfoResult");
                }
            }
            DaemonFrameDown::ReadFile { request_id, path, max_bytes, cwd } => {
                self.spawn_read_file(request_id, path, max_bytes, cwd, frame_up_tx.clone());
            }
            DaemonFrameDown::RunUpdateHook { run_id, version, release_url } => {
                self.spawn_update_hook(run_id, version, release_url);
            }
            _ => {}
        }
    }

    /// Run the deployment's update hook off the frame loop.
    ///
    /// Deliberately not a correlated request: the hook restarts the server that
    /// sent the frame, so there is no reply to send back — progress goes over
    /// HTTP instead, to whichever server process is alive by then.
    fn spawn_update_hook(&self, run_id: uuid::Uuid, version: String, release_url: String) {
        crate::updatehook::spawn(
            self.client.http(),
            self.client.base_url().to_owned(),
            self.machine_key.clone(),
            run_id,
            version,
            release_url,
        );
    }

    /// A file read may PUT up to 32 MiB to the blob store, so it runs off the
    /// frame loop.
    fn spawn_read_file(
        &self,
        request_id: uuid::Uuid,
        path: String,
        max_bytes: u64,
        cwd: Option<String>,
        frame_up_tx: mpsc::Sender<DaemonFrameUp>,
    ) {
        let client = self.client.clone();
        let machine_key = self.machine_key.clone();
        tokio::spawn(async move {
            let up = crate::readfile::handle(
                &client,
                &machine_key,
                request_id,
                &path,
                max_bytes,
                cwd.as_deref(),
            )
            .await;
            if frame_up_tx.send(up).await.is_err() {
                tracing::warn!("frame_up channel closed; dropping ReadFileResult");
            }
        });
    }

    #[allow(clippy::cognitive_complexity)]
    fn reconcile(
        &self,
        adapters: Vec<DaemonAdapterConfig>,
        running: &mut HashMap<String, AdapterRunning>,
        event_tx: &mpsc::Sender<(String, AdapterEvent)>,
        shutdown: &CancellationToken,
    ) {
        let mut want: HashMap<String, DaemonAdapterConfig> =
            adapters.into_iter().map(|a| (a.adapter_id.to_string(), a)).collect();

        // Allow-list roots for agent-posted image markers, resolved
        // once per reconcile and shared by every adapter's event pump below.
        let image_roots = crate::imagepost::default_allowed_roots();

        // Stop adapters no longer in the manifest or disabled.
        let to_stop: Vec<String> = running
            .keys()
            .filter(|id| want.get(*id).is_none_or(|cfg| !cfg.enabled))
            .cloned()
            .collect();
        for id in to_stop {
            if let Some(r) = running.remove(&id) {
                r.shutdown.cancel();
                tracing::info!(adapter_id = %id, "stopped adapter");
            }
        }

        // Start adapters that should be running but aren't, and rebuild
        // adapters whose config changed since they were last (re)built —
        // without this a live mode switch (bg→sdk) is silently ignored until
        // the adapter is stopped or the daemon restarts.
        for (id, cfg) in want.drain() {
            if !cfg.enabled {
                continue;
            }
            if let Some(existing) = running.get(&id) {
                if existing.config == cfg.config {
                    // Unchanged — leave it running, don't churn it.
                    continue;
                }
                // Config changed: tear the old instance down and rebuild it
                // with the new config below. Cancel its token so the old
                // adapter task + event pump exit cleanly before the new
                // instance is spawned.
                if let Some(old) = running.remove(&id) {
                    old.shutdown.cancel();
                    // The old command sink is dropped here. Any command frame
                    // still queued in it (server → daemon → old adapter) but
                    // not yet consumed by the now-cancelled adapter is lost;
                    // surface that the way the unknown-adapter path does so a
                    // dropped command isn't silent.
                    let queued = old.commands_tx.max_capacity() - old.commands_tx.capacity();
                    if queued > 0 {
                        tracing::warn!(
                            adapter_id = %id,
                            queued,
                            "dropping in-flight commands for rebuilt adapter",
                        );
                    }
                    tracing::info!(adapter_id = %id, "config changed; rebuilding adapter");
                }
            }
            let Some(factory) = self.factories.iter().find(|f| f.id() == id) else {
                tracing::warn!(adapter_id = %id, "no factory compiled in for adapter");
                continue;
            };
            let token = shutdown.child_token();
            // Hand the adapter an authenticated server client + machine key so
            // its launch chokepoint can pull per-session gateway env.
            let (ctx, channels) = build_ctx(
                cfg.config.clone(),
                token.clone(),
                Some(self.client.clone()),
                Some(self.machine_key.clone()),
                &self.connected,
            );
            let adapter = factory.build(cfg.config.clone());
            let adapter_id_for_pump = id.clone();
            let event_tx = event_tx.clone();
            let mut events_rx = channels.events_rx;
            // Pump per-adapter events into the shared event_tx with the adapter
            // id attached. Assistant messages pass through the image-marker
            // rewrite here — per-adapter task, so an upload can't stall
            // the WS loop; a non-marker message returns unchanged.
            let img_client = self.client.clone();
            let img_key = self.machine_key.clone();
            let img_roots = image_roots.clone();
            let pump = tokio::spawn(async move {
                while let Some(evt) = events_rx.recv().await {
                    let evt =
                        crate::imagepost::process_event(&img_client, &img_key, evt, &img_roots)
                            .await;
                    if event_tx.send((adapter_id_for_pump.clone(), evt)).await.is_err() {
                        break;
                    }
                }
            });
            let id_for_task = id.clone();
            let driver = tokio::spawn(async move {
                if let Err(err) = adapter.start(ctx).await {
                    tracing::error!(adapter_id = %id_for_task, %err, "adapter exited with error");
                }
            });
            running.insert(
                id.clone(),
                AdapterRunning {
                    shutdown: token,
                    // Remember the config this instance was built from so the
                    // next reconcile can detect a change.
                    config: cfg.config,
                    commands_tx: channels.commands_tx,
                    tasks: vec![pump, driver],
                },
            );
            tracing::info!(adapter_id = %id, "started adapter");
        }
    }
}

/// A large serialized up-frame being sent as ordered chunks, with
/// enough state to resume after a disconnect: the content-hash id, the highest
/// chunk the server has acked, and the next chunk to hand this connection.
struct PendingTransfer {
    id: String,
    payload: Vec<u8>,
    total: u32,
    highest_acked: Option<u32>,
    cursor: u32,
    /// Codec the chunk bytes are compressed with, so the server decompresses the
    /// reassembled payload before parsing it. `None` = raw JSON.
    codec: Option<String>,
    /// Highest chunk index ever emitted, so a re-sent chunk after a resume is
    /// billed as a retransmit rather than forward.
    sent_high_water: Option<u32>,
}

impl PendingTransfer {
    /// Build a transfer for `payload` (already codec-compressed when `codec` is
    /// set), or `None` when it fits the single-message fast path.
    fn new(payload: Vec<u8>, codec: Option<String>) -> Option<Self> {
        if payload.len() <= cctui_proto::chunk::CHUNK_THRESHOLD {
            return None;
        }
        let id = cctui_proto::chunk::transfer_id(&payload);
        let total = cctui_proto::chunk::chunk_count(payload.len());
        Some(Self {
            id,
            payload,
            total,
            highest_acked: None,
            cursor: 0,
            codec,
            sent_high_water: None,
        })
    }

    fn resume_index(&self) -> u32 {
        self.highest_acked.map_or(0, |h| h.saturating_add(1))
    }

    fn rewind_to_ack(&mut self) {
        self.cursor = self.resume_index();
    }

    const fn has_unsent(&self) -> bool {
        self.cursor < self.total
    }

    /// The next chunk frame, paired with whether it re-sends an already-emitted
    /// index (a retransmit) versus advancing new ground (forward) —.
    fn next_frame(&mut self) -> (DaemonFrameUp, bool) {
        let idx = self.cursor;
        let retransmit = self.sent_high_water.is_some_and(|hw| idx <= hw);
        let frame = cctui_proto::chunk::chunk_frame(
            &self.id,
            &self.payload,
            idx,
            self.total,
            self.codec.as_deref(),
        );
        self.cursor = self.cursor.saturating_add(1);
        if self.sent_high_water.is_none_or(|hw| idx > hw) {
            self.sent_high_water = Some(idx);
        }
        (frame, retransmit)
    }

    fn record_ack(&mut self, highest_contiguous: Option<u32>) {
        if let Some(h) = highest_contiguous
            && self.highest_acked.is_none_or(|cur| h > cur)
        {
            self.highest_acked = Some(h);
        }
    }

    fn is_complete(&self) -> bool {
        self.highest_acked == self.total.checked_sub(1)
    }

    /// Best-effort list of the session ids carried by this transfer, for the
    /// give-up diagnostic when it is tombstoned.
    fn session_ids(&self) -> Vec<String> {
        let raw = self.codec.as_deref().map_or_else(
            || Some(self.payload.clone()),
            |codec| cctui_proto::compress::decompress_codec(codec, &self.payload).ok(),
        );
        raw.and_then(|r| serde_json::from_slice::<DaemonFrameUp>(&r).ok())
            .map(|frame| frame_session_ids(&frame))
            .unwrap_or_default()
    }
}

/// Collect the distinct session local ids referenced by a wire frame, unwrapping
/// a `Batch` into its members.
fn frame_session_ids(frame: &DaemonFrameUp) -> Vec<String> {
    match frame {
        DaemonFrameUp::Event { event, .. } => {
            let id = event_local_id(event);
            if id.is_empty() { vec![] } else { vec![id.to_owned()] }
        }
        DaemonFrameUp::Batch { frames } => {
            let mut ids: Vec<String> = frames.iter().flat_map(frame_session_ids).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        }
        _ => vec![],
    }
}

struct AdapterRunning {
    shutdown: CancellationToken,
    /// The adapter config this instance was built from. A reconcile compares
    /// the new manifest's config against this to decide rebuild-vs-leave-alone.
    config: serde_json::Value,
    /// Command sink the supervisor routes server `Command` frames into.
    commands_tx: mpsc::Sender<cctui_proto::adapter::AdapterCommand>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl AdapterRunning {
    /// Cancel and wait for the tasks to observe it: a replacement must not
    /// overlap its predecessor, two codex `LogTail`s sharing one
    /// `codex-offsets.json` corrupt the offsets.
    async fn stop(mut self) {
        self.shutdown.cancel();
        let tasks = std::mem::take(&mut self.tasks);
        if tokio::time::timeout(ADAPTER_STOP_TIMEOUT, futures_util::future::join_all(tasks))
            .await
            .is_err()
        {
            tracing::warn!("adapter did not stop within {ADAPTER_STOP_TIMEOUT:?}");
        }
    }
}

impl Supervisor {
    /// Queue a `Remove` for every archived session whose claude job is still
    /// on disk and was not already attempted within [`PURGE_RETRY`].
    fn purge_archived_jobs(&self, running: &HashMap<String, AdapterRunning>, archived: &[String]) {
        let Some(claude) = running.get(CLAUDE_ADAPTER_ID) else { return };
        let Some(jobs_root) = claude_jobs_root(running) else { return };
        let leaked = self.not_yet_attempted(leaked_jobs(&jobs_root, archived));
        if leaked.is_empty() {
            return;
        }
        let mut purge = self.purge.lock().unwrap();
        // One purge at a time: the fresh list supersedes whatever the
        // previous one was still working through.
        if let Some(previous) = purge.take() {
            previous.abort();
        }
        tracing::info!(count = leaked.len(), "removing claude jobs of archived sessions");
        let task = tokio::spawn(purge_leaked_jobs(
            claude.commands_tx.clone(),
            claude.shutdown.clone(),
            jobs_root,
            leaked,
        ));
        *purge = Some(task.abort_handle());
    }
}

impl Supervisor {
    /// Drop the ids whose retry gap has not elapsed and record the rest as
    /// attempted now (see [`purge_due`]).
    fn not_yet_attempted(&self, ids: Vec<String>) -> Vec<String> {
        let now = tokio::time::Instant::now();
        let mut attempts = self.purge_attempts.lock().unwrap();
        purge_due(&mut attempts, ids, now)
    }
}

/// The claude adapter's jobs root, when the adapter is running.
fn claude_jobs_root(running: &HashMap<String, AdapterRunning>) -> Option<std::path::PathBuf> {
    let claude = running.get(CLAUDE_ADAPTER_ID)?;
    Some(claude.config.get("jobs_root").and_then(serde_json::Value::as_str).map_or_else(
        crate::adapters::claude_code::state::default_jobs_root,
        std::path::PathBuf::from,
    ))
}

/// Shorts of the job directories under `jobs_root`, sorted.
fn jobs_on_disk(jobs_root: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(jobs_root) else { return Vec::new() };
    let mut shorts: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| crate::configsweep::short_of(&e.file_name().to_string_lossy()))
        .collect();
    shorts.sort();
    shorts
}

async fn stop_adapters(running: &mut HashMap<String, AdapterRunning>) {
    let ids: Vec<String> = running.keys().cloned().collect();
    for id in ids {
        if let Some(r) = running.remove(&id) {
            r.stop().await;
            tracing::info!(adapter_id = %id, "stopped adapter for daemon shutdown");
        }
    }
}

impl Drop for AdapterRunning {
    /// A `CancellationToken` is not cancelled by being dropped, and the token is
    /// a child of the global shutdown token, so without this every dropped
    /// instance (notably the whole map, on a WS reconnect) would keep polling.
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Stage mid-chat attachments and build the `StageFilesResult` reply.
/// Filesystem-only and reuses the spawn-time staging dir, so it doesn't need the
/// running adapter beyond confirming the adapter is supported.
fn stage_files_result(
    request_id: uuid::Uuid,
    adapter_id: &str,
    local_id: &str,
    uploads: &[cctui_proto::adapter::BootstrapFile],
) -> DaemonFrameUp {
    // Staging is filesystem-only (writes to /tmp/cctui-uploads/<id>/ and returns
    // absolute paths the message text references), so it's adapter-agnostic —
    // codex reads staged file paths just like claude does.
    let result = if adapter_id == "claude-code" || adapter_id == "codex" {
        crate::adapters::claude_code::stage_mid_chat_files(local_id, uploads)
    } else {
        Err(anyhow::anyhow!("adapter {adapter_id} does not support mid-chat file staging"))
    };
    match result {
        Ok(paths) => {
            tracing::info!(%local_id, count = paths.len(), "staged mid-chat files");
            DaemonFrameUp::StageFilesResult { request_id, ok: true, paths, error: None }
        }
        Err(err) => {
            tracing::warn!(%local_id, %err, "mid-chat file staging failed");
            DaemonFrameUp::StageFilesResult {
                request_id,
                ok: false,
                paths: Vec::new(),
                error: Some(err.to_string()),
            }
        }
    }
}

/// Compile the effective scrub set from a synced [`SecretScrubConfig`]. The
/// correlation-suffix key is the daemon's `CCTUI_VAULT_KEY` if set (empty
/// otherwise, dropping the suffix — the daemon needn't hold the server key).
fn compile_scrub(cfg: &SecretScrubConfig) -> CompiledPatterns {
    let user: Vec<(String, String)> =
        cfg.patterns.iter().map(|p| (p.name.clone(), p.regex.clone())).collect();
    crate::adapters::codex::app_server::set_ring_scrub(&user);
    redact::compile(cfg.enabled, &user, &cctui_crypto::vault_key())
}

/// Redact secrets out of an event by round-tripping it through JSON and running
/// [`redact::redact_json`] over every string leaf. A no-op when scrubbing is
/// disabled; on the (structurally impossible) round-trip failure the original
/// event is passed through unchanged rather than dropped.
fn scrub_event(event: AdapterEvent, scrub: &CompiledPatterns) -> AdapterEvent {
    if scrub.is_empty() {
        return event;
    }
    let Ok(mut value) = serde_json::to_value(&event) else { return event };
    if redact::redact_json(&mut value, scrub) == 0 {
        return event;
    }
    serde_json::from_value(value).unwrap_or(event)
}

/// Claude Code requests `thinking.display: "omitted"`, so the API returns
/// thinking blocks as a bare replay signature. They render as nothing and cost
/// a `stream_events` row each.
fn is_textless_thinking(event: &AdapterEvent) -> bool {
    let AdapterEvent::Message { payload, .. } = event else { return false };
    if payload.get("role").and_then(serde_json::Value::as_str) != Some("assistant_thinking") {
        return false;
    }
    payload.get("text").and_then(serde_json::Value::as_str).is_none_or(|t| t.trim().is_empty())
}

fn event_local_id(event: &AdapterEvent) -> &str {
    match event {
        AdapterEvent::SessionStarted { local_id, .. }
        | AdapterEvent::Message { local_id, .. }
        | AdapterEvent::ToolUse { local_id, .. }
        | AdapterEvent::SessionEnded { local_id, .. }
        | AdapterEvent::Status { local_id, .. }
        | AdapterEvent::PermissionRequest { local_id, .. }
        | AdapterEvent::AskQuestion { local_id, .. }
        | AdapterEvent::AskResolved { local_id }
        | AdapterEvent::PlanRequest { local_id, .. }
        | AdapterEvent::PlanResolved { local_id } => local_id,
        _ => "",
    }
}

const fn event_kind(event: &AdapterEvent) -> &'static str {
    match event {
        AdapterEvent::SessionStarted { .. } => "session_started",
        AdapterEvent::Message { .. } => "message",
        AdapterEvent::ToolUse { .. } => "tool_use",
        AdapterEvent::SessionEnded { .. } => "session_ended",
        AdapterEvent::Status { .. } => "status",
        AdapterEvent::AskQuestion { .. } => "ask_question",
        AdapterEvent::AskResolved { .. } => "ask_resolved",
        AdapterEvent::PlanRequest { .. } => "plan_request",
        AdapterEvent::PlanResolved { .. } => "plan_resolved",
        _ => "other",
    }
}

/// Collapse a non-empty batch into one wire frame: a lone event stays an
/// `Event` (no envelope overhead); several become a `Batch` the server unwraps.
fn coalesce(frames: Vec<DaemonFrameUp>) -> DaemonFrameUp {
    if frames.len() == 1 {
        frames.into_iter().next().expect("len checked")
    } else {
        DaemonFrameUp::Batch { frames }
    }
}

/// Serialized byte size of a queued frame, for the pre-compression batch cap.
/// Falls back to 0 on the structurally-impossible serialization failure.
fn frame_size(frame: &DaemonFrameUp) -> usize {
    serde_json::to_vec(frame).map_or(0, |v| v.len())
}

/// Collect every event still owed to the wire at shutdown: the in-flight
/// micro-batch, plus events the adapters emit during their teardown flush.
/// Waits up to [`SHUTDOWN_DRAIN_QUIET`] for each next event and stops once the
/// pipeline goes quiet or [`SHUTDOWN_DRAIN_MAX`] elapses, so a stuck adapter
/// can't wedge teardown.
async fn drain_for_shutdown(
    batch: Vec<DaemonFrameUp>,
    event_rx: &mut mpsc::Receiver<(String, AdapterEvent)>,
    scrub: &CompiledPatterns,
) -> Vec<DaemonFrameUp> {
    let mut frames = batch;
    let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_MAX;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let step = SHUTDOWN_DRAIN_QUIET.min(deadline - now);
        match tokio::time::timeout(step, event_rx.recv()).await {
            Ok(Some((adapter_id, event))) => {
                let event = scrub_event(event, scrub);
                frames.push(DaemonFrameUp::Event { adapter_id, event });
            }
            // All senders dropped (adapters finished) or the pipeline went quiet.
            Ok(None) | Err(_) => break,
        }
    }
    frames
}

/// The wire form a prepared frame takes: a ready-to-send text message, or a
/// chunked transfer to drive over the connection with ack/resume.
enum Prepared {
    Frame(String),
    Chunked(PendingTransfer),
    /// Post-compression bytes over [`MAX_PAYLOAD_BYTES`]; dropped unsent.
    Oversized(usize),
}

/// Compress `inner` when worthwhile, then chunk the compressed bytes if they
/// still exceed the threshold. Composes compression with the
/// chunk transfer while preserving its ack/resume semantics.
fn prepare_send(inner: &DaemonFrameUp) -> anyhow::Result<Prepared> {
    let json = serde_json::to_vec(inner)?;
    let (bytes, codec) = cctui_proto::compress::maybe_compress(&json);
    classify(bytes, codec.map(str::to_owned))
}

/// Turn the final post-compression wire `bytes` into a send decision: over the
/// size cap → drop; over the chunk threshold → chunked transfer; else one frame.
fn classify(bytes: Vec<u8>, codec: Option<String>) -> anyhow::Result<Prepared> {
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Ok(Prepared::Oversized(bytes.len()));
    }
    if bytes.len() > cctui_proto::chunk::CHUNK_THRESHOLD {
        let transfer =
            PendingTransfer::new(bytes, codec).expect("bytes exceed the chunk threshold");
        Ok(Prepared::Chunked(transfer))
    } else if let Some(codec) = codec {
        let up = cctui_proto::compress::compressed_frame(&codec, &bytes);
        Ok(Prepared::Frame(serde_json::to_string(&up)?))
    } else {
        Ok(Prepared::Frame(String::from_utf8(bytes).expect("serde_json output is valid utf8")))
    }
}

const CLAUDE_ADAPTER_ID: &str = "claude-code";

/// Feed one `Remove` per leaked job into the claude adapter, at the adapter's
/// own pace. Each removal is slow (kill the worker, wait for it to exit, then
/// `claude rm`), so a backlog of hundreds (a daemon updated after weeks of
/// archives) takes minutes to hours: this must never run on the transport
/// loop, which has to keep pinging the server meanwhile. Skips jobs that
/// vanished in the meantime, so an overlapping purge can't double-remove.
async fn purge_leaked_jobs(
    commands_tx: mpsc::Sender<AdapterCommand>,
    shutdown: CancellationToken,
    jobs_root: std::path::PathBuf,
    leaked: Vec<String>,
) {
    let total = leaked.len();
    let mut sent = 0usize;
    for local_id in leaked {
        let still_there = crate::configsweep::short_of(&local_id)
            .is_some_and(|short| jobs_root.join(short).is_dir());
        if !still_there {
            continue;
        }
        let command = AdapterCommand::Remove { local_id, command_id: None };
        tokio::select! {
            () = shutdown.cancelled() => {
                tracing::info!(sent, total, "adapter stopped; leaked job purge interrupted");
                return;
            }
            res = commands_tx.send(command) => {
                if res.is_err() {
                    tracing::info!(sent, total, "adapter command channel closed; leaked job purge interrupted");
                    return;
                }
                sent += 1;
            }
        }
    }
    tracing::info!(sent, total, "queued removal of claude jobs of archived sessions");
}

/// Archived session ids whose claude job directory is still on disk.
fn leaked_jobs(jobs_root: &std::path::Path, archived: &[String]) -> Vec<String> {
    archived
        .iter()
        .filter(|id| {
            crate::configsweep::short_of(id).is_some_and(|short| jobs_root.join(short).is_dir())
        })
        .cloned()
        .collect()
}

fn parse_frame(msg: Message) -> anyhow::Result<Option<DaemonFrameDown>> {
    let txt = match msg {
        Message::Text(t) => t.to_string(),
        Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
        // Close + ping/pong/etc are not application frames.
        _ => return Ok(None),
    };
    Ok(Some(serde_json::from_str(&txt)?))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use cctui_proto::api::DaemonAdapterConfig;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use cctui_proto::adapter::AdapterCommand;
    use cctui_proto::chunk::{Accept, Reassembler};
    use cctui_proto::ws::DaemonFrameUp;

    use super::{
        AdapterRunning, Duration, LIVENESS_TIMEOUT, PING_INTERVAL, PendingTransfer, Supervisor,
        compile_scrub, is_textless_thinking, scrub_event,
    };
    use crate::adapter_runtime::{Adapter, AdapterCtx, AdapterFactory};
    use crate::client::ServerClient;

    #[test]
    fn textless_thinking_is_filtered() {
        let msg = |payload| cctui_proto::adapter::AdapterEvent::Message {
            local_id: "s".to_owned(),
            payload,
            turn_id: None,
        };
        let thinking = |text| serde_json::json!({ "role": "assistant_thinking", "text": text });

        assert!(is_textless_thinking(&msg(thinking(""))));
        assert!(is_textless_thinking(&msg(thinking("   \n "))));
        assert!(is_textless_thinking(&msg(serde_json::json!({ "role": "assistant_thinking" }))));
        assert!(is_textless_thinking(&msg(
            serde_json::json!({ "role": "assistant_thinking", "text": null })
        )));

        assert!(!is_textless_thinking(&msg(thinking("weighing the options"))));
        assert!(
            !is_textless_thinking(&msg(
                serde_json::json!({ "role": "assistant_redacted_thinking", "text": "" })
            )),
            "the redacted placeholder is a distinct role and must survive"
        );
        assert!(!is_textless_thinking(&msg(
            serde_json::json!({ "role": "assistant", "text": "" })
        )));
        assert!(!is_textless_thinking(&cctui_proto::adapter::AdapterEvent::ToolUse {
            local_id: "s".to_owned(),
            payload: serde_json::json!({ "tool": "Bash" }),
        }));
    }

    #[test]
    fn scrub_event_masks_tool_use_secret_before_send() {
        let cfg = cctui_proto::ws::SecretScrubConfig { enabled: true, patterns: vec![] };
        let scrub = compile_scrub(&cfg);
        let event = cctui_proto::adapter::AdapterEvent::ToolUse {
            local_id: "sess-1".to_owned(),
            payload: serde_json::json!({
                "command": "export GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWX0123",
            }),
        };
        let out = scrub_event(event, &scrub);
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWX0123"), "secret leaked: {json}");
        assert!(json.contains("[REDACTED:github_token"), "no placeholder: {json}");
    }

    #[test]
    fn scrub_event_is_noop_when_disabled() {
        let scrub = compile_scrub(&cctui_proto::ws::SecretScrubConfig::default());
        let event = cctui_proto::adapter::AdapterEvent::Message {
            local_id: "s".to_owned(),
            payload: serde_json::json!({ "text": "ghp_ABCDEFGHIJKLMNOPQRSTUVWX0123" }),
            turn_id: None,
        };
        let out = scrub_event(event, &scrub);
        assert!(serde_json::to_string(&out).unwrap().contains("ghp_ABCDEFGHIJKLMNOPQRSTUVWX0123"));
    }

    /// Records the config each `build` call was handed, so a test can assert
    /// whether (and with what config) an adapter was (re)built.
    #[derive(Clone, Default)]
    struct BuildRecorder {
        builds: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    struct StubAdapter;

    #[async_trait::async_trait]
    impl Adapter for StubAdapter {
        fn id(&self) -> &'static str {
            "stub"
        }
        async fn start(&self, ctx: AdapterCtx) -> anyhow::Result<()> {
            // Idle until cancelled so the spawned task doesn't error out.
            ctx.shutdown.cancelled().await;
            Ok(())
        }
    }

    struct StubFactory {
        recorder: BuildRecorder,
    }

    impl AdapterFactory for StubFactory {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn build(&self, config: serde_json::Value) -> Box<dyn Adapter> {
            self.recorder.builds.lock().unwrap().push(config);
            Box::new(StubAdapter)
        }
    }

    fn cfg(mode: &str) -> DaemonAdapterConfig {
        DaemonAdapterConfig {
            adapter_id: "stub".into(),
            config: serde_json::json!({ "mode": mode }),
            enabled: true,
        }
    }

    #[tokio::test]
    async fn config_change_rebuilds_adapter_identical_does_not_churn() {
        let recorder = BuildRecorder::default();
        let supervisor = Supervisor::new(
            ServerClient::new("http://localhost"),
            "machine-key".to_string(),
            vec![Box::new(StubFactory { recorder: recorder.clone() })],
        );
        let shutdown = CancellationToken::new();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();

        // First reconcile: adapter starts → one build with mode=bg.
        supervisor.reconcile(vec![cfg("bg")], &mut running, &event_tx, &shutdown);
        let first_token = running.get("stub").expect("adapter running").shutdown.clone();
        assert_eq!(recorder.builds.lock().unwrap().len(), 1);

        // Identical reconcile: no churn — still one build, same instance.
        supervisor.reconcile(vec![cfg("bg")], &mut running, &event_tx, &shutdown);
        assert_eq!(recorder.builds.lock().unwrap().len(), 1, "identical config must not rebuild");
        assert!(!first_token.is_cancelled(), "identical config must not cancel the adapter");

        // Changed reconcile (mode bg→sdk): rebuild — second build with the new
        // config, old instance cancelled, fresh token in the map.
        supervisor.reconcile(vec![cfg("sdk")], &mut running, &event_tx, &shutdown);
        let builds = recorder.builds.lock().unwrap().clone();
        assert_eq!(builds.len(), 2, "config change must rebuild the adapter");
        assert_eq!(builds[1], serde_json::json!({ "mode": "sdk" }));
        assert!(first_token.is_cancelled(), "old adapter instance must be cancelled on rebuild");
        let new_token = &running.get("stub").expect("adapter still running").shutdown;
        assert!(!new_token.is_cancelled(), "rebuilt adapter must be live");
    }

    #[tokio::test]
    async fn command_for_unknown_adapter_fails_the_command_result() {
        let supervisor = Supervisor::new(
            ServerClient::new("http://localhost"),
            "machine-key".to_string(),
            vec![],
        );
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (frame_up_tx, _frame_up_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();
        let mut scrub = cctui_crypto::redact::CompiledPatterns::disabled();
        let command_id = uuid::Uuid::new_v4();
        let spec = cctui_proto::adapter::SessionSpec {
            service_tier: None,
            adapter_id: "opencode".into(),
            working_dir: None,
            prompt: None,
            name: None,
            permission_mode: None,
            effort: None,
            model: None,
            env: std::collections::BTreeMap::new(),
            bootstrap: serde_json::Value::Null,
            parent_local_id: None,
        };
        let frame = cctui_proto::ws::DaemonFrameDown::Command {
            adapter_id: "opencode".to_owned(),
            command: Box::new(cctui_proto::adapter::AdapterCommand::Spawn {
                spec,
                command_id: Some(command_id),
                session_id: Some(command_id),
            }),
        };
        supervisor
            .handle_frame(frame, &mut running, &event_tx, &frame_up_tx, &mut scrub, &shutdown)
            .await;
        match event_rx.recv().await {
            Some((
                adapter_id,
                cctui_proto::adapter::AdapterEvent::CommandResult { command_id: got, ok, error },
            )) => {
                assert_eq!(adapter_id, "opencode");
                assert_eq!(got, command_id);
                assert!(!ok);
                assert!(error.unwrap().contains("not running"));
            }
            other => panic!("expected a failed CommandResult, got {other:?}"),
        }
    }

    /// A `Command` for an adapter whose queue is full must neither block the
    /// transport loop nor vanish: the server-side waiter gets a failed result.
    #[tokio::test]
    async fn command_for_a_full_adapter_queue_is_rejected_not_awaited() {
        let supervisor = Supervisor::new(
            ServerClient::new("http://localhost"),
            "machine-key".to_string(),
            vec![],
        );
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (frame_up_tx, _frame_up_rx) = mpsc::channel(8);
        let (commands_tx, _commands_rx) = mpsc::channel(1);
        // Fill the (never drained) queue.
        commands_tx
            .try_send(cctui_proto::adapter::AdapterCommand::ResumeMarks { marks: vec![] })
            .unwrap();
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();
        running.insert(
            "claude-code".to_owned(),
            AdapterRunning {
                shutdown: CancellationToken::new(),
                config: serde_json::json!({}),
                commands_tx,
                tasks: Vec::new(),
            },
        );
        let mut scrub = cctui_crypto::redact::CompiledPatterns::disabled();
        let command_id = uuid::Uuid::new_v4();
        let frame = cctui_proto::ws::DaemonFrameDown::Command {
            adapter_id: "claude-code".to_owned(),
            command: Box::new(cctui_proto::adapter::AdapterCommand::Remove {
                local_id: "deadbeef-0000-0000-0000-000000000000".to_owned(),
                command_id: Some(command_id),
            }),
        };
        let handled = supervisor.handle_frame(
            frame,
            &mut running,
            &event_tx,
            &frame_up_tx,
            &mut scrub,
            &shutdown,
        );
        tokio::time::timeout(Duration::from_secs(5), handled)
            .await
            .expect("handle_frame must not block on a full adapter queue");
        match event_rx.try_recv() {
            Ok((
                _,
                cctui_proto::adapter::AdapterEvent::CommandResult { command_id: got, ok, error },
            )) => {
                assert_eq!(got, command_id);
                assert!(!ok);
                assert!(error.unwrap().contains("queue is full"));
            }
            other => panic!("expected a failed CommandResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_marks_removes_leaked_jobs_of_archived_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = tmp.path().join("jobs");
        std::fs::create_dir_all(jobs.join("deadbeef")).unwrap();
        std::fs::create_dir_all(jobs.join("cafebabe")).unwrap();
        let (commands_tx, mut commands_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();
        running.insert(
            "claude-code".to_owned(),
            AdapterRunning {
                shutdown: CancellationToken::new(),
                config: serde_json::json!({ "jobs_root": jobs.to_str().unwrap() }),
                commands_tx,
                tasks: Vec::new(),
            },
        );
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (frame_up_tx, _frame_up_rx) = mpsc::channel(8);
        let mut scrub = cctui_crypto::redact::CompiledPatterns::disabled();
        let supervisor = Supervisor::new(
            ServerClient::new("http://localhost"),
            "machine-key".to_string(),
            vec![],
        );
        let frame = cctui_proto::ws::DaemonFrameDown::ResumeMarks {
            session_marks: vec![],
            archived: vec![
                "deadbeef-0000-0000-0000-000000000000".to_owned(),
                "12345678-0000-0000-0000-000000000000".to_owned(),
                "cafebabe-0000-0000-0000-000000000000".to_owned(),
            ],
        };
        supervisor
            .handle_frame(
                frame,
                &mut running,
                &event_tx,
                &frame_up_tx,
                &mut scrub,
                &CancellationToken::new(),
            )
            .await;
        // The purge runs on its own task, so the commands arrive asynchronously.
        let mut removed = Vec::new();
        while removed.len() < 2 {
            let cmd = tokio::time::timeout(Duration::from_secs(5), commands_rx.recv())
                .await
                .expect("purge must queue the leaked removals")
                .expect("command channel open");
            match cmd {
                cctui_proto::adapter::AdapterCommand::Remove { local_id, command_id } => {
                    assert!(command_id.is_none());
                    removed.push(local_id);
                }
                // The marks fan-out precedes the purge.
                cctui_proto::adapter::AdapterCommand::ResumeMarks { .. } => {}
                other => panic!("expected Remove, got {other:?}"),
            }
        }
        assert_eq!(
            removed,
            vec![
                "deadbeef-0000-0000-0000-000000000000".to_owned(),
                "cafebabe-0000-0000-0000-000000000000".to_owned(),
            ]
        );
        tokio::task::yield_now().await;
        assert!(commands_rx.try_recv().is_err(), "a session without a job dir must not be removed");
    }

    fn claude_running(
        jobs: &std::path::Path,
        commands_tx: mpsc::Sender<AdapterCommand>,
    ) -> std::collections::HashMap<String, AdapterRunning> {
        let mut running = std::collections::HashMap::new();
        running.insert(
            "claude-code".to_owned(),
            AdapterRunning {
                shutdown: CancellationToken::new(),
                config: serde_json::json!({ "jobs_root": jobs.to_str().unwrap() }),
                commands_tx,
                tasks: Vec::new(),
            },
        );
        running
    }

    async fn expect_removals(
        commands_rx: &mut mpsc::Receiver<AdapterCommand>,
        count: usize,
    ) -> Vec<String> {
        let mut removed = Vec::new();
        while removed.len() < count {
            let cmd = tokio::time::timeout(Duration::from_secs(5), commands_rx.recv())
                .await
                .expect("purge must queue the leaked removals")
                .expect("command channel open");
            match cmd {
                AdapterCommand::Remove { local_id, command_id } => {
                    assert!(command_id.is_none());
                    removed.push(local_id);
                }
                other => panic!("expected Remove, got {other:?}"),
            }
        }
        removed
    }

    #[tokio::test]
    async fn archived_jobs_frame_removes_leaked_jobs_once_per_retry_window() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = tmp.path().join("jobs");
        std::fs::create_dir_all(jobs.join("deadbeef")).unwrap();
        std::fs::create_dir_all(jobs.join("cafebabe")).unwrap();
        let (commands_tx, mut commands_rx) = mpsc::channel(8);
        let mut running = claude_running(&jobs, commands_tx);
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (frame_up_tx, _frame_up_rx) = mpsc::channel(8);
        let mut scrub = cctui_crypto::redact::CompiledPatterns::disabled();
        let supervisor = Supervisor::new(
            ServerClient::new("http://localhost"),
            "machine-key".to_string(),
            vec![],
        );
        let frame = || cctui_proto::ws::DaemonFrameDown::ArchivedJobs {
            session_ids: vec![
                "deadbeef-0000-0000-0000-000000000000".to_owned(),
                "12345678-0000-0000-0000-000000000000".to_owned(),
                "cafebabe-0000-0000-0000-000000000000".to_owned(),
            ],
        };
        supervisor
            .handle_frame(
                frame(),
                &mut running,
                &event_tx,
                &frame_up_tx,
                &mut scrub,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(
            expect_removals(&mut commands_rx, 2).await,
            vec![
                "deadbeef-0000-0000-0000-000000000000".to_owned(),
                "cafebabe-0000-0000-0000-000000000000".to_owned(),
            ]
        );
        tokio::task::yield_now().await;
        assert!(commands_rx.try_recv().is_err(), "a session without a job dir must not be removed");

        // The next heartbeat's answer names the same jobs (`claude rm` may
        // have refused); they are not re-queued inside the retry window.
        supervisor
            .handle_frame(
                frame(),
                &mut running,
                &event_tx,
                &frame_up_tx,
                &mut scrub,
                &CancellationToken::new(),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(commands_rx.try_recv().is_err(), "removal must not repeat every heartbeat");
    }

    #[test]
    fn purge_gap_doubles_and_caps() {
        assert_eq!(super::purge_gap(0), Duration::from_mins(10));
        assert_eq!(super::purge_gap(1), Duration::from_mins(20));
        assert_eq!(super::purge_gap(3), Duration::from_mins(80));
        assert_eq!(super::purge_gap(8), Duration::from_hours(24));
        assert_eq!(super::purge_gap(40), Duration::from_hours(24));
    }

    /// A job that stays on disk after each attempt (a root-owned file, a
    /// worktree occupied by a live session) is retried on a doubling gap,
    /// and its history is forgotten once the dir is gone.
    #[test]
    fn purge_due_backs_off_while_the_job_stays_on_disk() {
        let mut attempts = std::collections::HashMap::new();
        let job = || vec!["deadbeef".to_owned()];
        let t0 = tokio::time::Instant::from_std(std::time::Instant::now());
        assert_eq!(super::purge_due(&mut attempts, job(), t0), job(), "first sighting is due");
        assert!(super::purge_due(&mut attempts, job(), t0 + Duration::from_mins(9)).is_empty());
        assert_eq!(super::purge_due(&mut attempts, job(), t0 + Duration::from_mins(10)), job());
        // Second failure: the gap is now 20 minutes.
        let t1 = t0 + Duration::from_mins(10);
        assert!(super::purge_due(&mut attempts, job(), t1 + Duration::from_mins(19)).is_empty());
        assert_eq!(super::purge_due(&mut attempts, job(), t1 + Duration::from_mins(20)), job());
        assert_eq!(attempts["deadbeef"].failures, 2);
        // The dir is gone (or the session is no longer archived): forgotten,
        // so a fresh leak of the same id starts over at the first gap.
        assert!(super::purge_due(&mut attempts, vec![], t1 + Duration::from_mins(21)).is_empty());
        assert!(attempts.is_empty());
        assert_eq!(super::purge_due(&mut attempts, job(), t1 + Duration::from_mins(22)), job());
        assert_eq!(attempts["deadbeef"].failures, 0);
    }

    #[test]
    fn jobs_on_disk_lists_job_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = tmp.path().join("jobs");
        std::fs::create_dir_all(jobs.join("deadbeef")).unwrap();
        std::fs::create_dir_all(jobs.join("cafebabe")).unwrap();
        std::fs::create_dir_all(jobs.join("not-a-short")).unwrap();
        std::fs::write(jobs.join("pins.json"), "{}").unwrap();
        std::fs::write(jobs.join("0badf00d"), "").unwrap();
        assert_eq!(super::jobs_on_disk(&jobs), vec!["cafebabe".to_owned(), "deadbeef".to_owned()]);
        assert!(super::jobs_on_disk(&tmp.path().join("missing")).is_empty());
    }

    /// A claude-code stand-in that never drains its command channel, the way
    /// the real adapter looks while it serialises `Remove`s (kill, wait for the
    /// worker, `claude rm`: tens of seconds each).
    struct StuckAdapter;

    #[async_trait::async_trait]
    impl Adapter for StuckAdapter {
        fn id(&self) -> &'static str {
            "claude-code"
        }
        async fn start(&self, ctx: AdapterCtx) -> anyhow::Result<()> {
            // Hold `ctx.commands` open without ever receiving from it.
            ctx.shutdown.cancelled().await;
            Ok(())
        }
    }

    struct StuckFactory;

    impl AdapterFactory for StuckFactory {
        fn id(&self) -> &'static str {
            "claude-code"
        }
        fn build(&self, _config: serde_json::Value) -> Box<dyn Adapter> {
            Box::new(StuckAdapter)
        }
    }

    /// Regression for the connect-time freeze: a daemon that has never purged
    /// (updated after many archives) receives a `ResumeMarks` whose `archived`
    /// list matches hundreds of job dirs on disk. Every matching job used to be
    /// pushed into the adapter's 64-deep command channel with `send().await`
    /// from inside the transport `select!`, so the 65th blocked the loop: no
    /// more pings, the server evicted the daemon after its 60s read timeout,
    /// and the daemon never even read the Close. The invariant: pings keep
    /// flowing during the whole purge, whatever the backlog.
    #[tokio::test]
    async fn resume_marks_backlog_does_not_starve_pings() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        const BACKLOG: usize = 500; // load_archived's LIMIT
        const PING: Duration = Duration::from_millis(50);

        let tmp = tempfile::tempdir().unwrap();
        let jobs = tmp.path().join("jobs");
        let archived: Vec<String> =
            (0..BACKLOG).map(|i| format!("{i:08x}-0000-0000-0000-000000000000")).collect();
        for id in &archived {
            std::fs::create_dir_all(jobs.join(&id[..8])).unwrap();
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let supervisor = Supervisor::new(
            ServerClient::new(format!("http://{addr}")),
            "machine-key".to_string(),
            vec![Box::new(StuckFactory)],
        )
        .with_ping_interval(PING);
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();

        let jobs_root = jobs.to_str().unwrap().to_owned();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            let reconcile = serde_json::json!({
                "type": "reconcile",
                "adapters": [{
                    "adapter_id": "claude-code",
                    "config": { "jobs_root": jobs_root },
                    "enabled": true,
                }],
                "secret_scrub": [],
            });
            ws.send(Message::Text(reconcile.to_string().into())).await.unwrap();
            let marks =
                cctui_proto::ws::DaemonFrameDown::ResumeMarks { session_marks: vec![], archived };
            ws.send(Message::Text(serde_json::to_string(&marks).unwrap().into())).await.unwrap();

            // Count keepalives over a window worth forty ping intervals; the
            // bar below is a quarter of that, to ride out a loaded test host.
            let window = PING * 40;
            let deadline = tokio::time::Instant::now() + window;
            let mut pings = 0usize;
            loop {
                let Ok(next) = tokio::time::timeout_at(deadline, ws.next()).await else { break };
                match next {
                    Some(Ok(Message::Ping(_))) => pings += 1,
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
            // Finish the close handshake so the daemon sees a clean Close
            // rather than a connection reset.
            ws.close(None).await.ok();
            while let Some(Ok(_)) = ws.next().await {}
            pings
        });

        // A frozen loop never reads the server's Close either, so bound the
        // wait: a timeout here is the freeze, not a slow test.
        let run = supervisor.run_once(shutdown.clone(), &mut running, &event_tx, &mut event_rx);
        // Only the return matters: a loop that answers the Close is alive.
        let _ = tokio::time::timeout(PING * 600, run)
            .await
            .expect("transport loop froze: it never read the server's Close");
        let pings = server.await.unwrap();
        assert!(
            pings >= 10,
            "expected keepalive pings throughout the purge of {BACKLOG} jobs, got {pings}"
        );
        super::stop_adapters(&mut running).await;
    }

    fn chunk_parts(frame: DaemonFrameUp) -> (String, u32, u32, String) {
        match frame {
            DaemonFrameUp::Chunk { transfer_id, chunk_index, total_chunks, data, .. } => {
                (transfer_id, chunk_index, total_chunks, data)
            }
            _ => panic!("expected a chunk frame"),
        }
    }

    #[test]
    fn small_frames_take_the_single_message_fast_path() {
        assert!(PendingTransfer::new(vec![0u8; 1024], None).is_none());
        assert!(
            PendingTransfer::new(vec![0u8; cctui_proto::chunk::CHUNK_THRESHOLD], None).is_none()
        );
        assert!(
            PendingTransfer::new(vec![0u8; cctui_proto::chunk::CHUNK_THRESHOLD + 1], None)
                .is_some()
        );
    }

    #[test]
    fn resume_completes_20mb_transfer_across_repeated_disconnects() {
        // The ticket's acceptance test: a 20MB event over a link killed every
        // few chunks must still complete by resuming from the last acked chunk.
        let payload: Vec<u8> =
            (0..20 * 1024 * 1024).map(|i| u8::try_from(i % 251).unwrap()).collect();
        let mut sender = PendingTransfer::new(payload.clone(), None).expect("20MB must chunk");
        let total = sender.total;
        let mut server = Reassembler::new(64 * 1024 * 1024);
        let mut completed: Option<Vec<u8>> = None;
        let mut connections = 0u32;
        let mut sends = 0u32;
        let kill_after = 5u32;
        let mut guard = 0u32;
        while completed.is_none() {
            guard += 1;
            assert!(guard < 100_000, "resume loop failed to converge");
            connections += 1;
            sender.rewind_to_ack();
            let mut sent_this_conn = 0u32;
            while sender.has_unsent() {
                let (id, idx, tot, data) = chunk_parts(sender.next_frame().0);
                sends += 1;
                match server.accept(&id, idx, tot, &data) {
                    Accept::Pending(highest) => sender.record_ack(highest),
                    Accept::Complete(bytes) => {
                        completed = Some(bytes);
                        break;
                    }
                    Accept::Restart => sender.record_ack(None),
                }
                sent_this_conn += 1;
                if sent_this_conn >= kill_after {
                    break;
                }
            }
        }
        assert_eq!(completed.unwrap(), payload, "resumed transfer must be byte-exact");
        assert!(connections > 1, "completing must have spanned multiple connections");
        // Resuming past the acked prefix means no chunk is ever re-sent.
        assert_eq!(sends, total, "resume must not re-upload already-acked chunks");
    }

    fn synth_event(i: usize) -> DaemonFrameUp {
        DaemonFrameUp::Event {
            adapter_id: "claude-code".into(),
            event: cctui_proto::adapter::AdapterEvent::Message {
                local_id: format!("sess-{}", i % 4),
                payload: serde_json::json!({ "text": format!("event number {i}"), "n": i }),
                turn_id: None,
            },
        }
    }

    fn hi_entropy_event(rng: &mut u64, i: usize) -> DaemonFrameUp {
        let bytes: Vec<u8> = (0..12_500)
            .map(|_| {
                *rng ^= *rng << 13;
                *rng ^= *rng >> 7;
                *rng ^= *rng << 17;
                *rng as u8
            })
            .collect();
        let blob = hex::encode(bytes);
        DaemonFrameUp::Event {
            adapter_id: "claude-code".into(),
            event: cctui_proto::adapter::AdapterEvent::Message {
                local_id: format!("s{i}"),
                payload: serde_json::json!({ "n": i, "blob": blob }),
                turn_id: None,
            },
        }
    }

    #[test]
    fn coalesce_single_stays_event_many_becomes_batch() {
        assert!(matches!(super::coalesce(vec![synth_event(0)]), DaemonFrameUp::Event { .. }));
        let batch = super::coalesce((0..3).map(synth_event).collect());
        match batch {
            DaemonFrameUp::Batch { frames } => assert_eq!(frames.len(), 3),
            _ => panic!("expected a batch"),
        }
    }

    #[test]
    fn small_batch_sends_plain_uncompressed_frame() {
        // One small event flushes as a plain Event frame — never compressed and
        // never chunked, so tiny frames aren't taxed with zstd overhead.
        let prepared = super::prepare_send(&synth_event(1)).unwrap();
        let super::Prepared::Frame(text) = prepared else { panic!("small frame must not chunk") };
        let back: DaemonFrameUp = serde_json::from_str(&text).unwrap();
        assert!(matches!(back, DaemonFrameUp::Event { .. }), "small frame stays a plain Event");
    }

    #[test]
    fn oversized_payload_is_dropped_without_chunking() {
        // Over the 32 MiB cap → Oversized (dropped); at the cap → normal chunk.
        let over = vec![0u8; super::MAX_PAYLOAD_BYTES + 1];
        assert!(matches!(super::classify(over, None).unwrap(), super::Prepared::Oversized(_)));
        let at_cap = vec![0u8; super::MAX_PAYLOAD_BYTES];
        assert!(matches!(super::classify(at_cap, None).unwrap(), super::Prepared::Chunked(_)));
    }

    #[test]
    fn session_ids_lists_batch_members_for_the_give_up_log() {
        let mut rng = 0x1234_5678_9abc_def0_u64;
        let batch = super::coalesce((0..800).map(|i| hi_entropy_event(&mut rng, i)).collect());
        let super::Prepared::Chunked(t) = super::prepare_send(&batch).unwrap() else {
            panic!("a large batch must chunk");
        };
        let ids = t.session_ids();
        assert!(!ids.is_empty(), "the give-up log must name the affected sessions");
        assert!(ids.iter().all(|s| s.starts_with('s')), "ids are the batch's local ids");
    }

    #[test]
    fn heartbeat_prepares_as_immediate_plain_frame() {
        // Heartbeats never enter the batch buffer (the ping arm sends them
        // directly); prepared, one is a small plain frame, proving the control
        // path is never delayed or wrapped by batching.
        let hb = DaemonFrameUp::Heartbeat {
            sent_at: chrono::Utc::now(),
            bandwidth: None,
            update_hook: None,
            resources: None,
            claude_jobs: None,
        };
        let super::Prepared::Frame(text) = super::prepare_send(&hb).unwrap() else {
            panic!("heartbeat must not chunk")
        };
        assert!(text.contains(r#""type":"heartbeat""#));
    }

    #[test]
    fn batched_compressed_chunked_20mb_resumes_across_disconnects() {
        // The acceptance test, extended for a ~20MB batch of
        // events is coalesced, compressed, and chunked; a link killed every few
        // chunks must still complete by resuming, and the server-side reassemble
        // → decompress → parse must recover the exact batch.
        let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
        let events: Vec<DaemonFrameUp> = (0..800).map(|i| hi_entropy_event(&mut rng, i)).collect();
        let want = events.len();
        let inner = super::coalesce(events);
        let super::Prepared::Chunked(mut sender) = super::prepare_send(&inner).unwrap() else {
            panic!("a 20MB batch must chunk")
        };
        let codec = sender.codec.clone();
        assert_eq!(codec.as_deref(), Some("zstd"), "large batch must be zstd-tagged");
        let total = sender.total;

        let mut server = Reassembler::new(128 * 1024 * 1024);
        let mut compressed: Option<Vec<u8>> = None;
        let mut sends = 0u32;
        let mut connections = 0u32;
        let mut guard = 0u32;
        while compressed.is_none() {
            guard += 1;
            assert!(guard < 100_000, "resume loop failed to converge");
            connections += 1;
            sender.rewind_to_ack();
            let mut sent_this_conn = 0u32;
            while sender.has_unsent() {
                let (id, idx, tot, data) = chunk_parts(sender.next_frame().0);
                sends += 1;
                match server.accept(&id, idx, tot, &data) {
                    Accept::Pending(highest) => sender.record_ack(highest),
                    Accept::Complete(bytes) => {
                        compressed = Some(bytes);
                        break;
                    }
                    Accept::Restart => sender.record_ack(None),
                }
                sent_this_conn += 1;
                if sent_this_conn >= 4 {
                    break;
                }
            }
        }
        assert!(connections > 1, "completing must span multiple connections");
        assert_eq!(sends, total, "resume must not re-upload acked chunks");
        let joined = cctui_proto::compress::decompress_codec("zstd", &compressed.unwrap()).unwrap();
        let back: DaemonFrameUp = serde_json::from_slice(&joined).unwrap();
        match back {
            DaemonFrameUp::Batch { frames } => assert_eq!(frames.len(), want),
            _ => panic!("reassembled payload must be the original batch"),
        }
    }

    #[test]
    fn next_frame_flags_resent_chunks_as_retransmit() {
        // First pass: every chunk is forward (never emitted before).
        let mut t =
            PendingTransfer::new(vec![9u8; cctui_proto::chunk::CHUNK_SIZE * 3 + 1], None).unwrap();
        assert_eq!(t.total, 4);
        for _ in 0..4 {
            assert!(!t.next_frame().1, "first emission of a chunk is forward");
        }
        // A disconnect acked chunk 0 only; resume rewinds to chunk 1. Chunks 1..3
        // were already emitted, so re-sending them is a retransmit.
        t.record_ack(Some(0));
        t.rewind_to_ack();
        for expected_idx in 1..4 {
            let (frame, retransmit) = t.next_frame();
            assert!(retransmit, "re-sending an already-emitted chunk is a retransmit");
            let (_, idx, _, _) = chunk_parts(frame);
            assert_eq!(idx, expected_idx);
        }
    }

    #[test]
    fn forward_event_frame_bytes_are_counted() {
        use crate::counters::{BandwidthCounters, Subsystem};
        // The batch-flush arm accounts a plain (non-chunked) event frame as
        // forward-path bytes; assert the accounting the arm performs.
        let counters = BandwidthCounters::new();
        let super::Prepared::Frame(text) = super::prepare_send(&synth_event(1)).unwrap() else {
            panic!("small event must be a plain frame")
        };
        counters.add(Subsystem::Forward, text.len() as u64);
        assert_eq!(counters.summary().forward, text.len() as u64);
        assert!(counters.summary().forward > 0);
    }

    #[test]
    fn record_ack_is_monotonic_and_marks_completion() {
        let mut t =
            PendingTransfer::new(vec![7u8; cctui_proto::chunk::CHUNK_SIZE * 3 + 1], None).unwrap();
        assert_eq!(t.total, 4);
        t.record_ack(Some(2));
        // A stale lower ack must not rewind progress.
        t.record_ack(Some(1));
        assert_eq!(t.resume_index(), 3);
        assert!(!t.is_complete());
        t.record_ack(Some(3));
        assert!(t.is_complete());
    }

    #[test]
    fn liveness_timeout_allows_at_least_two_missed_pings() {
        // The daemon pings every PING_INTERVAL and the server auto-pongs, so
        // last_rx refreshes each interval. The liveness window must span
        // several pings or a single dropped pong would trigger a needless
        // reconnect.
        assert!(
            LIVENESS_TIMEOUT >= PING_INTERVAL * 2,
            "LIVENESS_TIMEOUT must tolerate >=2 missed pings to avoid flapping"
        );
    }

    fn msg_event(local_id: &str) -> cctui_proto::adapter::AdapterEvent {
        cctui_proto::adapter::AdapterEvent::Message {
            local_id: local_id.to_owned(),
            payload: serde_json::json!({ "text": "tail" }),
            turn_id: None,
        }
    }

    #[tokio::test]
    async fn shutdown_drain_keeps_batch_and_flushes_late_tail() {
        let scrub = compile_scrub(&cctui_proto::ws::SecretScrubConfig::default());
        let (tx, mut rx) = mpsc::channel::<(String, cctui_proto::adapter::AdapterEvent)>(8);
        // A late teardown-flush event lands after the drain begins.
        tx.send(("claude-code".to_owned(), msg_event("late"))).await.unwrap();
        drop(tx);
        let batch = vec![synth_event(0)];
        let frames = super::drain_for_shutdown(batch, &mut rx, &scrub).await;
        assert_eq!(frames.len(), 2, "the in-flight batch plus the drained tail must survive");
        let ids: Vec<String> = frames
            .iter()
            .map(|f| match f {
                DaemonFrameUp::Event { event, .. } => super::event_local_id(event).to_owned(),
                _ => panic!("drain must produce Event frames"),
            })
            .collect();
        assert_eq!(ids, vec!["sess-0", "late"], "batch first, then the drained tail, in order");
    }

    #[tokio::test]
    async fn shutdown_drain_stops_when_all_senders_drop() {
        let scrub = compile_scrub(&cctui_proto::ws::SecretScrubConfig::default());
        let (tx, mut rx) = mpsc::channel::<(String, cctui_proto::adapter::AdapterEvent)>(8);
        drop(tx);
        let started = tokio::time::Instant::now();
        let frames = super::drain_for_shutdown(Vec::new(), &mut rx, &scrub).await;
        assert!(frames.is_empty());
        assert!(
            started.elapsed() < super::SHUTDOWN_DRAIN_MAX,
            "a closed channel must end the drain before the hard cap"
        );
    }

    #[test]
    fn parse_frame_decodes_text_and_binary_and_skips_control() {
        use tokio_tungstenite::tungstenite::Message;
        let json = r#"{"type":"ack","seq":7}"#;
        for msg in [Message::Text(json.into()), Message::Binary(json.as_bytes().to_vec().into())] {
            match super::parse_frame(msg).unwrap() {
                Some(cctui_proto::ws::DaemonFrameDown::Ack { seq }) => assert_eq!(seq, 7),
                other => panic!("expected Ack, got {other:?}"),
            }
        }
        assert!(super::parse_frame(Message::Ping(Vec::new().into())).unwrap().is_none());
        assert!(super::parse_frame(Message::Close(None)).unwrap().is_none());
    }

    /// Counts adapter instances that are live at the same time. `peak > 1`
    /// means two codex `LogTail`s would share one `codex-offsets.json`.
    #[derive(Clone, Default)]
    struct LiveTracker {
        live: Arc<Mutex<usize>>,
        peak: Arc<Mutex<usize>>,
        sessions: Arc<Mutex<usize>>,
        builds: Arc<Mutex<usize>>,
        connects: Arc<Mutex<usize>>,
    }

    impl LiveTracker {
        fn live(&self) -> usize {
            *self.live.lock().unwrap()
        }
        fn peak(&self) -> usize {
            *self.peak.lock().unwrap()
        }
        fn sessions(&self) -> usize {
            *self.sessions.lock().unwrap()
        }
        fn builds(&self) -> usize {
            *self.builds.lock().unwrap()
        }
        fn connects(&self) -> usize {
            *self.connects.lock().unwrap()
        }
    }

    struct TrackedAdapter {
        tracker: LiveTracker,
    }

    #[async_trait::async_trait]
    impl Adapter for TrackedAdapter {
        fn id(&self) -> &'static str {
            "stub"
        }
        async fn start(&self, mut ctx: AdapterCtx) -> anyhow::Result<()> {
            {
                let mut live = self.tracker.live.lock().unwrap();
                *live += 1;
                let mut peak = self.tracker.peak.lock().unwrap();
                *peak = (*peak).max(*live);
            }
            // Stands in for a session subprocess whose lifetime is bound to the
            // adapter token, the way `opencode serve` and `codex app-server` are.
            *self.tracker.sessions.lock().unwrap() += 1;
            loop {
                tokio::select! {
                    () = ctx.shutdown.cancelled() => break,
                    edge = ctx.connected.recv() => {
                        if edge.is_ok() {
                            *self.tracker.connects.lock().unwrap() += 1;
                        }
                    }
                }
            }
            *self.tracker.sessions.lock().unwrap() -= 1;
            *self.tracker.live.lock().unwrap() -= 1;
            Ok(())
        }
    }

    struct TrackedFactory {
        tracker: LiveTracker,
    }

    impl AdapterFactory for TrackedFactory {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn build(&self, _config: serde_json::Value) -> Box<dyn Adapter> {
            *self.tracker.builds.lock().unwrap() += 1;
            Box::new(TrackedAdapter { tracker: self.tracker.clone() })
        }
    }

    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition never held");
    }

    fn tracked() -> (Supervisor, LiveTracker) {
        let tracker = LiveTracker::default();
        let supervisor = Supervisor::new(
            ServerClient::new("http://localhost"),
            "machine-key".to_string(),
            vec![Box::new(TrackedFactory { tracker: tracker.clone() })],
        );
        (supervisor, tracker)
    }

    /// Accept one WS connection, send a `Reconcile` for the stub adapter, then
    /// close — the server-rollout shape of a disconnect.
    async fn serve_one_reconcile(listener: &tokio::net::TcpListener) {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let (sock, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
        let frame = serde_json::json!({
            "type": "reconcile",
            "adapters": [{ "adapter_id": "stub", "config": { "mode": "bg" }, "enabled": true }],
            "secret_scrub": [],
        });
        ws.send(Message::Text(frame.to_string().into())).await.unwrap();
        ws.close(None).await.unwrap();
    }

    #[tokio::test]
    async fn reconnect_keeps_adapters_and_their_live_sessions_running() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tracker = LiveTracker::default();
        let supervisor = Supervisor::new(
            ServerClient::new(format!("http://{addr}")),
            "machine-key".to_string(),
            vec![Box::new(TrackedFactory { tracker: tracker.clone() })],
        );
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();

        let server = tokio::spawn(async move {
            serve_one_reconcile(&listener).await;
            listener
        });
        supervisor
            .run_once(shutdown.clone(), &mut running, &event_tx, &mut event_rx)
            .await
            .unwrap();
        let listener = server.await.unwrap();
        let token = running.get("stub").expect("adapter running").shutdown.clone();
        wait_until(|| tracker.sessions() == 1).await;

        assert!(!token.is_cancelled(), "a WS disconnect must not cancel a live adapter");
        assert_eq!(tracker.sessions(), 1, "a WS disconnect must not kill a live session");
        assert!(!shutdown.is_cancelled());

        let server = tokio::spawn(async move { serve_one_reconcile(&listener).await });
        supervisor
            .run_once(shutdown.clone(), &mut running, &event_tx, &mut event_rx)
            .await
            .unwrap();
        server.await.unwrap();

        let after = running.get("stub").expect("adapter still running").shutdown.clone();
        assert!(!after.is_cancelled(), "the reconnect must not cancel the adapter");
        assert_eq!(tracker.sessions(), 1, "the live session must survive the reconnect");
        assert_eq!(tracker.builds(), 1, "a reconnect with unchanged config must not rebuild");
        assert_eq!(tracker.peak(), 1, "only one instance may own codex-offsets.json at a time");

        super::stop_adapters(&mut running).await;
        assert!(after.is_cancelled());
        assert_eq!(tracker.sessions(), 0, "daemon shutdown must stop the session");
        assert!(running.is_empty());
    }

    #[tokio::test]
    async fn every_connect_signals_the_adapter_including_the_first() {
        let mut listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tracker = LiveTracker::default();
        let supervisor = Supervisor::new(
            ServerClient::new(format!("http://{addr}")),
            "machine-key".to_string(),
            vec![Box::new(TrackedFactory { tracker: tracker.clone() })],
        );
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();

        for expected in 1..=3 {
            let server = tokio::spawn(async move {
                serve_one_reconcile(&listener).await;
                listener
            });
            supervisor
                .run_once(shutdown.clone(), &mut running, &event_tx, &mut event_rx)
                .await
                .unwrap();
            listener = server.await.unwrap();
            wait_until(|| tracker.connects() == expected).await;
        }
        assert_eq!(tracker.builds(), 1, "three connects, one adapter build");
        assert_eq!(tracker.connects(), 3, "one signal per connection, no more");
    }

    #[tokio::test]
    async fn dropping_a_running_adapter_cancels_it() {
        let (supervisor, tracker) = tracked();
        let shutdown = CancellationToken::new();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let mut running: std::collections::HashMap<String, AdapterRunning> =
            std::collections::HashMap::new();

        supervisor.reconcile(vec![cfg("bg")], &mut running, &event_tx, &shutdown);
        let token = running.get("stub").expect("adapter running").shutdown.clone();
        wait_until(|| tracker.live() == 1).await;

        drop(running);
        assert!(token.is_cancelled(), "dropping the map must cancel, not orphan, the adapter");
        wait_until(|| tracker.live() == 0).await;
    }
}
