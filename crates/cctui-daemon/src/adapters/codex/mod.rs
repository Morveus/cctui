//! Codex adapter.
//!
//! Two modes, picked at start by config or env:
//!
//! - **Log-tail (default)** — watches `~/.codex/sessions/` for
//!   new log files, emits `SessionStarted`/`Message`/`ToolUse`/
//!   `SessionEnded` based on file activity and a configurable quiesce
//!   window. Sessions root and timing knobs are tunable via the
//!   `adapters_enabled.config` JSON.
//! - **UDS injection (legacy v0)** — listens on
//!   `$CCTUI_CODEX_SOCK` (or `$XDG_RUNTIME_DIR/cctui-codex.sock`) and
//!   forwards line-delimited `AdapterEvent` JSON. Kept for tests and
//!   for tools that want to push events directly. Enable with
//!   `config.mode = "uds"`.
//!
//! Same shape as the claude-code adapter: listens on a dedicated Unix
//! domain socket and forwards line-delimited [`AdapterEvent`] JSON to the
//! daemon. Proves the `Adapter` trait holds for a second harness.
//!
//! Runs on every machine by default ([`cctui_proto::adapter::KNOWN_ADAPTERS`]);
//! an `adapters_enabled` row can disable it per machine.
//!
//! Socket path: `$CCTUI_CODEX_SOCK`, defaulting to
//! `$XDG_RUNTIME_DIR/cctui-codex.sock`.

pub(crate) mod app_server;
mod contract;
pub mod daemon;
mod log_tail;
mod model_list;
mod persist;
mod pty_view;
mod thread_list;
pub mod thread_read;

use std::path::PathBuf;

use cctui_proto::adapter::{AdapterCommand, AdapterEvent};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::adapter_runtime::{Adapter, AdapterCtx, AdapterFactory};
use crate::client::ServerClient;
use app_server::{
    AppServerConfig, CodexLiveSnapshot, CodexSession, LiveSessionRegistry, RouteAction,
    SessionCommand, SessionRegistry, route_or_prepare_resume, spawn_resumed_session,
};
use cctui_proto::diagnose::{
    CodexDiagnose, DiagnoseFact, EffectiveState, GatewayStatus, SessionDiagnose,
};

/// Served settings are the `service_tier` source when the spawn spec carries
/// none. Fail-closed on a missing/partial gateway env for an account-bound
/// session (see [`crate::adapters::gateway_env`]).
async fn resolve_launch(
    server: Option<&ServerClient>,
    machine_key: Option<&String>,
    local_id: &str,
    hint: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<crate::adapters::gateway_env::LaunchEnv> {
    crate::adapters::gateway_env::resolve_launch(
        "codex",
        server,
        machine_key,
        local_id,
        hint,
        crate::adapters::gateway_env::OPENAI_GATEWAY_KEYS,
    )
    .await
}

/// The tier for a codex launch: the spawn spec's resolved value, else the one
/// the gateway-env pull served. `None` on both leaves codex's own default.
fn spec_service_tier(
    spec_tier: Option<&str>,
    settings: Option<&serde_json::Value>,
) -> Option<String> {
    app_server::normalize_service_tier(spec_tier)
        .or_else(|| app_server::service_tier_from_settings(settings))
}

fn uses_uds_mode(config: &serde_json::Value) -> bool {
    config.get("mode").and_then(|v| v.as_str()) == Some("uds")
}

pub struct CodexAdapter;

#[async_trait::async_trait]
impl Adapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    async fn start(&self, ctx: AdapterCtx) -> anyhow::Result<()> {
        if !uses_uds_mode(&ctx.config) {
            return run_default(ctx).await;
        }
        let path = resolve_socket_path(&ctx.config);
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
        tracing::info!(socket = %path.display(), "codex adapter listening");

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
                        if let Err(err) = handle_connection(stream, events).await {
                            tracing::warn!(%err, "codex uds connection error");
                        }
                    });
                }
            }
        }
    }
}

