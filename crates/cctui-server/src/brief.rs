//! `GET /sessions/{id}/brief`: the user/assistant transcript of a session as
//! markdown, for a follow-up session's first prompt. Tool calls, tool
//! results, thinking, harness meta and keep-alive ticks are dropped; images
//! stay as the name the transcript already references them by. Consecutive
//! chunks of one speaker collapse into one turn. The oldest turns are elided
//! behind one line once the turn or byte cap is hit.

use std::fmt::Write as _;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::Value;

use cctui_proto::api::{ApiError, BriefResponse};

use crate::state::AppState;

pub const DEFAULT_MAX_TURNS: usize = 60;
pub const DEFAULT_MAX_BYTES: usize = 48 * 1024;
const USER_PREFIX: &str = "▷ User: ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    const fn heading(self) -> &'static str {
        match self {
            Self::User => "**User:**",
            Self::Assistant => "**Assistant:**",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_turns: usize,
    pub max_bytes: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self { max_turns: DEFAULT_MAX_TURNS, max_bytes: DEFAULT_MAX_BYTES }
    }
}

fn is_keepalive(payload: &Value) -> bool {
    payload.get("metadata").and_then(|m| m.get("keepalive")).and_then(Value::as_bool) == Some(true)
}

/// The speaker and prose of one stored row, or `None` for anything the brief
/// drops.
fn classify(adapter_id: &str, event_type: &str, payload: Value) -> Option<(Role, String)> {
    let v = crate::normalize::for_client(adapter_id, event_type, payload)?;
    if v.get("type").and_then(Value::as_str) != Some("text")
        || v.get("meta").and_then(Value::as_bool).unwrap_or(false)
    {
        return None;
    }
    let content = v.get("content").and_then(Value::as_str).unwrap_or_default();
    let kind = v.get("kind").and_then(Value::as_str);
    if let Some(text) = content.strip_prefix(USER_PREFIX) {
        return Some((Role::User, text.to_owned()));
    }
    if v.get("role").and_then(Value::as_str) == Some("Assistant")
        && matches!(kind, None | Some("attachment"))
    {
        return Some((Role::Assistant, content.to_owned()));
    }
    None
}

/// Fold stored rows (oldest first) into speaker turns.
pub fn collect_turns<'a>(
    adapter_id: &str,
    rows: impl IntoIterator<Item = (&'a str, Value)>,
) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut in_tick = false;
    for (event_type, payload) in rows {
        if is_keepalive(&payload) {
            in_tick = true;
            continue;
        }
        let Some((role, text)) = classify(adapter_id, event_type, payload) else { continue };
        if in_tick && role == Role::Assistant {
            continue;
        }
        in_tick = false;
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some(last) if last.role == role => {
                last.text.push_str("\n\n");
                last.text.push_str(text);
            }
            _ => turns.push(Turn { role, text: text.to_owned() }),
        }
    }
    turns
}

const fn turn_len(turn: &Turn) -> usize {
    turn.role.heading().len() + 2 + turn.text.len() + 2
}

