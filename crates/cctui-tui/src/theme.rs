use ratatui::style::{Color, Modifier, Style};

// Status colors
pub const ACTIVE: Style = Style::new().fg(Color::Green);
pub const NEW: Style = Style::new().fg(Color::Cyan);
pub const INACTIVE: Style = Style::new().fg(Color::DarkGray);

// UI chrome
pub const BORDER_FOCUSED: Style = Style::new().fg(Color::Blue);
pub const BORDER_DIM: Style = Style::new().fg(Color::DarkGray);
pub const SELECTED: Style = Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD);
pub const HOTKEY: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
pub const HOTKEY_DESC: Style = Style::new().fg(Color::DarkGray);
pub const STATUS_BAR_BG: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
pub const DIM: Style = Style::new().fg(Color::DarkGray);
pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

// Session list details
pub const MODEL: Style = Style::new().fg(Color::DarkGray);
pub const COST: Style = Style::new().fg(Color::Yellow);
pub const BRANCH: Style = Style::new().fg(Color::DarkGray);

// Borderless layout
pub const HEADER_BG: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
pub const SECTION_TITLE: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);

pub const fn status_style(status: cctui_proto::models::SessionStatus) -> Style {
    match status {
        cctui_proto::models::SessionStatus::Active => ACTIVE,
        cctui_proto::models::SessionStatus::New => NEW,
        cctui_proto::models::SessionStatus::Inactive
        | cctui_proto::models::SessionStatus::Archived
        | cctui_proto::models::SessionStatus::Draft
        | cctui_proto::models::SessionStatus::Queued => INACTIVE,
    }
}

pub const fn status_icon(status: cctui_proto::models::SessionStatus) -> &'static str {
    match status {
        cctui_proto::models::SessionStatus::Active => "●",
        cctui_proto::models::SessionStatus::New => "◎",
        cctui_proto::models::SessionStatus::Inactive => "○",
        cctui_proto::models::SessionStatus::Archived => "▢",
        cctui_proto::models::SessionStatus::Draft => "◌",
        cctui_proto::models::SessionStatus::Queued => "⧗",
    }
}
