mod app;
mod client;
mod install;
mod selfupdate;
mod theme;
mod ui;
mod views;
mod widgets;

use std::io;
use std::time::Duration;

use anyhow::Result;
use app::{App, ConversationLine, LineKind, PendingPermission, View};
use cctui_proto::ws::{AgentEvent, ServerEvent, TuiCommand};
use client::ServerClient;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;
use tokio::time;

/// Display ordering for the classifier buckets in the session list:
/// sessions that want the user's eyes float to the top.
pub(crate) const fn bucket_rank(bucket: cctui_proto::classifier::Bucket) -> u8 {
    use cctui_proto::classifier::Bucket;
    match bucket {
        Bucket::Blocked => 0,
        Bucket::Review => 1,
        Bucket::Working => 2,
        Bucket::Done => 3,
    }
}

/// Uptime derived from `registered_at`; 0 when unset.
pub(crate) fn uptime_secs(s: &cctui_proto::api::SessionListItem) -> i64 {
    s.registered_at.map_or(0, |r| (chrono::Utc::now() - r).num_seconds())
}

/// Input event from the terminal: either a key press or mouse scroll.
#[derive(Debug, Clone)]
enum InputEvent {
    Key(KeyEvent),
    ScrollUp,
    ScrollDown,
}

/// Prefer `~/.config/cctui/user.json`; env vars still override so local dev
/// (e.g. `CCTUI_TOKEN=dev-admin`) keeps working.
fn resolve_identity() -> (String, String) {
    let identity = cctui_proto::identity::load_user();
    let default_url = identity
        .as_ref()
        .map_or_else(|| "http://localhost:8700".to_string(), |i| i.server_url.clone());
    let default_token = identity.map(|i| i.user_key).unwrap_or_default();
    let base_url = std::env::var("CCTUI_URL").unwrap_or(default_url);
    let token = std::env::var("CCTUI_TOKEN").unwrap_or(default_token);
    (base_url, token)
}

#[derive(clap::Parser)]
#[command(name = "cctui", version, about = "Claude Code Control TUI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Force re-download of the latest cctui release and re-apply settings.
    Update,
    /// One-call session diagnose: print everything the daemon knows
    /// about a session — each fact dated + sourced — plus the server-side
    /// gateway/account binding facts.
    Diagnose {
        /// The session id (as shown in the session list / URL).
        session_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    use clap::Parser;
    match Cli::parse().command {
        Some(Command::Update) => {
            let (base_url, _) = resolve_identity();
            selfupdate::force_update(&base_url).await
        }
        Some(Command::Diagnose { session_id }) => run_diagnose(&session_id).await,
        None => {
            let (base_url, _) = resolve_identity();
            selfupdate::maybe_update(&base_url).await;
            run_tui().await
        }
    }
}

/// `cctui diagnose <session-id>`: fetch the one-call diagnose blob
/// and render it as one dated, sourced line per fact.
async fn run_diagnose(session_id: &str) -> Result<()> {
    let (base_url, token) = resolve_identity();
    let server = ServerClient::new(&base_url, &token);
    let resp = server.diagnose_session(session_id).await?;

    println!("session {}", resp.session_id);
    let s = &resp.server;
    println!(
        "server: status={} adapter={} account_bound={} accounts=[{}] machine={} last_seen={}",
        s.status.as_deref().unwrap_or("?"),
        s.adapter_id.as_deref().unwrap_or("?"),
        s.account_bound,
        s.accounts.join(", "),
        s.machine_id.as_deref().unwrap_or("?"),
        s.machine_last_seen_ms.map_or_else(|| "?".to_owned(), fmt_age_since),
    );
    if let Some(err) = &resp.daemon_error {
        println!("daemon: UNAVAILABLE — {err}");
    }
    let Some(d) = &resp.daemon else { return Ok(()) };
    println!(
        "daemon report: adapter={} short={} generated_at={}",
        d.adapter,
        d.short.as_deref().unwrap_or("?"),
        d.generated_at_ms,
    );
    print_fact("effective_state", &d.effective_state);
    print_fact("last_hook_event", &d.last_hook_event);
    print_fact("attach", &d.attach);
    print_fact("pty_output", &d.pty_output);
    print_fact("claude_socket", &d.claude_socket);
    print_fact("transcript", &d.transcript);
    print_fact("prompts", &d.prompts);
    print_fact("permission_mode", &d.permission_mode);
    print_fact("dispatch", &d.dispatch);
    print_fact("gateway", &d.gateway);
    Ok(())
}

