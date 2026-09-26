//! Archive-on-done for one-shot spawns (macros).
//!
//! A spawn that carries `auto_archive: true` leaves an intent row keyed by
//! its spawn key (the pre-minted session id for claude-code, the command id
//! for adapters that mint their own id). When the daemon registers the
//! session, the intent is claimed and moves to `sessions.metadata.auto_archive`.
//! The reaper then archives the session the first time the classifier reads
//! it as *done* without a failure: a session that asks a question, is
//! stopped by hand, or fails stays on the list so the human can look.
//!
//! The flag is consumed on archive, so un-archiving the session later does
//! not re-arm it.

use std::collections::HashMap;

use cctui_proto::classifier::Bucket;
use cctui_proto::models::SessionStatus;
use chrono::{DateTime, Utc};

use crate::live_sessions::live_sessions_predicate;
use crate::routes::sessions::ArchiveOutcome;
use crate::state::AppState;

/// Unclaimed intents older than this are dropped: the spawn never registered.
const INTENT_TTL_SECS: i64 = 6 * 3600;
const BATCH: i64 = 100;

/// Remember that the session spawned under `spawn_key` wants archiving once
/// its first turn ends. Best-effort: a lost intent leaves the session listed.
pub async fn remember_intent(state: &AppState, spawn_key: &str) {
    if let Err(e) = sqlx::query(
        "INSERT INTO session_auto_archive (spawn_key) VALUES ($1) ON CONFLICT (spawn_key) DO NOTHING",
    )
    .bind(spawn_key)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%spawn_key, error = %e, "could not record the auto-archive intent");
    }
}

/// Move a pending intent onto the session that just registered. The session
/// is looked up by its own id and by the spawn key the adapter echoed, so
/// both pre-minted (claude-code) and self-minted (codex) ids resolve.
pub async fn claim_intent(state: &AppState, session_id: &str, spawn_key: Option<&str>) {
    let key = spawn_key.unwrap_or(session_id);
    let claimed = match sqlx::query_scalar::<_, String>(
        "DELETE FROM session_auto_archive WHERE spawn_key = $1 OR spawn_key = $2 RETURNING spawn_key",
    )
    .bind(session_id)
    .bind(key)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row.is_some(),
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "auto-archive intent lookup failed");
            false
        }
    };
    if !claimed {
        return;
    }
    if let Err(e) = sqlx::query(
        "UPDATE sessions SET metadata = COALESCE(metadata, '{}'::jsonb) || '{\"auto_archive\": true}'::jsonb \
         WHERE id = $1",
    )
    .bind(session_id)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%session_id, error = %e, "could not flag the session for auto-archive");
    } else {
        tracing::info!(%session_id, "session armed for auto-archive on done");
    }
}

/// A flagged session is archived once the classifier reads it as done and
/// nothing says the turn failed. `activity` is the agent's own verdict:
/// `failure` and `stopped` (killed by hand) keep the session listed.
fn should_archive(
    tempo: Option<&str>,
    agent_state: Option<&str>,
    activity: Option<&str>,
    soft_limit_blocked: Option<&str>,
) -> bool {
    if matches!(activity, Some("failure" | "stopped")) {
        return false;
    }
    let bucket = crate::routes::sessions::bucket_from_signals(
        tempo,
        agent_state,
        activity,
        soft_limit_blocked,
        &[],
        &HashMap::new(),
    );
    bucket == Bucket::Done
}

type SignalRow = (String, Option<String>, Option<String>, Option<String>, Option<String>);