/// Default mode (+): the passive log-tail observes sessions
/// started outside cctui, while the app-server command pump drives sessions
/// that cctui spawns. They share a [`SessionRegistry`] so the log-tail skips
/// rollout files an app-server session already owns (no double-ingest).
async fn run_default(ctx: AdapterCtx) -> anyhow::Result<()> {
    let app_cfg = AppServerConfig::from_value(&ctx.config);
    let registry: SessionRegistry = SessionRegistry::default();
    let live: LiveSessionRegistry = LiveSessionRegistry::default();

    let mut log = log_tail::LogTail::new(
        log_tail::LogTailConfig::from_value(&ctx.config),
        ctx.events.clone(),
        ctx.shutdown.clone(),
    );
    log.set_owned(registry.clone());
    let marks: log_tail::ResumeMarks = log_tail::ResumeMarks::default();
    log.set_resume_marks(marks.clone());
    let served = thread_read::ServedIds::default();
    log.set_served(served.clone());

    // poll `codex app-server`'s state-DB-backed `thread/list` for a
    // first-class inventory of EVERY machine session (cli/vscode/exec/
    // appServer) with real preview/name/cwd/status — the parity-with-claude
    // upgrade over the log-tail's heuristic JSONL scrape. Shares the
    // app-server `registry` so cctui-driven threads aren't double-emitted.
    // Falls back silently to log-tail-only when the poll can't run (codex
    // missing, sandbox/userns, auth). Disable with `inventory = false`.
    // before driving any commands, rediscover the codex threads cctui
    // itself owned before this daemon (re)started — a self-update / release
    // rollout restarts the daemon and drops the in-memory registry, leaving
    // in-flight `appServer`-source threads unrevivable. Seeding the durable
    // registry from `thread/list` lets the next reply/rename/set-model resume
    // them via `thread/resume`, mirroring the claude-code backfill/reconnect.
    let shared = daemon::SharedDaemon::new(app_cfg.bin.clone(), ctx.shutdown.clone());

    let restored = persist::load(&registry).await;
    if restored > 0 {
        tracing::info!(restored, "codex: session registry restored from state file");
    }
    if thread_list::ThreadListConfig::enabled(&ctx.config) {
        let cfg = thread_list::ThreadListConfig::from_value(&ctx.config);
        thread_list::rediscover_owned(&cfg, Some(&shared), &registry).await;
    }
    persist::save(&registry).await;

    let inventory_handle = if thread_list::ThreadListConfig::enabled(&ctx.config) {
        // the inventory's `seen` set is its own dedup state only — it
        // is no longer shared with the log-tail to suppress rollout files, so a
        // discovered CLI session still gets its real transcript backfilled.
        let seen = thread_list::SeenIds::default();
        let inv = thread_list::ThreadListInventory::new(
            thread_list::ThreadListConfig::from_value(&ctx.config),
            ctx.events.clone(),
            ctx.shutdown.clone(),
            registry.clone(),
            seen,
            served,
            Some(shared.clone()),
        );
        Some(tokio::spawn(inv.run()))
    } else {
        None
    };

    let log_handle = tokio::spawn(log.run());

    let pump = command_pump(
        ctx.commands,
        ctx.events.clone(),
        live,
        registry,
        app_cfg,
        ctx.shutdown,
        ctx.server,
        ctx.machine_key,
        marks,
        shared,
    );
    pump.await;
    log_handle.abort();
    if let Some(h) = inventory_handle {
        h.abort();
    }
    Ok(())
}