/// One `name [source, age]: value-or-reason` line per fact.
fn print_fact<T: serde::Serialize>(name: &str, fact: &cctui_proto::diagnose::DiagnoseFact<T>) {
    let age = fact.age_ms.map_or_else(|| "undated".to_owned(), fmt_age);
    match &fact.value {
        Some(v) => {
            let rendered = serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".into());
            println!("  {name} [{}, {age}]: {rendered}", fact.source);
        }
        None => println!(
            "  {name} [{}, {age}]: — ({})",
            fact.source,
            fact.missing_reason.as_deref().unwrap_or("missing"),
        ),
    }
}

fn fmt_age(ms: i64) -> String {
    match ms {
        ms if ms < 1_000 => format!("{ms}ms ago"),
        ms if ms < 60_000 => format!("{}s ago", ms / 1_000),
        ms if ms < 3_600_000 => format!("{}m ago", ms / 60_000),
        ms => format!("{}h ago", ms / 3_600_000),
    }
}

fn fmt_age_since(at_ms: i64) -> String {
    fmt_age((chrono::Utc::now().timestamp_millis() - at_ms).max(0))
}

async fn run_tui() -> Result<()> {
    let (base_url, token) = resolve_identity();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, base_url, token).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableMouseCapture, LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    base_url: String,
    token: String,
) -> Result<()> {
    let server = ServerClient::new(&base_url, &token);
    let mut app = App::new();

    init_sessions(&server, &mut app).await;
    let (mut cmd_tx, mut event_rx) = connect_ws_or_dummy(&server).await;
    let mut refresh_interval = time::interval(Duration::from_secs(5));
    refresh_interval.tick().await;
    let mut input_rx = spawn_input_task();
    // Backoff for WS reconnect attempts after the stream drops.
    let mut reconnect_backoff_secs: u64 = 1;
    let mut reconnect_timer: Option<std::pin::Pin<Box<time::Sleep>>> = None;

    loop {
        update_scroll_metrics(&mut app);
        terminal.draw(|f| render(f, &mut app))?;

        tokio::select! {
            biased;

            maybe_input = input_rx.recv() => {
                if let Some(input) = maybe_input {
                    handle_input(&mut app, input, &cmd_tx, &server).await;
                }
            }
            maybe_event = event_rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        handle_server_event(&mut app, event);
                        while let Ok(ev) = event_rx.try_recv() {
                            handle_server_event(&mut app, ev);
                        }
                    }
                    None if reconnect_timer.is_none() => {
                        // Stream dropped — schedule a reconnect attempt.
                        reconnect_timer = Some(Box::pin(time::sleep(Duration::from_secs(reconnect_backoff_secs))));
                    }
                    None => {
                        // Already waiting; yield so the timer branch can fire.
                        tokio::task::yield_now().await;
                    }
                }
            }
            () = async { reconnect_timer.as_mut().unwrap().await }, if reconnect_timer.is_some() => {
                reconnect_timer = None;
                if let Ok((new_tx, new_rx)) = server.connect_ws().await {
                    cmd_tx = new_tx;
                    event_rx = new_rx;
                    reconnect_backoff_secs = 1;
                    if matches!(app.view, View::Conversation)
                        && let Some(id) = app.selected_session().map(|s| s.id.clone()) {
                        let _ = cmd_tx.send(TuiCommand::Subscribe { session_id: id }).await;
                    }
                    refresh_sessions(&server, &mut app).await;
                } else {
                    reconnect_backoff_secs = (reconnect_backoff_secs * 2).min(30);
                    reconnect_timer = Some(Box::pin(time::sleep(Duration::from_secs(reconnect_backoff_secs))));
                }
            }
            _ = refresh_interval.tick() => {
                refresh_sessions(&server, &mut app).await;
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

async fn init_sessions(server: &ServerClient, app: &mut App) {
    if let Ok(resp) = server.list_sessions().await {
        app.sessions = resp.sessions;
        app.update_aggregates();
    }
}

async fn connect_ws_or_dummy(
    server: &ServerClient,
) -> (mpsc::Sender<TuiCommand>, mpsc::Receiver<ServerEvent>) {
    (server.connect_ws().await).unwrap_or_else(|_| {
        let (tx, _) = mpsc::channel::<TuiCommand>(1);
        let (_, rx) = mpsc::channel::<ServerEvent>(1);
        (tx, rx)
    })
}

async fn refresh_sessions(server: &ServerClient, app: &mut App) {
    if let Ok(resp) = server.list_sessions().await {
        app.sessions = resp.sessions;
        app.update_aggregates();
    }
}

/// Bootstrap `viewport_height` from terminal size if not yet set by a render pass.
fn update_scroll_metrics(app: &mut App) {
    if app.viewport_height == 0
        && let Ok((_, rows)) = crossterm::terminal::size()
    {
        app.viewport_height = (rows as usize).saturating_sub(5);
    }
}

/// Spawns a dedicated blocking thread that reads terminal events and forwards
/// them to the main loop via a channel. Using a persistent task (rather than
/// `spawn_blocking` per iteration inside `tokio::select!`) prevents input
/// starvation when the WS event stream keeps the select loop busy — the
/// channel retains pending keypresses across iterations.
fn spawn_input_task() -> mpsc::Receiver<InputEvent> {
    let (tx, rx) = mpsc::channel::<InputEvent>(64);
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return,
            }
            let Ok(ev) = event::read() else { return };
            let mapped = match ev {
                Event::Key(key) if key.kind == KeyEventKind::Press => Some(InputEvent::Key(key)),
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => Some(InputEvent::ScrollUp),
                    MouseEventKind::ScrollDown => Some(InputEvent::ScrollDown),
                    _ => None,
                },
                _ => None,
            };
            if let Some(input) = mapped
                && tx.blocking_send(input).is_err()
            {
                return;
            }
        }
    });
    rx
}

