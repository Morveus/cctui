use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use cctui_daemon::client::{AuthRejected, ServerClient};
use cctui_daemon::config::Config;
use cctui_daemon::supervisor::Supervisor;
use cctui_daemon::{adapters, fatal, runlock, runtime, selfupdate, service};

#[derive(Parser)]
#[command(name = "cctui-daemon", about = "Per-machine agent supervisor for cctui", version)]
struct Cli {
    #[arg(long, env = "CCTUI_DAEMON_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Enrol a machine with a cctui-server. Without a target this machine:
    /// mint a machine key and write it to the local config file. With an
    /// `[user@]host` ssh target: one-shot remote install — push the
    /// right daemon binary through the server's release proxy, enroll, write
    /// the remote config, install + start the systemd user service, and wait
    /// until the machine shows connected in the fleet. Idempotent: re-running
    /// upgrades/repairs an existing install.
    Enroll {
        /// ssh target (`user@host` or an ssh-config alias) for remote
        /// enrolment. Omit to enroll this machine locally.
        ssh_target: Option<String>,
        #[arg(long)]
        server_url: String,
        #[arg(long)]
        token: String,
        /// Machine name. Required for local enrolment; defaults to the remote
        /// hostname for remote enrolment.
        #[arg(long)]
        name: Option<String>,
        /// Machine kind: `persistent` (default, a real dev machine) or
        /// `ephemeral` (a dispatch/worker pod — hidden from the New-session
        /// picker and reaped once stale;).
        #[arg(long, default_value = "persistent")]
        kind: String,
        /// Seconds to wait for the remote daemon to connect before failing
        /// the verification step (remote enrolment only).
        #[arg(long, default_value_t = 90)]
        verify_timeout_secs: u64,
    },
    /// Connect to the configured server and supervise adapters.
    Run {
        /// Disable the periodic auto-update poller. Equivalent to
        /// `CCTUI_DAEMON_AUTOUPDATE=0`.
        #[arg(long)]
        no_auto_update: bool,
    },
    /// Print the resolved configuration (`machine_key` redacted).
    Status,
    /// Internal: the Claude Code `AskUserQuestion` PreToolUse/PostToolUse hook
    /// command. Reads the hook JSON on stdin and forwards the pending
    /// question (or its resolution) to the running daemon over `--sock`.
    /// Observe-only: prints nothing and always exits 0.
    AskHook {
        /// Hook phase: `pre` (question appeared), `post` (answered), or `perm`
        /// (a tool-permission `PreToolUse` hook that blocks and long-polls the
        /// daemon for an allow/deny decision).
        #[arg(long)]
        event: String,
        /// Daemon socket to deliver to.
        #[arg(long)]
        sock: PathBuf,
        /// Whip mode (🐎): after forwarding the question for UI visibility,
        /// emit a `PreToolUse` deny decision so the form never renders and the
        /// model is told to decide and keep working.
        #[arg(long)]
        deny: bool,
    },
    /// Internal: the stdio MCP server exposing the `CctuiAgent` tool to a
    /// claude session. Registered by the per-session MCP config the daemon
    /// writes at launch; relays each tool call to the running daemon over
    /// `--sock`, which owns the spawn path.
    McpAgent {
        /// Session the tool call is made on behalf of. Fixed at launch so a
        /// session can never spawn as another.
        #[arg(long)]
        session: String,
        /// Daemon socket serving the agent tool.
        #[arg(long)]
        sock: PathBuf,
    },
    /// Internal: the Claude Code `SessionStart` hook that holds the first turn
    /// until the session's MCP relay has connected, so a prompt calling
    /// `CctuiAgent` on turn 1 does not race it. Always exits 0 — a relay that
    /// never connects costs the session a bounded wait, never its launch.
    McpWait {
        /// Session whose relay is awaited.
        #[arg(long)]
        session: String,
        /// Daemon socket serving the agent tool.
        #[arg(long)]
        sock: PathBuf,
        /// Seconds to wait before releasing the turn anyway.
        #[arg(long, default_value_t = 8)]
        timeout: u64,
    },
    /// Internal: the Claude Code `Stop` hook for whip mode (🐎). Reads
    /// the hook JSON on stdin; exits 2 with guidance on stderr when the final
    /// message reads as a graceful early exit / hand-back, else exits 0.
    WhipStopHook {
        /// Per-session whip phrase override file written by the daemon
        /// at spawn. Absent/unreadable → the compiled default phrase list.
        #[arg(long)]
        phrases: Option<PathBuf>,
    },
    /// Check for a newer release, swap the binary in place, and restart
    /// the daemon service (if one is running) so it picks up the new binary.
    Update,
    /// Install / uninstall the cctui-daemon systemd user service.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Write the user-systemd unit, reload, enable, and start it.
    Install,
    /// Stop + disable the unit and remove the user-systemd file.
    Uninstall,
    /// Restart the running cctui-daemon service.
    Restart,
    /// Show the service manager status and the most recent daemon logs.
    Status,
    /// Print the embedded unit content to stdout (for manual install).
    Unit,
}