/// Route adapter commands. `Spawn` launches a new `codex app-server`-driven
/// session; the rest are forwarded to the owning session task by `local_id`
/// via the shared registry.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines, clippy::too_many_arguments)]
async fn command_pump(
    mut commands: mpsc::Receiver<AdapterCommand>,
    events: mpsc::Sender<AdapterEvent>,
    live: LiveSessionRegistry,
    registry: SessionRegistry,
    app_cfg: AppServerConfig,
    shutdown: tokio_util::sync::CancellationToken,
    server: Option<ServerClient>,
    machine_key: Option<String>,
    marks: log_tail::ResumeMarks,
    shared: daemon::SharedDaemon,
) {
    let pty_views = pty_view::RingViewManager::default();
    loop {
        tokio::select! {
                   () = shutdown.cancelled() => return,
                   cmd = commands.recv() => {
                       let Some(cmd) = cmd else { return };
                       let cmd_id = cmd.command_id();
                       match cmd {
                           // codex mints its own thread id, so the server-pre-minted
                           // `session_id` is ignored here.
                           AdapterCommand::Spawn { spec, command_id, session_id } => {
                               let Some(working_dir) = spec.working_dir.clone() else {
                                   tracing::error!("codex spawn: working_dir required");
                                   if let Some(command_id) = command_id {
                                       let _ = events
                                           .send(AdapterEvent::CommandResult {
                                               command_id,
                                               ok: false,
                                               error: Some("working_dir required".to_owned()),
                                           })
                                           .await;
                                   }
                                   continue;
                               };
        // pull the launch-time gateway env from
                               // the server's durable binding, keyed by the id the
                               // server bound the gateway token to — the pre-minted
                               // session id when present, else `command_id` (codex mints
                               // its own thread id, so the server keys its token on
                               // command_id, spawn.rs). Never pull with an empty id (it
                               // would hit `/sessions//gateway-env` and never match).
                               // Merge over the carried `spec.env`. Fail-closed: an
                               // account-bound
                               // session with empty gateway env refuses to launch
                               // rather than starting env-less and 401ing.
                               let launch_key = session_id
                                   .or(command_id)
                                   .map_or_else(String::new, |id| id.to_string());
                               let launch = match resolve_launch(
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &launch_key,
                                   &spec.env,
                               )
                               .await
                               {
                                   Ok(launch) => launch,
                                   Err(err) => {
                                       tracing::error!(%err, "codex spawn: refusing env-less launch");
                                       if let Some(command_id) = command_id {
                                           let _ = events
                                               .send(AdapterEvent::CommandResult {
                                                   command_id,
                                                   ok: false,
                                                   error: Some(err.to_string()),
                                               })
                                               .await;
                                       }
                                       continue;
                                   }
                               };
                               let served_settings = launch.settings.clone();
                               let env = launch.env.clone();
                               // The CommandResult for `command_id` is deferred to the
                               // session driver: it reports ok only after
                               // `thread/start` succeeds.
                               // Per-spawn permission posture: override the
                               // host default sandbox_mode + approval_policy. None →
                               // keep the daemon.toml defaults (which a no-userns host
                               // sets to full-access). `auto` keeps the workspace
                               // sandbox but disables approval prompts (approval=never).
                               let mut cfg = app_cfg.clone();
                               if let Some(mode) = spec.permission_mode {
                                   let (sandbox, approval) = mode.codex_sandbox_approval();
                                   sandbox.clone_into(&mut cfg.sandbox_mode);
                                   approval.clone_into(&mut cfg.approval_policy);
                               }
                               // Per-spawn reasoning effort (codex: low/medium/high/xhigh).
                               if let Some(effort) =
                                   spec.effort.as_deref().map(str::trim).filter(|e| !e.is_empty())
                               {
                                   cfg.reasoning_effort = Some(effort.to_owned());
                               }
                               // Per-spawn model family.
                               if let Some(model) =
                                   spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty())
                               {
                                   cfg.model = Some(model.to_owned());
                               }
                               cfg.service_tier = spec_service_tier(
                                   spec.service_tier.as_deref(),
                                   served_settings.as_ref(),
                               );
                               // Stage spawn attachments. A staging failure is
                               // fatal to the spawn — silently dropping a file the user
                               // expects the session to read is the P0 bug this fixes.
                               // Keyed by the same id the gateway env used so the staging
                               // dir is stable across the session lifetime.
                               let attachments = match crate::adapters::uploads::stage_bootstrap(
                                   &launch_key,
                                   &spec.bootstrap,
                               ) {
                                   Ok(paths) => paths,
                                   Err(err) => {
                                       tracing::error!(%err, "codex spawn: attachment staging failed");
                                       if let Some(command_id) = command_id {
                                           let _ = events
                                               .send(AdapterEvent::CommandResult {
                                                   command_id,
                                                   ok: false,
                                                   error: Some(format!("attachment staging failed: {err}")),
                                               })
                                               .await;
                                       }
                                       continue;
                                   }
                               };
                               let session = CodexSession::new_fresh(
                                   cfg,
                                   working_dir,
                                   env,
                                   spec.prompt.clone(),
                                   spec.name.clone(),
                                   attachments,
                                   command_id,
                                   session_id.map(|id| id.to_string()),
                                   spec.parent_local_id.clone(),
                                   events.clone(),
                                   live.clone(),
                                   registry.clone(),
                                   shutdown.clone(),
                               )
                               .with_agent_mcp(
                                   crate::adapters::agent_mcp::AgentMcp::for_capability(
                                       &launch_key,
                                       launch.spawn_capability.as_ref(),
                                   ),
                               );
                               tokio::spawn(async move {
                                   if let Err(err) = session.run().await {
                                       tracing::error!(%err, "codex app-server session ended in error");
                                   }
                               });
                           }
                           AdapterCommand::Fork { parent_local_id, spec, command_id, session_id, extract: _ } => {
                               // Fork an existing thread into a new one seeded from its
                               // history. Mirrors Spawn for cfg overrides
                               // (permission/effort/model) but launches via thread/fork.
                               let working_dir = spec
                                   .working_dir
                                   .clone()
                                   .unwrap_or_else(|| parent_local_id.clone());
                               // resolve gateway env keyed by the child
                               // session id the server pre-minted + bound the gateway
                               // token to (falling back to the parent thread id when
                               // absent), and fail closed on an account-bound fork with
                               // empty env — same contract as Spawn.
                               let (env, served_settings) = match resolve_launch(
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &session_id.clone().unwrap_or_else(|| parent_local_id.clone()),
                                   &spec.env,
                               )
                               .await
                               {
                                   Ok(launch) => (launch.env, launch.settings),
                                   Err(err) => {
                                       tracing::error!(%err, "codex fork: refusing env-less launch");
                                       if let Some(command_id) = command_id {
                                           let _ = events
                                               .send(AdapterEvent::CommandResult {
                                                   command_id,
                                                   ok: false,
                                                   error: Some(err.to_string()),
                                               })
                                               .await;
                                       }
                                       continue;
                                   }
                               };
                               let mut cfg = app_cfg.clone();
                               if let Some(mode) = spec.permission_mode {
                                   let (sandbox, approval) = mode.codex_sandbox_approval();
                                   sandbox.clone_into(&mut cfg.sandbox_mode);
                                   approval.clone_into(&mut cfg.approval_policy);
                               }
                               if let Some(effort) =
                                   spec.effort.as_deref().map(str::trim).filter(|e| !e.is_empty())
                               {
                                   cfg.reasoning_effort = Some(effort.to_owned());
                               }
                               if let Some(model) =
                                   spec.model.as_deref().map(str::trim).filter(|m| !m.is_empty())
                               {
                                   cfg.model = Some(model.to_owned());
                               }
                               cfg.service_tier = spec_service_tier(
                                   spec.service_tier.as_deref(),
                                   served_settings.as_ref(),
                               );
                               // Stage fork attachments, fatal on failure — same
                               // contract as spawn.
                               let stage_id = session_id
                                   .clone()
                                   .unwrap_or_else(|| parent_local_id.clone());
                               let attachments = match crate::adapters::uploads::stage_bootstrap(
                                   &stage_id,
                                   &spec.bootstrap,
                               ) {
                                   Ok(paths) => paths,
                                   Err(err) => {
                                       tracing::error!(%err, "codex fork: attachment staging failed");
                                       if let Some(command_id) = command_id {
                                           let _ = events
                                               .send(AdapterEvent::CommandResult {
                                                   command_id,
                                                   ok: false,
                                                   error: Some(format!("attachment staging failed: {err}")),
                                               })
                                               .await;
                                       }
                                       continue;
                                   }
                               };
                               let session = CodexSession::new_fork(
                                   cfg,
                                   working_dir,
                                   env,
                                   parent_local_id,
                                   spec.prompt.clone(),
                                   spec.name.clone(),
                                   attachments,
                                   command_id,
                                   events.clone(),
                                   live.clone(),
                                   registry.clone(),
                                   shutdown.clone(),
                               );
                               tokio::spawn(async move {
                                   if let Err(err) = session.run().await {
                                       tracing::error!(%err, "codex app-server fork ended in error");
                                   }
                               });
                           }
                           AdapterCommand::PermissionResponse { local_id, request_id, allow } => {
                               forward(
                                   &live,
                                   &registry,
                                   &events,
                                   &shutdown,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &app_cfg,
                                   &local_id,
                                   SessionCommand::Permission { request_id, allow },
                               )
                                   .await;
                           }
                           AdapterCommand::SendMessage { local_id, text }
                           | AdapterCommand::Reply { local_id, text, .. } => {
                               forward(
                                   &live,
                                   &registry,
                                   &events,
                                   &shutdown,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &app_cfg,
                                   &local_id,
                                   SessionCommand::Send { text, command_id: cmd_id },
                               )
                               .await;
                           }
                           AdapterCommand::Kill { local_id, signal } => {
                               forward(
                                   &live,
                                   &registry,
                                   &events,
                                   &shutdown,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &app_cfg,
                                   &local_id,
                                   SessionCommand::Kill { signal },
                               )
                               .await;
                           }
                           AdapterCommand::Interrupt { local_id, command_id } => {
                               // `turn/interrupt` only makes sense for a LIVE session
                               // (a hibernated thread has no in-flight turn to abort).
                               // When delivered, the session driver answers
                               // `command_id` from the correlated `turn/interrupt`
                               // JSON-RPC outcome; a non-delivery is
                               // reported as a failure here so the webui can say so.
                               let delivered = matches!(
                                   route_or_prepare_resume(
                                       &live,
                                       &registry,
                                       &local_id,
                                       SessionCommand::Interrupt { command_id },
                                   )
                                   .await,
                                   RouteAction::Delivered
                               );
                               if !delivered {
                                   if let Some(command_id) = command_id {
                                       let _ = events
                                           .send(AdapterEvent::CommandResult {
                                               command_id,
                                               ok: false,
                                               error: Some(
                                                   "no live codex session to interrupt".to_owned(),
                                               ),
                                           })
                                           .await;
                                   }
                                   tracing::warn!(%local_id, "codex: interrupt for non-live session");
                               }
                           }
                           AdapterCommand::Rename { local_id, name } => {
                               forward(
                                   &live,
                                   &registry,
                                   &events,
                                   &shutdown,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &app_cfg,
                                   &local_id,
                                   SessionCommand::Rename { name },
                               )
                               .await;
                           }
                           AdapterCommand::Remove { local_id, .. } => {
                               // Stop the live worker, drop the durable record, then
                               // archive the thread natively so it disappears
                               // from codex's own views too — the analogue of claude's
                               // `claude rm`, keeping the transcript recoverable.
                               // Idempotent: archiving an already-archived / missing
                               // thread succeeds. Runs off the pump so a 30s app-server
                               // spawn can't stall other commands.
                               forward(
                                   &live,
                                   &registry,
                                   &events,
                                   &shutdown,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &app_cfg,
                                   &local_id,
                                   SessionCommand::Kill { signal: None },
                               )
                               .await;
                               registry.lock().await.remove(&local_id);
                               persist::save(&registry).await;
                               let cfg = app_cfg.clone();
                               let shared_for_op = shared.clone();
                               tokio::spawn(async move {
                                   if let Err(err) = app_server::run_thread_lifecycle(
                                       &cfg,
                                       Some(&shared_for_op),
                                       &local_id,
                                       app_server::LifecycleOp::Archive,
                                   )
                                   .await
                                   {
                                       tracing::warn!(%local_id, %err, "codex: native thread/archive failed");
                                   }
                               });
                           }
                           AdapterCommand::Resume { local_id, .. } => {
                               // Reopen the thread natively: un-archive it so
                               // it reappears in codex's own views. Idempotent —
                               // unarchiving a non-archived / missing thread succeeds.
                               // cctui-side revival stays lazy: the next message resumes
                               // the hibernated app-server via the registry.
                               let cfg = app_cfg.clone();
                               let shared_for_op = shared.clone();
                               tokio::spawn(async move {
                                   if let Err(err) = app_server::run_thread_lifecycle(
                                       &cfg,
                                       Some(&shared_for_op),
                                       &local_id,
                                       app_server::LifecycleOp::Unarchive,
                                   )
                                   .await
                                   {
                                       tracing::warn!(%local_id, %err, "codex: native thread/unarchive failed");
                                   }
                               });
                           }
                           AdapterCommand::SetModel { local_id, model, effort, command_id } => {
                               forward(
                                   &live,
                                   &registry,
                                   &events,
                                   &shutdown,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &app_cfg,
                                   &local_id,
                                   SessionCommand::SetModel { model, effort, command_id },
                               )
                               .await;
                           }
                           AdapterCommand::Diagnose { local_id, request_id } => {
                               let report = build_diagnose(
                                   &live,
                                   &registry,
                                   server.as_ref(),
                                   machine_key.as_ref(),
                                   &local_id,
                               )
                               .await;
                               let _ = events
                                   .send(AdapterEvent::Diagnose {
                                       local_id,
                                       request_id,
                                       report: Box::new(report),
                                   })
                                   .await;
                           }
                           AdapterCommand::ResumeMarks { marks: session_marks } => {
                               // the tail needs the marks before it adopts a rollout:
                               // they are the only evidence of where the server's copy
                               // of the transcript actually stops.
                               if let Ok(mut store) = marks.lock() {
                                   store.extend(session_marks.iter().cloned());
                               }
                               announce_resume_marks(&registry, &events, &session_marks).await;
                           }
                           AdapterCommand::WatchPty { local_id, watch } => {
                               if watch {
                                   pty_views.watch(
                                       local_id,
                                       live.clone(),
                                       events.clone(),
                                       &shutdown,
                                   );
                               } else {
                                   pty_views.unwatch(&local_id);
                               }
                           }
                           _ => tracing::warn!("codex: unhandled AdapterCommand variant"),
                       }
                   }
               }
    }
}