// --- Input handling ---

async fn handle_input(
    app: &mut App,
    input: InputEvent,
    cmd_tx: &mpsc::Sender<TuiCommand>,
    server: &ServerClient,
) {
    match input {
        InputEvent::Key(key) => {
            if app.input_active {
                handle_input_mode(app, key, cmd_tx).await;
                return;
            }

            match app.view {
                View::SessionList => handle_session_list_keys(app, key.code, cmd_tx, server).await,
                View::Conversation => {
                    if !handle_conversation_action_keys(app, key, server).await
                        && !handle_conversation_keys(app, key)
                    {
                        // Key not consumed by navigation — auto-activate input mode.
                        app.input_active = true;
                        handle_input_mode(app, key, cmd_tx).await;
                    }
                }
                View::Help => {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
                        app.view = View::SessionList;
                    }
                }
                View::PermissionDialog => {
                    handle_permission_dialog_keys(app, key.code, cmd_tx).await;
                }
            }
        }
        InputEvent::ScrollUp => match app.view {
            View::Conversation => {
                snap_scroll_if_following(app);
                app.scroll_offset = app.scroll_offset.saturating_sub(3);
                app.follow_tail = false;
            }
            View::SessionList => app.select_prev(),
            View::Help | View::PermissionDialog => {}
        },
        InputEvent::ScrollDown => match app.view {
            View::Conversation => {
                snap_scroll_if_following(app);
                app.scroll_offset = app.scroll_offset.saturating_add(3);
            }
            View::SessionList => app.select_next(),
            View::Help | View::PermissionDialog => {}
        },
    }
}

async fn handle_session_list_keys(
    app: &mut App,
    code: KeyCode,
    cmd_tx: &mpsc::Sender<TuiCommand>,
    server: &ServerClient,
) {
    match code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.select_next(),
        KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
        KeyCode::Char('g') => app.select_first(),
        KeyCode::Char('G') => app.select_last(),
        KeyCode::Char('a') => app.show_all_sessions = !app.show_all_sessions,
        KeyCode::Char('?') => app.view = View::Help,
        KeyCode::Enter => {
            load_conversation(app, cmd_tx, server).await;
            app.follow_tail = true;
            app.view = View::Conversation;
        }
        _ => {}
    }
}