/// Reaper tick: archive every flagged session that is done, and drop stale
/// unclaimed intents.
pub async fn sweep(state: &AppState) {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(INTENT_TTL_SECS);
    if let Err(e) = sqlx::query("DELETE FROM session_auto_archive WHERE created_at < $1")
        .bind(cutoff)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(error = %e, "auto-archive intent prune failed");
    }

    let rows = match sqlx::query_as::<_, SignalRow>(concat!(
        "SELECT id, tempo, agent_state, activity, soft_limit_reason FROM sessions \
             WHERE ",
        live_sessions_predicate!(),
        " AND metadata->>'auto_archive' = 'true' \
               AND status NOT IN ('archived', 'draft', 'queued') \
             LIMIT $1"
    ))
    .bind(BATCH)
    .fetch_all(&state.pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "auto-archive sweep query failed");
            return;
        }
    };

    for (id, tempo, agent_state, activity, soft_limit) in rows {
        if !should_archive(
            tempo.as_deref(),
            agent_state.as_deref(),
            activity.as_deref(),
            soft_limit.as_deref(),
        ) {
            continue;
        }
        // Consume the flag first so a failed archive is retried on the next
        // tick but an un-archive by the human is never undone.
        if let Err(e) = sqlx::query(
            "UPDATE sessions SET metadata = COALESCE(metadata, '{}'::jsonb) || '{\"auto_archive\": false}'::jsonb \
             WHERE id = $1",
        )
        .bind(&id)
        .execute(&state.pool)
        .await
        {
            tracing::warn!(session_id = %id, error = %e, "could not clear the auto-archive flag");
            continue;
        }
        // Never forced: a session the human pinned since the spawn stays.
        match crate::routes::sessions::archive_one(
            state,
            &id,
            false,
            cctui_proto::adapter::RemoveInitiator::Automatic,
        )
        .await
        {
            Ok(ArchiveOutcome::Archived) => {
                tracing::info!(session_id = %id, "auto-archived a finished macro session");
            }
            Ok(ArchiveOutcome::SkippedPinned) => {
                tracing::info!(session_id = %id, "auto-archive skipped: session is pinned");
            }
            Err(e) => tracing::warn!(session_id = %id, error = %e, "auto-archive failed"),
        }
    }
}

/// When the idle-TTL sweep (`auto_archive_stale`) will archive a session:
/// `ttl_secs` after its last heartbeat, unless pinned, already archived, a
/// draft, or the sweep is disabled (`0`).
pub fn stale_archive_due(
    status: SessionStatus,
    pinned: bool,
    last_heartbeat: Option<DateTime<Utc>>,
    ttl_secs: u64,
) -> Option<DateTime<Utc>> {
    if ttl_secs == 0 || pinned || matches!(status, SessionStatus::Archived | SessionStatus::Draft) {
        return None;
    }
    last_heartbeat?.checked_add_signed(chrono::Duration::seconds(i64::try_from(ttl_secs).ok()?))
}

#[cfg(test)]
mod tests {
    use super::{should_archive, stale_archive_due};
    use cctui_proto::models::SessionStatus;

    #[test]
    fn stale_archive_is_due_one_ttl_after_the_last_heartbeat() {
        let hb = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let due = stale_archive_due(SessionStatus::Inactive, false, Some(hb), 24 * 3600);
        assert_eq!(due, Some(hb + chrono::Duration::hours(24)));
        assert!(stale_archive_due(SessionStatus::Active, false, Some(hb), 3600).is_some());
    }

    #[test]
    fn stale_archive_never_due_for_protected_sessions() {
        let hb = Some(chrono::Utc::now());
        assert_eq!(stale_archive_due(SessionStatus::Inactive, true, hb, 3600), None);
        assert_eq!(stale_archive_due(SessionStatus::Archived, false, hb, 3600), None);
        assert_eq!(stale_archive_due(SessionStatus::Draft, false, hb, 3600), None);
        assert_eq!(stale_archive_due(SessionStatus::Inactive, false, hb, 0), None);
        assert_eq!(stale_archive_due(SessionStatus::Inactive, false, None, 3600), None);
    }

    #[test]
    fn archives_a_clean_done_turn() {
        assert!(should_archive(None, Some("stopped"), None, None));
        assert!(should_archive(None, Some("done"), Some("success"), None));
        assert!(should_archive(Some("idle"), None, Some("success"), None));
    }

    #[test]
    fn keeps_working_blocked_and_failed_sessions() {
        assert!(!should_archive(None, None, None, None));
        assert!(!should_archive(Some("active"), Some("working"), None, None));
        assert!(!should_archive(Some("blocked"), None, None, None));
        assert!(!should_archive(None, Some("stopped"), None, Some("locked")));
        assert!(!should_archive(None, Some("done"), Some("failure"), None));
        assert!(!should_archive(None, Some("killed"), Some("stopped"), None));
    }
}