/// Re-announce the cctui-owned threads the server still believes are live:
/// only a `SessionStarted` reverts `daemon_lost`, and owned threads have no
/// other event source after a reconnect (the inventory and the log-tail both
/// skip them). The source must stay `codex-app-server` — the server merges
/// metadata, so a different value would downgrade the live driver's.
async fn announce_resume_marks(
    registry: &SessionRegistry,
    events: &mpsc::Sender<AdapterEvent>,
    marks: &[(String, u64)],
) {
    let known: Vec<(String, String)> = {
        let guard = registry.lock().await;
        marks
            .iter()
            .filter_map(|(local_id, _)| {
                guard.get(local_id).map(|r| (local_id.clone(), r.cwd.clone()))
            })
            .collect()
    };
    for (local_id, cwd) in known {
        events
            .send(AdapterEvent::SessionStarted {
                local_id,
                meta: cctui_proto::adapter::SessionMeta {
                    working_dir: Some(cwd),
                    parent_local_id: None,
                    extra: serde_json::json!({ "source": "codex-app-server" }),
                },
            })
            .await
            .ok();
    }
}

/// Assemble the adapter-neutral diagnose report for a codex session:
/// the claude-only facts come back `missing`, and the codex section carries the
/// app-server / thread / rpc / rollout state. Gathered from the live driver
/// (via a `SessionCommand::Diagnose` round-trip) when one is running, else from
/// the durable registry record for a hibernated thread.
async fn build_diagnose(
    live: &LiveSessionRegistry,
    registry: &SessionRegistry,
    server: Option<&ServerClient>,
    machine_key: Option<&String>,
    local_id: &str,
) -> SessionDiagnose {
    let now_ms = now_unix_ms();
    let live_present = live.lock().await.contains_key(local_id);
    let record = registry.lock().await.get(local_id).cloned();
    let registered = record.is_some();

    let snapshot = if live_present { request_live_snapshot(live, local_id).await } else { None };

    let has_turn = snapshot.as_ref().and_then(|s| s.active_turn_id.as_ref()).is_some();
    let (verdict, state) = if live_present {
        if has_turn { ("active/working", "working") } else { ("idle", "idle") }
    } else if registered {
        ("hibernated", "hibernated")
    } else {
        ("unknown session", "unknown")
    };
    let effective_state = DiagnoseFact::fresh(
        EffectiveState {
            verdict: verdict.to_owned(),
            tempo: None,
            state: Some(state.to_owned()),
            detail: None,
            activity: None,
        },
        "codex-adapter",
        now_ms,
    );

    let auth_state = record.as_ref().map(|r| {
        if r.env.keys().any(|k| k == "OPENAI_BASE_URL" || k == "OPENAI_API_KEY") {
            "gateway env present".to_owned()
        } else {
            "no gateway env (default upstream)".to_owned()
        }
    });
    let registry_live_mismatch = (live_present && !registered)
        .then(|| "live command channel exists but no durable registry record".to_owned());

    let codex = CodexDiagnose {
        codex_version: snapshot.as_ref().and_then(|s| s.codex_version.clone()),
        min_version: contract::CODEX_MIN_VERSION.to_owned(),
        version_supported: snapshot
            .as_ref()
            .and_then(|s| s.codex_version.as_deref())
            .map(contract::version_supported),
        transport: "stdio".to_owned(),
        app_server_pid: snapshot.as_ref().and_then(|s| s.pid),
        live: live_present,
        registered,
        thread_id: Some(local_id.to_owned()),
        active_turn_id: snapshot.as_ref().and_then(|s| s.active_turn_id.clone()),
        turn_status: if has_turn { "working".to_owned() } else { "idle".to_owned() },
        pending_rpc_count: snapshot
            .as_ref()
            .map_or(0, |s| u32::try_from(s.pending_rpc_methods.len()).unwrap_or(u32::MAX)),
        pending_rpc_methods: snapshot
            .as_ref()
            .map(|s| s.pending_rpc_methods.clone())
            .unwrap_or_default(),
        protocol_errors: snapshot.as_ref().map(|s| s.protocol_errors.clone()).unwrap_or_default(),
        stderr_tail: snapshot.as_ref().map(|s| s.stderr_tail.clone()).unwrap_or_default(),
        rpc_tail: snapshot.as_ref().map(|s| s.rpc_tail.clone()).unwrap_or_default(),
        rollout_path: snapshot.as_ref().and_then(|s| s.rollout_path.clone()),
        rollout_size_bytes: snapshot.as_ref().and_then(|s| s.rollout_size_bytes),
        auth_state,
        registry_live_mismatch,
    };

    let gateway = DiagnoseFact::fresh(
        GatewayStatus { server_configured: server.is_some() && machine_key.is_some() },
        "daemon-config",
        now_ms,
    );

    SessionDiagnose {
        local_id: local_id.to_owned(),
        short: None,
        generated_at_ms: now_ms,
        adapter: "codex".to_owned(),
        effective_state,
        last_hook_event: na(),
        attach: na(),
        pty_output: na(),
        claude_socket: na(),
        transcript: na(),
        prompts: na(),
        permission_mode: na(),
        dispatch: na(),
        gateway,
        codex: Some(codex),
    }
}