fn cut_head(text: &str, keep: usize) -> String {
    if text.len() <= keep {
        return text.to_owned();
    }
    let mut start = text.len() - keep;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Render the newest turns that fit the caps.
pub fn render(turns: &[Turn], caps: Caps) -> BriefResponse {
    let max_turns = caps.max_turns.max(1);
    let mut start = turns.len().saturating_sub(max_turns);
    let mut truncated = false;
    let mut bytes: usize = turns[start..].iter().map(turn_len).sum();
    while bytes > caps.max_bytes && start + 1 < turns.len() {
        bytes -= turn_len(&turns[start]);
        start += 1;
        truncated = true;
    }
    let omitted = start;
    let mut kept: Vec<Turn> = turns[start..].to_vec();
    if let Some(only) = kept.first_mut()
        && turn_len(only) > caps.max_bytes
    {
        let overhead = turn_len(only) - only.text.len();
        only.text = cut_head(&only.text, caps.max_bytes.saturating_sub(overhead));
        truncated = true;
    }
    let mut markdown = String::new();
    if omitted > 0 {
        let _ = write!(markdown, "… {omitted} earlier turns omitted\n\n");
    }
    for turn in &kept {
        markdown.push_str(turn.role.heading());
        markdown.push_str("\n\n");
        markdown.push_str(&turn.text);
        markdown.push_str("\n\n");
    }
    let markdown = markdown.trim_end().to_owned();
    BriefResponse {
        markdown,
        turns: u32::try_from(kept.len()).unwrap_or(u32::MAX),
        omitted: u32::try_from(omitted).unwrap_or(u32::MAX),
        truncated,
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct BriefQuery {
    pub max_turns: Option<usize>,
    pub max_bytes: Option<usize>,
}

impl BriefQuery {
    fn caps(&self) -> Caps {
        let d = Caps::default();
        Caps {
            max_turns: self.max_turns.unwrap_or(d.max_turns).clamp(1, 10_000),
            max_bytes: self.max_bytes.unwrap_or(d.max_bytes).clamp(256, 4 * 1024 * 1024),
        }
    }
}

pub async fn session_brief(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<BriefQuery>,
) -> Result<Json<BriefResponse>, (StatusCode, Json<ApiError>)> {
    let db_err = |e: sqlx::Error| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    };
    let adapter_id: Option<Option<String>> =
        sqlx::query_scalar("SELECT adapter_id FROM sessions WHERE id = $1")
            .bind(&session_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db_err)?;
    let Some(adapter_id) = adapter_id else {
        return Err((StatusCode::NOT_FOUND, Json(ApiError { error: "session not found".into() })));
    };
    let adapter_id = adapter_id.unwrap_or_else(|| "claude-code".to_owned());
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT event_type, payload FROM stream_events \
         WHERE session_id = $1 AND event_type IN ('message', 'tool_use') ORDER BY id ASC",
    )
    .bind(&session_id)
    .fetch_all(&state.pool)
    .await
    .map_err(db_err)?;
    let turns = collect_turns(&adapter_id, rows.iter().map(|(t, p)| (t.as_str(), p.clone())));
    Ok(Json(render(&turns, q.caps())))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Caps, Role, Turn, collect_turns, render};

    fn user(text: &str) -> (&'static str, serde_json::Value) {
        ("message", json!({"role": "user", "text": text}))
    }

    fn assistant(text: &str) -> (&'static str, serde_json::Value) {
        ("message", json!({"role": "assistant", "text": text, "message_id": "m1"}))
    }

    #[test]
    fn keeps_user_and_assistant_prose_only() {
        let rows = vec![
            user("hello"),
            ("message", json!({"role": "assistant_thinking", "text": "hmm"})),
            ("tool_use", json!({"tool": "Bash", "input": {"command": "ls"}})),
            ("tool_use", json!({"kind": "tool_result", "content": "a b c"})),
            ("message", json!({"role": "system_marker", "text": "compacted"})),
            (
                "message",
                json!({"role": "user", "text": "<task-notification>x</task-notification>", "meta": true}),
            ),
            assistant("hi there"),
        ];
        let turns = collect_turns("claude-code", rows);
        assert_eq!(
            turns,
            vec![
                Turn { role: Role::User, text: "hello".into() },
                Turn { role: Role::Assistant, text: "hi there".into() },
            ]
        );
    }

    #[test]
    fn keepalive_ticks_are_dropped() {
        let prompt = crate::keepalive::tick_prompt(chrono::Utc::now());
        let mut tick = json!({"role": "user", "text": prompt});
        assert!(crate::keepalive::stamp_tick(&mut tick));
        let turns = collect_turns(
            "claude-code",
            vec![user("q"), assistant("a"), ("message", tick), assistant("warm"), user("next")],
        );
        assert_eq!(
            turns,
            vec![
                Turn { role: Role::User, text: "q".into() },
                Turn { role: Role::Assistant, text: "a".into() },
                Turn { role: Role::User, text: "next".into() },
            ]
        );
    }

    #[test]
    fn consecutive_chunks_of_one_speaker_fold_into_one_turn() {
        let turns =
            collect_turns("claude-code", vec![user("q"), assistant("part 1"), assistant("part 2")]);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].text, "part 1\n\npart 2");
    }

    #[test]
    fn images_stay_as_their_reference() {
        let rows = vec![
            user("[Image: source: shot.png]\nwhat is this"),
            (
                "message",
                json!({"role": "assistant_attachment", "text": "[image attachment]", "message_id": "m"}),
            ),
            assistant("a chart"),
        ];
        let turns = collect_turns("claude-code", rows);
        assert_eq!(turns[0].text, "[Image: source: shot.png]\nwhat is this");
        assert_eq!(turns[1].text, "[image attachment]\n\na chart");
    }

    #[test]
    fn codex_turns_map_too() {
        let rows = vec![
            ("message", json!({"type": "userMessage", "content": [{"type": "text", "text": "q"}]})),
            ("message", json!({"type": "agentMessage", "text": "a"})),
        ];
        let turns = collect_turns("codex", rows);
        assert!(turns.iter().any(|t| t.role == Role::Assistant), "{turns:?}");
    }

    #[test]
    fn renders_markdown_headings() {
        let turns = collect_turns("claude-code", vec![user("q"), assistant("a")]);
        let out = render(&turns, Caps::default());
        assert_eq!(out.markdown, "**User:**\n\nq\n\n**Assistant:**\n\na");
        assert_eq!((out.turns, out.omitted, out.truncated), (2, 0, false));
    }

    #[test]
    fn turn_cap_elides_the_oldest_turns() {
        let rows: Vec<_> = (0..10)
            .flat_map(|i| {
                [("message", json!({"role": "user", "text": format!("q{i}")})), assistant("a")]
            })
            .collect();
        let turns = collect_turns("claude-code", rows);
        let out = render(&turns, Caps { max_turns: 4, max_bytes: 1 << 20 });
        assert!(out.markdown.starts_with("… 16 earlier turns omitted\n\n**User:**\n\nq8"));
        assert_eq!((out.turns, out.omitted, out.truncated), (4, 16, false));
    }

    #[test]
    fn byte_cap_elides_oldest_turns_and_flags_truncation() {
        let turns: Vec<Turn> =
            (0..5).map(|i| Turn { role: Role::User, text: format!("{i}").repeat(100) }).collect();
        let out = render(&turns, Caps { max_turns: 60, max_bytes: 250 });
        assert_eq!(out.omitted, 3);
        assert_eq!(out.turns, 2);
        assert!(out.truncated);
        assert!(out.markdown.starts_with("… 3 earlier turns omitted"));
        assert!(out.markdown.contains(&"3".repeat(100)) && out.markdown.contains(&"4".repeat(100)));
        assert!(!out.markdown.contains(&"2".repeat(100)));
    }

    #[test]
    fn a_single_oversized_turn_keeps_its_tail() {
        let turns = vec![Turn {
            role: Role::Assistant,
            text: format!("{}é{}", "x".repeat(300), "y".repeat(100)),
        }];
        let out = render(&turns, Caps { max_turns: 60, max_bytes: 128 });
        assert!(out.truncated);
        assert_eq!(out.omitted, 0);
        assert!(out.markdown.len() <= 128 + "…".len());
        assert!(out.markdown.ends_with(&"y".repeat(100)));
        assert!(out.markdown.contains("**Assistant:**\n\n…"));
    }
}
