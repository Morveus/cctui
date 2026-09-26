//! Session keep-alive: ticks an idle session every `interval_secs` so the
//! provider's prompt cache stays warm. A tick is a real turn (tokens, activity
//! bump, possible worker resume), hence the skip rules and the tick budget.
//! Claims go through one guarded `UPDATE … RETURNING`, so replicas cannot
//! double-send.

use cctui_proto::api::{KeepaliveState, SessionKeepaliveRequest};
use chrono::{DateTime, Duration, Utc};

use crate::soft_limit::UsageWindow;
use crate::state::AppState;

/// Ticks sent without a human message before the schedule pauses.
pub const DEFAULT_MAX_TICKS: u32 = 6;
/// Bounds on a user-chosen interval.
pub const MIN_INTERVAL_SECS: u32 = 60;
pub const MAX_INTERVAL_SECS: u32 = 24 * 3600;
/// Margin under the provider's cache TTL so the tick lands before expiry.
const TTL_MARGIN_SECS: u32 = 120;
/// Never tick an account past this share of any usage window.
pub const USAGE_CEILING_PCT: f64 = 90.0;
/// Sessions claimed per sweep.
const BATCH: i64 = 20;

/// Prefix of every tick; the webui and the stamp below key on it.
pub const TICK_MARKER: &str = "[cctui keep-alive";

/// Cache TTLs per provider family, mirroring the webui's `cacheTtl.ts`.
const ANTHROPIC_TTL_SECS: u32 = 60 * 60;
const OPENAI_GPT56_TTL_SECS: u32 = 30 * 60;
const DEFAULT_TTL_SECS: u32 = 5 * 60;

fn is_gpt56_or_later(model: &str) -> bool {
    let lower = model.to_lowercase();
    let Some(rest) = lower.find("gpt-").map(|i| &lower[i + 4..]) else {
        return false;
    };
    let mut parts = rest.split(|c: char| !c.is_ascii_digit() && c != '.');
    let ver = parts.next().unwrap_or("");
    let mut nums = ver.split('.');
    let major: u32 = nums.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let minor: u32 = nums.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    major > 5 || (major == 5 && minor >= 6)
}

/// The prompt-cache TTL for a session's provider family and model.
#[must_use]
pub fn cache_ttl_secs(adapter_id: Option<&str>, model: Option<&str>) -> u32 {
    let adapter = adapter_id.unwrap_or("").to_lowercase();
    if adapter.contains("claude") {
        return ANTHROPIC_TTL_SECS;
    }
    if (adapter.contains("codex") || adapter.contains("openai"))
        && model.is_some_and(is_gpt56_or_later)
    {
        return OPENAI_GPT56_TTL_SECS;
    }
    DEFAULT_TTL_SECS
}

/// Default tick interval: the provider TTL minus a two-minute margin, never
/// below [`MIN_INTERVAL_SECS`].
#[must_use]
pub fn default_interval_secs(adapter_id: Option<&str>, model: Option<&str>) -> u32 {
    cache_ttl_secs(adapter_id, model).saturating_sub(TTL_MARGIN_SECS).max(MIN_INTERVAL_SECS)
}

/// The tick text. The timestamp keeps every tick a distinct transcript event
/// (the dedup index would otherwise swallow a repeat).
#[must_use]
pub fn tick_prompt(now: DateTime<Utc>) -> String {
    format!(
        "{TICK_MARKER} {}] Cache keep-alive tick. Reply with a single word.",
        now.format("%Y-%m-%dT%H:%M:%SZ")
    )
}

/// Whether a user-role message text is one of our ticks.
#[must_use]
pub fn is_tick(text: &str) -> bool {
    text.trim_start().trim_start_matches("▷ User:").trim_start().starts_with(TICK_MARKER)
}

/// Build the schedule a set request asks for, or `None` to clear it.
/// `adapter_id`/`model` drive the default interval.
pub fn schedule_from_request(
    req: &SessionKeepaliveRequest,
    adapter_id: Option<&str>,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<KeepaliveState>, String> {
    if !req.enabled {
        return Ok(None);
    }
    let interval_secs =
        req.interval_secs.unwrap_or_else(|| default_interval_secs(adapter_id, model));
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&interval_secs) {
        return Err(format!(
            "interval_secs must be between {MIN_INTERVAL_SECS} and {MAX_INTERVAL_SECS}"
        ));
    }
    let max_ticks = req.max_ticks.unwrap_or(DEFAULT_MAX_TICKS);
    Ok(Some(KeepaliveState {
        interval_secs,
        max_ticks,
        ticks_sent: 0,
        until: until_for(interval_secs, max_ticks, now),
    }))
}