/// A claude-only fact rendered not-applicable for a codex session.
fn na<T>() -> DiagnoseFact<T> {
    DiagnoseFact::missing("codex", "claude-only fact; see the codex section")
}

/// Round-trip a `SessionCommand::Diagnose` to the live session driver, bounded
/// so a wedged session can't stall the report.
async fn request_live_snapshot(
    live: &LiveSessionRegistry,
    local_id: &str,
) -> Option<CodexLiveSnapshot> {
    let sender = live.lock().await.get(local_id).cloned()?;
    let (tx, mut rx) = mpsc::channel(1);
    if sender.send(SessionCommand::Diagnose { reply: tx }).await.is_err() {
        return None;
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.ok().flatten()
}

fn now_unix_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

enum DispatchOutcome {
    Handled(bool),
    Missing,
}

/// Whether a resume must re-pull the gateway env. A stored credential means the
/// launch already resolved one, so pulling again would be redundant; a restored
/// record keeps its base URL, so emptiness alone does not answer this.
fn resume_needs_env_pull(env: &std::collections::BTreeMap<String, String>) -> bool {
    !env.contains_key("OPENAI_API_KEY")
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    live: &LiveSessionRegistry,
    registry: &SessionRegistry,
    events: &mpsc::Sender<AdapterEvent>,
    shutdown: &tokio_util::sync::CancellationToken,
    server: Option<&ServerClient>,
    machine_key: Option<&String>,
    local_id: &str,
    cmd: SessionCommand,
) -> DispatchOutcome {
    match route_or_prepare_resume(live, registry, local_id, cmd).await {
        RouteAction::Delivered => DispatchOutcome::Handled(true),
        RouteAction::Resume { mut record, command } if command.is_resumable() => {
            tracing::info!(%local_id, ?command, "codex: resuming hibernated app-server session");
            // A thread rediscovered from `thread/list` is seeded env-less, so its
            // first resume would 401 for an account-bound session; re-pull under
            // the same fail-closed contract as spawn/fork.
            if resume_needs_env_pull(&record.env) {
                match resolve_launch(server, machine_key, local_id, &record.env).await {
                    Ok(launch) => {
                        let settings = launch.settings;
                        record.env = launch.env;
                        record.spawn_relay = record.spawn_relay
                            || launch.spawn_capability.as_ref().is_some_and(|c| !c.is_empty());
                        // A rediscovered thread has no cached tier; adopt the
                        // served one rather than resuming on codex's default.
                        record.cfg.service_tier =
                            record.cfg.service_tier.take().or_else(|| {
                                app_server::service_tier_from_settings(settings.as_ref())
                            });
                    }
                    Err(err) => {
                        tracing::error!(%local_id, %err, "codex resume: refusing env-less launch");
                        let _ = events.send(failed_status(local_id, &err.to_string())).await;
                        fail_command(events, &command, &err.to_string()).await;
                        return DispatchOutcome::Handled(false);
                    }
                }
            }
            spawn_resumed_session(
                record,
                local_id,
                vec![command],
                events.clone(),
                live.clone(),
                registry.clone(),
                shutdown.clone(),
            );
            DispatchOutcome::Handled(true)
        }
        RouteAction::Resume { command, .. } => {
            tracing::warn!(%local_id, ?command, "codex: command cannot be applied to hibernated session");
            fail_command(events, &command, "codex session is hibernated").await;
            if matches!(command, SessionCommand::Kill { .. }) {
                registry.lock().await.remove(local_id);
                persist::save(registry).await;
                let _ = events
                    .send(AdapterEvent::SessionEnded {
                        local_id: local_id.to_owned(),
                        reason: cctui_proto::adapter::EndReason::Killed,
                    })
                    .await;
            }
            DispatchOutcome::Handled(false)
        }
        RouteAction::Missing => DispatchOutcome::Missing,
    }
}

async fn fail_command(events: &mpsc::Sender<AdapterEvent>, cmd: &SessionCommand, error: &str) {
    if let Some(command_id) = cmd.command_id() {
        let _ = events
            .send(AdapterEvent::CommandResult {
                command_id,
                ok: false,
                error: Some(error.to_owned()),
            })
            .await;
    }
}

fn failed_status(local_id: &str, detail: &str) -> AdapterEvent {
    AdapterEvent::Status {
        local_id: local_id.to_owned(),
        tempo: None,
        state: Some("failed".to_owned()),
        detail: Some(detail.to_owned()),
        activity: Some("failure".to_owned()),
        name: None,
        intent: None,
        model: None,
        effort: None,
        permission_mode: None,
        children: vec![],
    }
}

/// Surface a command that could not be routed anywhere: a `CommandResult`
/// when it carries an id, `SessionEnded` for a kill of an already-gone
/// session, a failed `Status` otherwise — never a silent drop.
async fn emit_missing_failure(
    events: &mpsc::Sender<AdapterEvent>,
    local_id: &str,
    cmd: &SessionCommand,
) {
    tracing::warn!(%local_id, ?cmd, "codex: no app-server session for command");
    fail_command(events, cmd, "no codex session for command").await;
    if matches!(cmd, SessionCommand::Kill { .. }) {
        let _ = events
            .send(AdapterEvent::SessionEnded {
                local_id: local_id.to_owned(),
                reason: cctui_proto::adapter::EndReason::Killed,
            })
            .await;
        return;
    }
    let _ = events
        .send(failed_status(
            local_id,
            "codex session lost: no live app-server and no resumable thread record",
        ))
        .await;
}

/// Route a session command. When neither a live sender nor a durable record
/// exists, a resumable command triggers a bounded on-demand `thread/list`
/// probe to reconstruct the record before failing visibly.
#[allow(clippy::too_many_arguments)]
async fn forward(
    live: &LiveSessionRegistry,
    registry: &SessionRegistry,
    events: &mpsc::Sender<AdapterEvent>,
    shutdown: &tokio_util::sync::CancellationToken,
    server: Option<&ServerClient>,
    machine_key: Option<&String>,
    app_cfg: &AppServerConfig,
    local_id: &str,
    cmd: SessionCommand,
) -> bool {
    match dispatch(live, registry, events, shutdown, server, machine_key, local_id, cmd.clone())
        .await
    {
        DispatchOutcome::Handled(handled) => handled,
        DispatchOutcome::Missing if cmd.is_resumable() => {
            tracing::warn!(%local_id, "codex: session unknown; attempting on-demand thread recovery");
            let (live, registry, events, shutdown) =
                (live.clone(), registry.clone(), events.clone(), shutdown.clone());
            let (server, machine_key) = (server.cloned(), machine_key.cloned());
            let app = app_cfg.clone();
            let local_id = local_id.to_owned();
            tokio::spawn(async move {
                let Some(record) = thread_list::recover_record(&app, &local_id).await else {
                    emit_missing_failure(&events, &local_id, &cmd).await;
                    return;
                };
                registry.lock().await.entry(local_id.clone()).or_insert(record);
                persist::save(&registry).await;
                let handled = dispatch(
                    &live,
                    &registry,
                    &events,
                    &shutdown,
                    server.as_ref(),
                    machine_key.as_ref(),
                    &local_id,
                    cmd.clone(),
                )
                .await;
                if matches!(handled, DispatchOutcome::Missing) {
                    emit_missing_failure(&events, &local_id, &cmd).await;
                }
            });
            true
        }
        DispatchOutcome::Missing => {
            emit_missing_failure(events, local_id, &cmd).await;
            false
        }
    }
}

async fn handle_connection(
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
        match serde_json::from_str::<AdapterEvent>(line) {
            Ok(evt) => {
                if events.send(evt).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                tracing::warn!(%err, ?line, "ignoring non-AdapterEvent uds line");
            }
        }
    }
    Ok(())
}