/// When `follow_tail` is active, resolve `scroll_offset` to the actual bottom
/// position so that relative scroll operations work immediately without a dead zone.
const fn snap_scroll_if_following(app: &mut App) {
    if app.follow_tail {
        app.scroll_offset = app.total_display_lines.saturating_sub(app.viewport_height);
    }
}

/// Ctrl-modified conversation actions: Ctrl-C interrupts the
/// in-flight turn, Ctrl-A toggles cctui-side auto-approve. Returns `true` if
/// the key was consumed.
async fn handle_conversation_action_keys(
    app: &mut App,
    key: KeyEvent,
    server: &ServerClient,
) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    match key.code {
        KeyCode::Char('c') => {
            if let Some(id) = app.selected_session().map(|s| s.id.clone())
                && let Err(e) = server.interrupt_session(&id).await
            {
                tracing::warn!(%e, "interrupt failed");
            }
            true
        }
        KeyCode::Char('a') => {
            if let Some((id, want)) =
                app.selected_session().map(|s| (s.id.clone(), !s.auto_approve))
            {
                match server.set_auto_approve(&id, want).await {
                    // Optimistic; the next REST refresh confirms.
                    Ok(()) => {
                        if let Some(s) = app.sessions.iter_mut().find(|s| s.id == id) {
                            s.auto_approve = want;
                        }
                    }
                    Err(e) => tracing::warn!(%e, "auto-approve toggle failed"),
                }
            }
            true
        }
        _ => false,
    }
}

/// Returns `true` if the key was consumed as a navigation command, `false` if not.
/// Unhandled keys in conversation view trigger auto-activation of input mode.
fn handle_conversation_keys(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.view = View::SessionList;
            true
        }
        KeyCode::Char('j') | KeyCode::Down => {
            snap_scroll_if_following(app);
            app.scroll_offset = app.scroll_offset.saturating_add(1);
            app.follow_tail = false;
            true
        }
        KeyCode::Char('k') | KeyCode::Up => {
            snap_scroll_if_following(app);
            app.scroll_offset = app.scroll_offset.saturating_sub(1);
            app.follow_tail = false;
            true
        }
        KeyCode::PageUp => {
            snap_scroll_if_following(app);
            app.scroll_offset = app.scroll_offset.saturating_sub(15);
            app.follow_tail = false;
            true
        }
        KeyCode::PageDown => {
            snap_scroll_if_following(app);
            app.scroll_offset = app.scroll_offset.saturating_add(15);
            true
        }
        KeyCode::Char('g') => {
            app.scroll_offset = 0;
            app.follow_tail = false;
            true
        }
        KeyCode::Char('G') => {
            app.follow_tail = true;
            true
        }
        KeyCode::Char('?') => {
            app.view = View::Help;
            true
        }
        KeyCode::Char('t') => {
            app.show_timestamps = !app.show_timestamps;
            true
        }
        KeyCode::Char(c @ '1'..='9') => {
            let idx = (c as usize) - ('1' as usize);
            let flat = app.flattened_sessions();
            if idx < flat.len() {
                app.selected_index = idx;
                app.follow_tail = true;
            }
            true
        }
        _ => false,
    }
}

async fn handle_permission_dialog_keys(
    app: &mut App,
    code: KeyCode,
    cmd_tx: &mpsc::Sender<TuiCommand>,
) {
    let behavior = match code {
        KeyCode::Char('y') | KeyCode::Enter => "allow",
        KeyCode::Char('n') | KeyCode::Esc => "deny",
        _ => return,
    };

    if let Some(req) = app.permission_queue.pop_front() {
        let _ = cmd_tx
            .send(TuiCommand::PermissionResponse {
                session_id: req.session_id,
                request_id: req.request_id,
                behavior: behavior.to_string(),
            })
            .await;
    }

    // Advance to next queued request or restore previous view
    if app.permission_queue.is_empty() {
        app.view = app.pre_permission_view.clone();
    }
    // else: stay in PermissionDialog, front() now points to the next request
}