fn until_for(interval_secs: u32, max_ticks: u32, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
    (max_ticks > 0)
        .then(|| from + Duration::seconds(i64::from(interval_secs) * i64::from(max_ticks)))
}

/// Why a session must not be ticked right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    NeedsInput,
    SoftLimitBlocked,
    Ended,
    MidTurn,
    Exhausted,
    UsageHigh,
}

/// The row signals the skip rules read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot<'a> {
    pub status: &'a str,
    pub tempo: Option<&'a str>,
    pub agent_state: Option<&'a str>,
    pub soft_limit_reason: Option<&'a str>,
    pub ended: bool,
    pub ticks_sent: u32,
    pub max_ticks: u32,
}

/// Pure skip rules, mirrored by the claim query's WHERE clause so a claimed
/// row is re-checked with the same logic before anything is sent.
#[must_use]
pub fn skip_reason(snap: &Snapshot<'_>) -> Option<Skip> {
    if snap.ended || !matches!(snap.status, "active" | "inactive") {
        return Some(Skip::Ended);
    }
    if snap.soft_limit_reason.is_some() {
        return Some(Skip::SoftLimitBlocked);
    }
    if snap.tempo == Some("blocked") {
        return Some(Skip::NeedsInput);
    }
    if snap.tempo == Some("active") || matches!(snap.agent_state, Some("working" | "running")) {
        return Some(Skip::MidTurn);
    }
    if snap.max_ticks > 0 && snap.ticks_sent >= snap.max_ticks {
        return Some(Skip::Exhausted);
    }
    None
}

/// Whether any usage window of the account is past [`USAGE_CEILING_PCT`].
#[must_use]
pub fn usage_too_high(windows: &[UsageWindow]) -> bool {
    windows.iter().any(|w| w.utilization >= USAGE_CEILING_PCT)
}

/// Whether a due tick should fire, given the row and the usage of every
/// account bound to the session.
#[must_use]
pub fn decide(snap: &Snapshot<'_>, usage: &[Vec<UsageWindow>]) -> Option<Skip> {
    skip_reason(snap).or_else(|| usage.iter().any(|w| usage_too_high(w)).then_some(Skip::UsageHigh))
}

/// Stamp `metadata.keepalive = true` on the daemon's echo of a tick, in place.
/// Returns whether the payload was a tick.
pub fn stamp_tick(payload: &mut serde_json::Value) -> bool {
    let is_user_tick = payload.get("role").and_then(serde_json::Value::as_str) == Some("user")
        && payload.get("text").and_then(serde_json::Value::as_str).is_some_and(is_tick);
    if is_user_tick && let Some(obj) = payload.as_object_mut() {
        let meta = obj.entry("metadata").or_insert_with(|| serde_json::json!({}));
        if let Some(m) = meta.as_object_mut() {
            m.insert("keepalive".to_owned(), serde_json::Value::Bool(true));
        }
    }
    is_user_tick
}

/// Whether a message payload is a genuine human turn (resets `ticks_sent`).
#[must_use]
pub fn is_human_message(payload: &serde_json::Value) -> bool {
    payload.get("role").and_then(serde_json::Value::as_str) == Some("user")
        && !payload.get("text").and_then(serde_json::Value::as_str).is_some_and(is_tick)
}

/// Called for every persisted `message` event: stamps ticks and resets the
/// tick budget on human activity.
pub async fn observe_message(state: &AppState, session_id: &str, payload: &mut serde_json::Value) {
    if stamp_tick(payload) || !is_human_message(payload) {
        return;
    }
    let _ = sqlx::query(
        "UPDATE sessions SET keepalive_json = jsonb_set( \
             jsonb_set(keepalive_json, '{ticks_sent}', '0'::jsonb), \
             '{until}', \
             CASE WHEN (keepalive_json->>'max_ticks')::int > 0 \
                  THEN to_jsonb(now() + make_interval(secs => \
                       (keepalive_json->>'interval_secs')::int \
                       * (keepalive_json->>'max_ticks')::int)) \
                  ELSE 'null'::jsonb END) \
         WHERE id = $1 AND keepalive_json IS NOT NULL",
    )
    .bind(session_id)
    .execute(&state.pool)
    .await
    .map_err(|e| tracing::warn!(%session_id, "keep-alive reset failed: {e}"));
}