fn resolve_socket_path(config: &serde_json::Value) -> PathBuf {
    if let Some(p) = config.get("socket_path").and_then(|v| v.as_str()) {
        return PathBuf::from(p);
    }
    if let Ok(p) = std::env::var("CCTUI_CODEX_SOCK") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("cctui-codex.sock")
}

pub struct CodexFactory;

impl AdapterFactory for CodexFactory {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn build(&self, _config: serde_json::Value) -> Box<dyn Adapter> {
        Box::new(CodexAdapter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cctui_proto::adapter::EndReason;
    use std::time::Duration;

    fn unrecoverable_cfg() -> AppServerConfig {
        AppServerConfig {
            bin: "/nonexistent/cctui-test-codex".to_owned(),
            ..AppServerConfig::default()
        }
    }

    async fn recv(rx: &mut mpsc::Receiver<AdapterEvent>) -> AdapterEvent {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("event before timeout")
            .expect("event channel open")
    }

    #[test]
    fn spawn_tier_prefers_the_spec_then_the_served_settings() {
        let served = serde_json::json!({"service_tier": "fast"});
        assert_eq!(spec_service_tier(Some("default"), Some(&served)).as_deref(), Some("default"));
        assert_eq!(spec_service_tier(None, Some(&served)).as_deref(), Some("fast"));
        assert_eq!(spec_service_tier(Some("bogus"), Some(&served)).as_deref(), Some("fast"));
        assert_eq!(spec_service_tier(None, None), None);
        assert_eq!(spec_service_tier(None, Some(&serde_json::json!({}))), None);
    }

    /// The relay fix must not have widened the resume into an unconditional
    /// capability pull: a record that already carries a credential still makes
    /// no gateway call.
    #[test]
    fn a_resume_with_a_stored_credential_still_does_not_re_pull_the_env() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("OPENAI_API_KEY".to_owned(), "sk-live".to_owned());
        assert!(!resume_needs_env_pull(&env), "the double-pull guard must still hold");
        env.insert("OPENAI_BASE_URL".to_owned(), "https://cctui/gw".to_owned());
        assert!(!resume_needs_env_pull(&env));

        assert!(resume_needs_env_pull(&std::collections::BTreeMap::new()));
        let restored: std::collections::BTreeMap<String, String> =
            std::iter::once(("OPENAI_BASE_URL".to_owned(), "https://cctui/gw".to_owned()))
                .collect();
        assert!(
            resume_needs_env_pull(&restored),
            "a restored record keeps its base URL but has no credential, so it must pull"
        );
    }