async fn handle_input_mode(app: &mut App, key: KeyEvent, cmd_tx: &mpsc::Sender<TuiCommand>) {
    match key.code {
        KeyCode::Esc => {
            app.input_active = false;
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            app.message_input.insert_newline();
        }
        KeyCode::Enter => {
            let content = app.message_input.lines().join("\n");
            if !content.trim().is_empty()
                && let Some(id) = app.selected_session().map(|s| s.id.clone())
            {
                let _ = cmd_tx
                    .send(TuiCommand::Message {
                        session_id: id,
                        content,
                        client_msg_id: None,
                        ask_picks: None,
                        turn_id: None,
                    })
                    .await;
            }
            app.reset_input();
            app.input_active = false;
        }
        _ => {
            app.message_input.input(key);
        }
    }
}

// --- Data loading ---

async fn load_conversation(
    app: &mut App,
    cmd_tx: &mpsc::Sender<TuiCommand>,
    server: &ServerClient,
) {
    let Some(id) = app.selected_session().map(|s| s.id.clone()) else { return };

    if let std::collections::hash_map::Entry::Vacant(entry) = app.stream_buffer.entry(id.clone())
        && let Ok(events) = server.get_conversation(&id).await
    {
        let lines: Vec<ConversationLine> = events
            .iter()
            .filter_map(|v| serde_json::from_value::<AgentEvent>(v.clone()).ok())
            .map(|e| agent_event_to_line(&e))
            .collect();
        if !lines.is_empty() {
            entry.insert(lines);
        }
    }

    let _ = cmd_tx.send(TuiCommand::Subscribe { session_id: id.clone() }).await;
}

// --- Server events ---

fn handle_server_event(app: &mut App, event: ServerEvent) {
    match event {
        ServerEvent::PermissionRequest {
            session_id,
            request_id,
            tool_name,
            description,
            input_preview,
        } => enqueue_permission_request(
            app,
            PendingPermission { session_id, request_id, tool_name, description, input_preview },
        ),
        ServerEvent::Stream { session_id, data } => handle_stream_event(app, session_id, &data),
        ServerEvent::Status { session_id, status } => {
            if let Some(session) = app.sessions.iter_mut().find(|s| s.id == session_id) {
                session.status = status;
                app.update_aggregates();
            }
        }
        ServerEvent::SessionRegistered { session } => register_session(app, session),
        ServerEvent::SessionDeregistered { session_id } => deregister_session(app, &session_id),
        ServerEvent::PermissionResolved { session_id, request_id } => {
            resolve_permission(app, &session_id, &request_id);
        }
        ServerEvent::ArchiveManifest { .. }
        | ServerEvent::ArchiveUploaded { .. }
        | ServerEvent::CommandResult { .. }
        | ServerEvent::SessionEnded { .. }
        | ServerEvent::AskQuestion { .. }
        | ServerEvent::MessageAck { .. }
        | ServerEvent::MachineLiveness { .. }
        | ServerEvent::MachineResources { .. }
        | ServerEvent::AccountUsage { .. }
        | ServerEvent::DispatcherLiveness { .. }
        | ServerEvent::PlanRequest { .. }
        | ServerEvent::PlanResolved { .. }
        | ServerEvent::GithubEvent { .. }
        | ServerEvent::AskResolved { .. }
        | ServerEvent::SoftLimitReached { .. }
        | ServerEvent::PtyChunk { .. }
        | ServerEvent::Heartbeat { .. }
        | ServerEvent::SoftLimitCleared { .. } => {}
    }
}

fn enqueue_permission_request(app: &mut App, req: PendingPermission) {
    let was_empty = app.permission_queue.is_empty();
    app.permission_queue.push_back(req);
    if was_empty {
        app.pre_permission_view = app.view.clone();
        app.view = View::PermissionDialog;
    }
}

fn handle_stream_event(app: &mut App, session_id: String, data: &AgentEvent) {
    if let AgentEvent::Heartbeat { tokens_in, tokens_out, cost_usd, .. } = data
        && let Some(session) = app.sessions.iter_mut().find(|s| s.id == session_id)
    {
        session.token_usage.tokens_in = *tokens_in;
        session.token_usage.tokens_out = *tokens_out;
        session.token_usage.cost_usd = *cost_usd;
    }
    let line = agent_event_to_line(data);
    let buf = app.stream_buffer.entry(session_id).or_default();
    let is_dup = buf.last().is_some_and(|last| last.kind == line.kind && last.text == line.text);
    if !is_dup {
        buf.push(line);
    }
}