/// Persist (or clear) a session's schedule. `Ok(None)` when the session does
/// not exist.
pub async fn apply(
    state: &AppState,
    session_id: &str,
    req: &SessionKeepaliveRequest,
) -> Result<Option<Result<Option<KeepaliveState>, String>>, sqlx::Error> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT adapter_id, model FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some((adapter_id, model)) = row else {
        return Ok(None);
    };
    let schedule =
        match schedule_from_request(req, adapter_id.as_deref(), model.as_deref(), Utc::now()) {
            Ok(s) => s,
            Err(e) => return Ok(Some(Err(e))),
        };
    let json = schedule.as_ref().map(serde_json::to_value).transpose().unwrap_or_default();
    sqlx::query("UPDATE sessions SET keepalive_json = $2, last_keepalive_at = NULL WHERE id = $1")
        .bind(session_id)
        .bind(json)
        .execute(&state.pool)
        .await?;
    tracing::info!(%session_id, enabled = schedule.is_some(), "session keep-alive updated");
    Ok(Some(Ok(schedule)))
}

/// Claim every due tick in one statement. The inner predicate is the SQL twin
/// of [`skip_reason`]; the outer `last_keepalive_at` guard is what makes the
/// claim safe across replicas.
const CLAIM_SQL: &str = "WITH due AS ( \
        SELECT id FROM sessions \
        WHERE keepalive_json IS NOT NULL \
          AND status IN ('active', 'inactive') \
          AND ended_at IS NULL \
          AND soft_limit_reason IS NULL \
          AND COALESCE(tempo, '') NOT IN ('blocked', 'active') \
          AND COALESCE(agent_state, '') NOT IN ('working', 'running') \
          AND (COALESCE((keepalive_json->>'max_ticks')::int, 0) = 0 \
               OR COALESCE((keepalive_json->>'ticks_sent')::int, 0) \
                  < (keepalive_json->>'max_ticks')::int) \
          AND GREATEST(COALESCE(last_keepalive_at, to_timestamp(0)), \
                       COALESCE(last_heartbeat, to_timestamp(0))) \
              < now() - make_interval(secs => (keepalive_json->>'interval_secs')::int) \
        ORDER BY last_keepalive_at NULLS FIRST \
        LIMIT $1 \
        FOR UPDATE SKIP LOCKED \
     ) \
     UPDATE sessions s \
     SET last_keepalive_at = now(), \
         keepalive_json = jsonb_set(s.keepalive_json, '{ticks_sent}', \
             to_jsonb(COALESCE((s.keepalive_json->>'ticks_sent')::int, 0) + 1)) \
     FROM due \
     WHERE s.id = due.id \
       AND (s.last_keepalive_at IS NULL \
            OR s.last_keepalive_at \
               < now() - make_interval(secs => (s.keepalive_json->>'interval_secs')::int)) \
     RETURNING s.id, s.status, s.tempo, s.agent_state, s.soft_limit_reason, \
               s.ended_at, s.keepalive_json";

#[derive(sqlx::FromRow)]
struct ClaimedRow {
    id: String,
    status: String,
    tempo: Option<String>,
    agent_state: Option<String>,
    soft_limit_reason: Option<String>,
    ended_at: Option<DateTime<Utc>>,
    keepalive_json: serde_json::Value,
}

/// One reaper-cadence sweep: claim the due ticks and send each through the
/// same path as a human reply. A claimed tick that turns out unsendable is
/// simply deferred to the next interval.
pub async fn sweep(state: &AppState) {
    let rows: Vec<ClaimedRow> =
        match sqlx::query_as(CLAIM_SQL).bind(BATCH).fetch_all(&state.pool).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("keep-alive claim query failed: {e}");
                return;
            }
        };
    for row in rows {
        let schedule: KeepaliveState = match serde_json::from_value(row.keepalive_json.clone()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_id = %row.id, "keep-alive schedule unreadable: {e}");
                continue;
            }
        };
        let snap = Snapshot {
            status: &row.status,
            tempo: row.tempo.as_deref(),
            agent_state: row.agent_state.as_deref(),
            soft_limit_reason: row.soft_limit_reason.as_deref(),
            ended: row.ended_at.is_some(),
            ticks_sent: schedule.ticks_sent.saturating_sub(1),
            max_ticks: schedule.max_ticks,
        };
        let mut usage = Vec::new();
        for account_id in crate::routes::gateway::resolve_session_accounts(state, &row.id).await {
            let windows = crate::routes::gateway::usage_for_soft_limit(state, account_id)
                .await
                .as_ref()
                .map(crate::soft_limit::normalize_usage_windows)
                .unwrap_or_default();
            usage.push(windows);
        }
        if let Some(reason) = decide(&snap, &usage) {
            tracing::info!(session_id = %row.id, ?reason, "keep-alive tick skipped");
            continue;
        }
        fire(state, &row.id, schedule.ticks_sent, schedule.max_ticks).await;
    }
}