    #[tokio::test]
    async fn resume_marks_re_announce_owned_threads_only() {
        let registry = SessionRegistry::default();
        registry.lock().await.insert(
            "owned-thread".to_owned(),
            app_server::SessionRecord {
                cfg: AppServerConfig::default(),
                cwd: "/tmp/work".to_owned(),
                name: None,
                env: std::collections::BTreeMap::new(),
                spawn_relay: false,
            },
        );
        let (tx, mut rx) = mpsc::channel(8);
        announce_resume_marks(
            &registry,
            &tx,
            &[("owned-thread".to_owned(), 3), ("stranger".to_owned(), 7)],
        )
        .await;
        drop(tx);

        match recv(&mut rx).await {
            AdapterEvent::SessionStarted { local_id, meta } => {
                assert_eq!(local_id, "owned-thread");
                assert_eq!(meta.working_dir.as_deref(), Some("/tmp/work"));
                assert_eq!(meta.extra["source"], "codex-app-server");
            }
            other => panic!("expected SessionStarted, got {other:?}"),
        }
        assert!(rx.recv().await.is_none(), "unknown ids must not be announced");
    }

    #[tokio::test]
    async fn missing_non_resumable_command_emits_failed_status() {
        let (tx, mut rx) = mpsc::channel(8);
        let handled = forward(
            &LiveSessionRegistry::default(),
            &SessionRegistry::default(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            None,
            None,
            &unrecoverable_cfg(),
            "ghost",
            SessionCommand::Permission { request_id: "r".to_owned(), allow: true },
        )
        .await;
        assert!(!handled);
        match recv(&mut rx).await {
            AdapterEvent::Status { local_id, state, detail, activity, .. } => {
                assert_eq!(local_id, "ghost");
                assert_eq!(state.as_deref(), Some("failed"));
                assert_eq!(activity.as_deref(), Some("failure"));
                assert!(detail.unwrap().contains("no live app-server"));
            }
            other => panic!("expected failed Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_kill_emits_session_ended() {
        let (tx, mut rx) = mpsc::channel(8);
        forward(
            &LiveSessionRegistry::default(),
            &SessionRegistry::default(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            None,
            None,
            &unrecoverable_cfg(),
            "ghost",
            SessionCommand::Kill { signal: None },
        )
        .await;
        match recv(&mut rx).await {
            AdapterEvent::SessionEnded { local_id, reason } => {
                assert_eq!(local_id, "ghost");
                assert!(matches!(reason, EndReason::Killed));
            }
            other => panic!("expected SessionEnded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_send_with_failed_recovery_emits_failed_status() {
        let (tx, mut rx) = mpsc::channel(8);
        let handled = forward(
            &LiveSessionRegistry::default(),
            &SessionRegistry::default(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            None,
            None,
            &unrecoverable_cfg(),
            "ghost",
            SessionCommand::Send { text: "hi".to_owned(), command_id: None },
        )
        .await;
        assert!(handled, "recovery is attempted asynchronously");
        match recv(&mut rx).await {
            AdapterEvent::Status { local_id, state, .. } => {
                assert_eq!(local_id, "ghost");
                assert_eq!(state.as_deref(), Some("failed"));
            }
            other => panic!("expected failed Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_set_model_failure_resolves_command_id() {
        let (tx, mut rx) = mpsc::channel(8);
        let command_id = uuid::Uuid::new_v4();
        forward(
            &LiveSessionRegistry::default(),
            &SessionRegistry::default(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
            None,
            None,
            &unrecoverable_cfg(),
            "ghost",
            SessionCommand::SetModel {
                model: Some("gpt-5-codex".to_owned()),
                effort: None,
                command_id: Some(command_id),
            },
        )
        .await;
        match recv(&mut rx).await {
            AdapterEvent::CommandResult { command_id: cid, ok, error } => {
                assert_eq!(cid, command_id);
                assert!(!ok);
                assert!(error.is_some());
            }
            other => panic!("expected CommandResult, got {other:?}"),
        }
        assert!(matches!(
            recv(&mut rx).await,
            AdapterEvent::Status { state: Some(s), .. } if s == "failed"
        ));
    }
}