fn register_session(app: &mut App, session: cctui_proto::models::Session) {
    if app.sessions.iter().any(|s| s.id == session.id) {
        return;
    }
    app.sessions.push(cctui_proto::api::SessionListItem {
        id: session.id,
        parent_id: session.parent_id,
        machine_id: session.machine_id,
        working_dir: session.working_dir,
        status: session.status,
        liveness: cctui_proto::models::Liveness::Active,
        attention: None,
        // Classifier signals arrive on the next REST refresh; Working until then.
        bucket: cctui_proto::classifier::Bucket::Working,
        token_usage: cctui_proto::models::TokenUsage::default(),
        metadata: session.metadata,
        adapter_id: session.adapter_id,
        machine_name: None,
        machine_hue: None,
        machine_kind: None,
        account_name: None,
        unread_count: 0,
        activity_detail: None,
        last_tool_at: None,
        last_tool_name: None,
        tool_use_count: 0,
        todos: Vec::new(),
        has_token_credentials: false,
        account_traffic_observed: false,
        last_message_text: None,
        last_message_at: None,
        registered_at: Some(session.registered_at),
        name: None,
        model: None,
        effort: None,
        auto_approve: false,
        match_snippet: None,
        match_seq: None,
        last_activity_at: None,
        cache_cold: false,
        estimated_burst_tokens: None,
        hibernated: false,
        pinned: false,
        labels: Vec::new(),
        last_heartbeat: None,
        pr_links: Vec::new(),
        end_reason: None,
        end_detail: None,
        ended_at: None,
    });
    app.update_aggregates();
}

fn deregister_session(app: &mut App, session_id: &str) {
    app.sessions.retain(|s| s.id != session_id);
    app.stream_buffer.remove(session_id);
    let len = app.flattened_sessions().len();
    if len > 0 && app.selected_index >= len {
        app.selected_index = len - 1;
    }
    app.update_aggregates();
}

/// Drop any queued entry that matches; if it's the head and the dialog is
/// currently showing, restore the pre-dialog view.
fn resolve_permission(app: &mut App, session_id: &str, request_id: &str) {
    let was_head_matching = app
        .permission_queue
        .front()
        .is_some_and(|p| p.session_id == session_id && p.request_id == request_id);
    app.permission_queue.retain(|p| !(p.session_id == session_id && p.request_id == request_id));
    if was_head_matching
        && app.permission_queue.is_empty()
        && matches!(app.view, View::PermissionDialog)
    {
        app.view = app.pre_permission_view.clone();
    }
}

fn extract_tag_content(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let Some(start) = text.find(&open)
        && let Some(end) = text[start..].find(&close)
    {
        let content_start = start + open.len();
        return Some(text[content_start..content_start + end].to_string());
    }
    None
}

fn remove_tag_pair(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    if let Some(start) = text.find(&open)
        && let Some(end) = text[start..].find(&close)
    {
        let end_pos = start + end + close.len();
        let mut result = text[..start].to_string();
        result.push_str(&text[end_pos..]);
        return remove_tag_pair(&result, tag);
    }
    text.to_string()
}

fn strip_all_tags(text: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for ch in text.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }
    result
}