async fn fire(state: &AppState, session_id: &str, tick: u32, max_ticks: u32) {
    let env = crate::routes::gateway::resume_env_for_session(state, session_id).await;
    let dispatch = crate::bus::dispatch(
        state,
        session_id,
        cctui_proto::adapter::AdapterCommand::Reply {
            local_id: session_id.to_owned(),
            text: tick_prompt(Utc::now()),
            ask_picks: None,
            env,
            command_id: None,
            turn_id: None,
        },
    )
    .await;
    match dispatch {
        Ok(()) => tracing::info!(%session_id, tick, max_ticks, "keep-alive tick sent"),
        Err(err) => {
            tracing::warn!(%session_id, tick, %err, "keep-alive tick could not be dispatched");
        }
    }
}

#[cfg(test)]
mod tests {
    use cctui_proto::api::SessionKeepaliveRequest;
    use chrono::{TimeZone, Utc};

    use super::{
        CLAIM_SQL, DEFAULT_MAX_TICKS, MIN_INTERVAL_SECS, Skip, Snapshot, cache_ttl_secs, decide,
        default_interval_secs, is_human_message, is_tick, schedule_from_request, skip_reason,
        stamp_tick, tick_prompt, usage_too_high,
    };
    use crate::soft_limit::UsageWindow;