/// Print resolved config plus the version split: the binary this CLI is, and
/// the version of the service actually running (from the runtime state file).
fn print_status(path: &PathBuf) -> anyhow::Result<()> {
    if !Config::exists_at(path) {
        println!("config: {} (not found)", path.display());
        println!(
            "enrolled: no — run `cctui-daemon enroll --server-url <url> --token <token> --name <name>`"
        );
        println!("service: {}", if service::is_active() { "running" } else { "not running" });
        println!("binary version: {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let cfg = Config::load_from(path)?;
    println!("config: {}", path.display());
    println!("server_url: {}", cfg.server_url);
    if let Some(id) = cfg.machine_id {
        println!("machine_id: {id}");
    }
    println!("machine_key: <redacted>");
    println!("service: {}", if service::is_active() { "running" } else { "not running" });
    println!("binary version: {}", env!("CARGO_PKG_VERSION"));
    match runtime::read() {
        Some(rt) if runtime::pid_alive(rt.pid) => {
            println!("running version: {} (pid {}, since {})", rt.version, rt.pid, rt.started_at);
        }
        Some(rt) => println!(
            "running version: none — last run was {} (pid {}, since {}, no longer alive)",
            rt.version, rt.pid, rt.started_at
        ),
        None => println!("running version: unknown (daemon has not run on this machine)"),
    }
    match cctui_daemon::counters::read_snapshot() {
        Some(snap) => println!("{}", snap.render()),
        None => println!("bandwidth: unavailable (daemon has not reported yet)"),
    }
    Ok(())
}

fn auto_update_enabled(flag: bool) -> bool {
    if flag {
        return false;
    }
    !matches!(std::env::var("CCTUI_DAEMON_AUTOUPDATE").as_deref(), Ok("0" | "false"))
}

/// Same escalating schedule as the supervisor's WS reconnect loop, applied to
/// the pre-supervisor auth so a server outage / no-network boot never exits
/// the service into a launchd/systemd respawn loop.
const AUTH_BACKOFF_SECS: &[u64] = &[5, 10, 20, 60];

/// Run the long-lived daemon: authenticate, wire up the optional self-update
/// loop, then run the supervisor until shutdown (`Cmd::Run`).
async fn run_daemon(path: &std::path::Path, no_auto_update: bool) -> anyhow::Result<()> {
    let cfg = Config::load_or_env(&path.to_path_buf()).map_err(fatal::mark)?;
    let _run_lock = runlock::acquire()?;
    // Record this process as the running service so `status` /
    // `service status` can report the version actually serving.
    runtime::record();
    let counters = cctui_daemon::counters::BandwidthCounters::new();
    counters.persist();
    let client = ServerClient::new(&cfg.server_url).with_counters(counters.clone());
    let mut attempt = 0usize;
    let auth = loop {
        match client.daemon_auth(&cfg.machine_key).await {
            Ok(auth) => break auth,
            Err(err) if err.downcast_ref::<AuthRejected>().is_some() => {
                return Err(fatal::mark(err));
            }
            Err(err) => {
                let delay = AUTH_BACKOFF_SECS[attempt.min(AUTH_BACKOFF_SECS.len() - 1)];
                tracing::warn!(%err, retry_in_secs = delay, "daemon_auth failed; retrying");
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
        }
    };
    tracing::info!(machine_id = %auth.machine_id, user_id = %auth.user_id, "authenticated");
    // Captured for the self-update loop before `machine_key` is moved
    // into the supervisor; both flow to the server-routed updater.
    let update_server_url = cfg.server_url.clone();
    let update_machine_key = cfg.machine_key.clone();
    let supervisor = Supervisor::new(client, cfg.machine_key, adapters::registry());
    let shutdown = CancellationToken::new();
    // SIGTERM must reach the graceful path, not just Ctrl-C: a dispatched worker
    // is torn down with `kill`, and the default SIGTERM disposition would kill the
    // process before the transcript tail flushes.
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        wait_for_termination().await;
        signal_token.cancel();
    });
    if auto_update_enabled(no_auto_update) {
        let interval = selfupdate::poll_interval();
        tracing::info!(interval_secs = interval.as_secs(), "auto-update enabled");
        selfupdate::spawn_loop(
            shutdown.clone(),
            update_server_url,
            update_machine_key,
            interval,
            counters.clone(),
        );
    } else {
        tracing::info!("auto-update disabled");
    }
    cctui_daemon::harness_update::spawn_loop(shutdown.clone());
    supervisor.run(shutdown).await;
    Ok(())
}

/// Resolve the first of SIGINT (Ctrl-C) or SIGTERM (`kill`, pod teardown).
#[cfg(unix)]
async fn wait_for_termination() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "cannot install SIGTERM handler; Ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_termination() {
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                // `codex_app_server_stderr` is its own target, outside the
                // `cctui_daemon` tree, so journald keeps codex's stderr.
                "cctui_daemon=info,codex_app_server_stderr=info".into()
            },
        ))
        .init();

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(Config::default_path);

    match cli.cmd {
        Cmd::Enroll { ssh_target, server_url, token, name, kind, verify_timeout_secs } => {
            if let Some(ssh_target) = ssh_target {
                return cctui_daemon::enroll::run(cctui_daemon::enroll::RemoteEnrollOpts {
                    ssh_target,
                    server_url,
                    token,
                    name,
                    kind,
                    verify_timeout: std::time::Duration::from_secs(verify_timeout_secs),
                })
                .await;
            }
            let Some(name) = name else {
                anyhow::bail!("--name is required when enrolling this machine locally");
            };
            let client = ServerClient::new(&server_url);
            // Send `kind` only when non-default so older servers are unaffected.
            let kind_arg = (kind != "persistent").then_some(kind.as_str());
            let resp = client.enroll(&token, &name, kind_arg).await?;
            let cfg = Config {
                server_url,
                machine_key: resp.machine_key,
                machine_id: Some(resp.machine_id),
                read_file_roots: Config::load_from(&path)
                    .map(|old| old.read_file_roots)
                    .unwrap_or_default(),
            };
            cfg.save_to(&path)?;
            println!("enrolled as {} → {}", resp.machine_id, path.display());
            Ok(())
        }
        Cmd::Run { no_auto_update } => match run_daemon(&path, no_auto_update).await {
            Err(err) if fatal::is_config_fatal(&err) => {
                eprintln!("Error: {err:#}");
                std::process::exit(fatal::EXIT_CONFIG);
            }
            other => other,
        },
        Cmd::Update => {
            let cfg = Config::load_from(&path)?;
            match selfupdate::check_and_apply(&cfg.server_url, &cfg.machine_key).await {
                Ok(Some(_)) => {
                    // The running service is a separate process still on the
                    // old binary — restart it so the swap takes effect now.
                    match service::restart_if_active() {
                        Ok(true) => println!("cctui-daemon upgraded and service restarted"),
                        Ok(false) => println!(
                            "cctui-daemon upgraded; no running service found — \
                             start it (`cctui-daemon service install`) or restart to apply"
                        ),
                        Err(err) => println!(
                            "cctui-daemon upgraded, but restarting the service failed: {err}\n\
                             restart it manually (`cctui-daemon service restart`) to apply"
                        ),
                    }
                    Ok(())
                }
                Ok(None) => {
                    println!("cctui-daemon already on the latest release");
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
        Cmd::Service { cmd } => match cmd {
            ServiceCmd::Install => service::install(),
            ServiceCmd::Uninstall => service::uninstall(),
            ServiceCmd::Restart => service::restart(),
            ServiceCmd::Status => service::status(),
            ServiceCmd::Unit => {
                service::print_unit();
                Ok(())
            }
        },
        Cmd::Status => print_status(&path),
        Cmd::AskHook { event, sock, deny } => cctui_daemon::askhook::run(&event, &sock, deny),
        Cmd::McpAgent { session, sock } => cctui_daemon::mcp::run(&session, &sock),
        Cmd::McpWait { session, sock, timeout } => {
            cctui_daemon::mcp::wait_ready(&session, &sock, std::time::Duration::from_secs(timeout));
            Ok(())
        }
        Cmd::WhipStopHook { phrases } => {
            std::process::exit(cctui_daemon::whipstop::run(phrases.as_deref()))
        }
    }
}