fn strip_ansi_codes(text: &str) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            while let Some(&c) = chars.peek() {
                chars.next();
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if ch == '[' {
            let mut temp = chars.clone();
            let is_ansi = temp.peek().is_some_and(|&c| c.is_ascii_digit() || c == ';');
            if is_ansi {
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                result.push(ch);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Strip XML tags, ANSI codes, and system noise from user message text.
/// Returns None if the result is empty or only whitespace.
fn clean_user_message(text: &str) -> Option<String> {
    let mut result = remove_tag_pair(text, "system-reminder");
    result = remove_tag_pair(&result, "local-command-caveat");

    let cmd_name = extract_tag_content(&result, "command-name");
    let cmd_args = extract_tag_content(&result, "command-args");
    let cmd_stdout = extract_tag_content(&result, "local-command-stdout");

    result = remove_tag_pair(&result, "command-name");
    result = remove_tag_pair(&result, "command-args");
    result = remove_tag_pair(&result, "local-command-stdout");
    result = strip_all_tags(&result);

    if let Some(ref name) = cmd_name {
        let mut cmd_line = format!("/{name}");
        if let Some(ref args) = cmd_args
            && !args.is_empty()
        {
            cmd_line.push(' ');
            cmd_line.push_str(args);
        }
        if let Some(ref stdout) = cmd_stdout {
            cmd_line.push_str(" → ");
            cmd_line.push_str(stdout);
        }
        result = if result.trim().is_empty() { cmd_line } else { format!("{cmd_line} {result}") };
    }

    result = strip_ansi_codes(&result);
    let trimmed = result.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn agent_event_to_line(event: &AgentEvent) -> ConversationLine {
    match event {
        AgentEvent::Text { content, meta, ts, kind: text_kind, .. } => {
            let marker = matches!(text_kind.as_deref(), Some("system_marker" | "turn_annotation"));
            let (kind, text) = if marker {
                (LineKind::System, content.clone())
            } else if content.starts_with("▷ User:") {
                let user_text = content.trim_start_matches("▷ User: ");
                // `meta` (set authoritatively at the adapter layer) marks a
                // system/agent-directed message — render it as System, not a
                // user line. Fall back to the local text-cleaning heuristic.
                if *meta {
                    (LineKind::System, user_text.to_owned())
                } else {
                    clean_user_message(user_text).map_or_else(
                        || (LineKind::System, String::new()),
                        |cleaned| (LineKind::User, cleaned),
                    )
                }
            } else {
                (LineKind::Assistant, content.clone())
            };
            ConversationLine { timestamp: *ts, kind, text, tool_input: None }
        }
        AgentEvent::ToolCall { tool, input, ts, .. } => {
            let detail = views::sessions::format_tool_input(tool, input);
            // Keep raw input for Edit/Write so we can generate diffs during render
            let keep_input = matches!(tool.as_str(), "Edit" | "Write");
            ConversationLine {
                timestamp: *ts,
                kind: LineKind::ToolCall,
                text: format!("[{tool}] {detail}"),
                tool_input: if keep_input { Some(input.clone()) } else { None },
            }
        }
        AgentEvent::ToolResult { output_summary, ts, .. } => ConversationLine {
            timestamp: *ts,
            kind: LineKind::ToolResult,
            text: format!("  → {output_summary}"),
            tool_input: None,
        },
        AgentEvent::Heartbeat { ts, .. } | AgentEvent::TurnEnd { ts, .. } => ConversationLine {
            timestamp: *ts,
            kind: LineKind::System,
            text: String::new(),
            tool_input: None,
        },
        // /clear boundary within one session.
        AgentEvent::ContextReset { ts, .. } => ConversationLine {
            timestamp: *ts,
            kind: LineKind::System,
            text: "⟳ context reset (/clear · /compact)".to_owned(),
            tool_input: None,
        },
        // /compact summary (no rotation; carries the summary text).
        AgentEvent::CompactSummary { content, ts, .. } => ConversationLine {
            timestamp: *ts,
            kind: LineKind::System,
            text: format!("⟳ context compacted\n{content}"),
            tool_input: None,
        },
        AgentEvent::TurnSummary { detail, ts, .. } => ConversationLine {
            timestamp: *ts,
            kind: LineKind::System,
            text: format!("· {detail}"),
            tool_input: None,
        },
        AgentEvent::Reply { content, ts, .. } => ConversationLine {
            timestamp: *ts,
            kind: LineKind::Reply,
            text: content.clone(),
            tool_input: None,
        },
    }
}

// --- Rendering ---

fn render(frame: &mut Frame, app: &mut App) {
    match app.view {
        View::SessionList => views::sessions::draw(frame, app),
        View::Conversation => views::conversation::draw(frame, app),
        View::Help => {
            // Show help on top of whatever view was active
            views::sessions::draw(frame, app);
            views::help::draw(frame);
        }
        View::PermissionDialog => {
            // Draw underlying view, then overlay the dialog
            match app.pre_permission_view {
                View::Conversation => views::conversation::draw(frame, app),
                _ => views::sessions::draw(frame, app),
            }
            if let Some(req) = app.permission_queue.front() {
                views::permission::draw(frame, req);
            }
        }
    }
}