    fn idle() -> Snapshot<'static> {
        Snapshot {
            status: "active",
            tempo: Some("idle"),
            agent_state: Some("idle"),
            soft_limit_reason: None,
            ended: false,
            ticks_sent: 0,
            max_ticks: DEFAULT_MAX_TICKS,
        }
    }

    fn window(utilization: f64) -> UsageWindow {
        UsageWindow {
            key: "session".into(),
            kind: "percent".into(),
            label: "5h".into(),
            utilization,
            amount_usd: None,
            resets_at: None,
            model_id: None,
            model_display_name: None,
        }
    }

    #[test]
    fn an_idle_live_session_is_ticked() {
        assert_eq!(skip_reason(&idle()), None);
        assert_eq!(decide(&idle(), &[vec![window(10.0)]]), None);
    }

    #[test]
    fn needs_input_blocked_ended_and_mid_turn_are_skipped() {
        assert_eq!(
            skip_reason(&Snapshot { tempo: Some("blocked"), ..idle() }),
            Some(Skip::NeedsInput)
        );
        assert_eq!(
            skip_reason(&Snapshot { soft_limit_reason: Some("weekly_all"), ..idle() }),
            Some(Skip::SoftLimitBlocked)
        );
        assert_eq!(skip_reason(&Snapshot { ended: true, ..idle() }), Some(Skip::Ended));
        assert_eq!(skip_reason(&Snapshot { status: "archived", ..idle() }), Some(Skip::Ended));
        assert_eq!(skip_reason(&Snapshot { status: "draft", ..idle() }), Some(Skip::Ended));
        assert_eq!(skip_reason(&Snapshot { tempo: Some("active"), ..idle() }), Some(Skip::MidTurn));
        assert_eq!(
            skip_reason(&Snapshot { agent_state: Some("working"), ..idle() }),
            Some(Skip::MidTurn)
        );
        assert_eq!(
            skip_reason(&Snapshot { agent_state: Some("running"), ..idle() }),
            Some(Skip::MidTurn)
        );
    }

    #[test]
    fn hibernated_sessions_are_still_ticked_so_a_reply_can_resume_them() {
        assert_eq!(skip_reason(&Snapshot { tempo: Some("hibernated"), ..idle() }), None);
    }

    #[test]
    fn the_tick_budget_stops_the_schedule_unless_indefinite() {
        assert_eq!(skip_reason(&Snapshot { ticks_sent: 5, ..idle() }), None);
        assert_eq!(skip_reason(&Snapshot { ticks_sent: 6, ..idle() }), Some(Skip::Exhausted));
        assert_eq!(skip_reason(&Snapshot { ticks_sent: 60, max_ticks: 0, ..idle() }), None);
    }

    #[test]
    fn any_window_over_ninety_percent_blocks_the_tick() {
        assert!(!usage_too_high(&[window(10.0), window(89.9)]));
        assert!(usage_too_high(&[window(10.0), window(90.0)]));
        assert_eq!(
            decide(&idle(), &[vec![window(5.0)], vec![window(95.0)]]),
            Some(Skip::UsageHigh)
        );
        assert_eq!(decide(&idle(), &[]), None);
        assert_eq!(
            decide(&Snapshot { ended: true, ..idle() }, &[vec![window(95.0)]]),
            Some(Skip::Ended)
        );
    }

    #[test]
    fn default_interval_is_the_provider_ttl_minus_two_minutes() {
        assert_eq!(cache_ttl_secs(Some("claude-code"), None), 3600);
        assert_eq!(default_interval_secs(Some("claude-code"), Some("claude-opus-5")), 3480);
        assert_eq!(default_interval_secs(Some("codex"), Some("gpt-5.6-codex")), 1680);
        assert_eq!(default_interval_secs(Some("codex"), Some("gpt-6")), 1680);
        assert_eq!(default_interval_secs(Some("codex"), Some("gpt-5.5")), 180);
        assert_eq!(default_interval_secs(Some("codex"), None), 180);
        assert_eq!(default_interval_secs(None, None), 180);
        assert!(default_interval_secs(None, None) >= MIN_INTERVAL_SECS);
    }

    #[test]
    fn a_set_request_fills_defaults_and_validates_the_interval() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 12, 0, 0).unwrap();
        let req = SessionKeepaliveRequest { enabled: true, interval_secs: None, max_ticks: None };
        let s = schedule_from_request(&req, Some("claude-code"), None, now).unwrap().unwrap();
        assert_eq!((s.interval_secs, s.max_ticks, s.ticks_sent), (3480, 6, 0));
        assert_eq!(s.until, Some(now + chrono::Duration::seconds(3480 * 6)));

        let indefinite =
            SessionKeepaliveRequest { enabled: true, interval_secs: Some(600), max_ticks: Some(0) };
        let s = schedule_from_request(&indefinite, None, None, now).unwrap().unwrap();
        assert_eq!(s.until, None);

        let off =
            SessionKeepaliveRequest { enabled: false, interval_secs: Some(600), max_ticks: None };
        assert_eq!(schedule_from_request(&off, None, None, now).unwrap(), None);

        let too_short =
            SessionKeepaliveRequest { enabled: true, interval_secs: Some(30), max_ticks: None };
        assert!(schedule_from_request(&too_short, None, None, now).is_err());
    }

    #[test]
    fn ticks_are_recognised_and_stamped_and_human_messages_are_not() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 12, 0, 0).unwrap();
        let text = tick_prompt(now);
        assert!(is_tick(&text));
        assert!(is_tick(&format!("▷ User: {text}")));
        assert!(!is_tick("continue"));

        let mut tick = serde_json::json!({"role": "user", "text": text});
        assert!(stamp_tick(&mut tick));
        assert_eq!(tick["metadata"]["keepalive"], serde_json::json!(true));
        assert!(!is_human_message(&tick));

        let mut human = serde_json::json!({"role": "user", "text": "please continue"});
        assert!(!stamp_tick(&mut human));
        assert!(human.get("metadata").is_none());
        assert!(is_human_message(&human));

        let mut assistant = serde_json::json!({"role": "assistant", "text": text});
        assert!(!stamp_tick(&mut assistant));
        assert!(!is_human_message(&assistant));
    }

    #[test]
    fn the_claim_is_guarded_by_last_keepalive_at_on_the_update_itself() {
        assert!(CLAIM_SQL.contains("FOR UPDATE SKIP LOCKED"));
        assert!(CLAIM_SQL.contains(
            "AND (s.last_keepalive_at IS NULL OR s.last_keepalive_at < now() - make_interval"
        ));
        assert!(CLAIM_SQL.contains("RETURNING s.id"));
        for guard in [
            "status IN ('active', 'inactive')",
            "ended_at IS NULL",
            "soft_limit_reason IS NULL",
            "COALESCE(tempo, '') NOT IN ('blocked', 'active')",
            "COALESCE(agent_state, '') NOT IN ('working', 'running')",
        ] {
            assert!(CLAIM_SQL.contains(guard), "claim query lost the `{guard}` guard");
        }
    }
}
