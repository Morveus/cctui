//! A claim is a lease: a row left in `sending` by a crashed replica becomes
//! claimable again once its `next_attempt_at` passes.

use chrono::{DateTime, Duration, Utc};

use crate::state::AppState;

pub const MAX_HORIZON_DAYS: i64 = 30;
const MAX_ATTEMPTS: i32 = 8;
const BACKOFF_SECS: &[i64] = &[10, 30, 120, 300, 900, 1800, 3600];
const CLAIM_LEASE_SECS: i64 = 300;
const CLAIM_BATCH: i64 = 50;

#[derive(Debug, PartialEq, Eq)]
pub enum DeliverAtError {
    Malformed,
    Past,
    TooFar,
}

impl std::fmt::Display for DeliverAtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "deliver_at must be an RFC3339 timestamp",
            Self::Past => "deliver_at must be in the future",
            Self::TooFar => "deliver_at must be at most 30 days ahead",
        })
    }
}

pub fn parse_deliver_at(raw: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, DeliverAtError> {
    let at = DateTime::parse_from_rfc3339(raw.trim())
        .map_err(|_| DeliverAtError::Malformed)?
        .with_timezone(&Utc);
    if at <= now {
        return Err(DeliverAtError::Past);
    }
    if at > now + Duration::days(MAX_HORIZON_DAYS) {
        return Err(DeliverAtError::TooFar);
    }
    Ok(at)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Retry {
    After { attempts: i32, secs: i64 },
    Dead { attempts: i32 },
}

pub fn retry_after_failure(attempts: i32) -> Retry {
    let next = attempts + 1;
    if next >= MAX_ATTEMPTS {
        return Retry::Dead { attempts: next };
    }
    let secs = usize::try_from(attempts)
        .ok()
        .and_then(|i| BACKOFF_SECS.get(i))
        .or_else(|| BACKOFF_SECS.last())
        .copied()
        .unwrap_or(3600);
    Retry::After { attempts: next, secs }
}

/// Reason a message can never be delivered to a session in this status.
pub fn undeliverable_reason(session_status: Option<&str>) -> Option<&'static str> {
    match session_status {
        None => Some("session no longer exists"),
        Some("ended") => Some("session ended before delivery"),
        Some("archived") => Some("session archived before delivery"),
        Some("failed") => Some("session failed to launch"),
        _ => None,
    }
}

#[derive(sqlx::FromRow)]
pub struct ClaimedRow {
    pub id: uuid::Uuid,
    pub session_id: String,
    pub body: String,
    pub ask_picks: Option<serde_json::Value>,
    pub attempts: i32,
    pub turn_id: uuid::Uuid,
    pub session_status: Option<String>,
}

const CLAIM_SQL: &str = "\
    WITH claimed AS ( \
        UPDATE session_message_queue q \
        SET state = 'sending', next_attempt_at = now() + ($1 || ' seconds')::interval \
        WHERE q.id IN ( \
            SELECT id FROM session_message_queue \
            WHERE (state = 'scheduled' AND deliver_at <= now() AND next_attempt_at <= now()) \
               OR (state = 'sending' AND next_attempt_at <= now()) \
            ORDER BY deliver_at \
            LIMIT $2 \
            FOR UPDATE SKIP LOCKED) \
        RETURNING q.id, q.session_id, q.body, q.ask_picks, q.attempts, q.turn_id) \
    SELECT c.*, s.status AS session_status \
    FROM claimed c LEFT JOIN sessions s ON s.id = c.session_id";

const CLAIM_ONE_SQL: &str = "\
    WITH claimed AS ( \
        UPDATE session_message_queue \
        SET state = 'sending', next_attempt_at = now() + ($3 || ' seconds')::interval \
        WHERE id = $1 AND session_id = $2 AND state = 'scheduled' \
        RETURNING id, session_id, body, ask_picks, attempts, turn_id) \
    SELECT c.*, s.status AS session_status \
    FROM claimed c LEFT JOIN sessions s ON s.id = c.session_id";

pub async fn claim_due(pool: &sqlx::PgPool) -> sqlx::Result<Vec<ClaimedRow>> {
    sqlx::query_as(CLAIM_SQL)
        .bind(CLAIM_LEASE_SECS.to_string())
        .bind(CLAIM_BATCH)
        .fetch_all(pool)
        .await
}

pub async fn claim_one(
    pool: &sqlx::PgPool,
    session_id: &str,
    id: uuid::Uuid,
) -> sqlx::Result<Option<ClaimedRow>> {
    sqlx::query_as(CLAIM_ONE_SQL)
        .bind(id)
        .bind(session_id)
        .bind(CLAIM_LEASE_SECS.to_string())
        .fetch_optional(pool)
        .await
}

pub async fn scheduled_turns(
    pool: &sqlx::PgPool,
    session_id: &str,
) -> sqlx::Result<std::collections::HashMap<uuid::Uuid, DateTime<Utc>>> {
    let rows: Vec<(uuid::Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT turn_id, deliver_at FROM session_message_queue \
         WHERE session_id = $1 AND state = 'sent'",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn sweep(state: &AppState) {
    let rows = match claim_due(&state.pool).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("scheduled message claim failed: {e}");
            return;
        }
    };
    for row in rows {
        let _ = deliver(state, row).await;
    }
}

pub async fn deliver(state: &AppState, row: ClaimedRow) -> Result<(), String> {
    if let Some(reason) = undeliverable_reason(row.session_status.as_deref()) {
        mark_dead(&state.pool, row.id, row.attempts + 1, reason).await;
        return Err(reason.to_owned());
    }
    let env = crate::routes::gateway::resume_env_for_session(state, &row.session_id).await;
    let command_id = uuid::Uuid::new_v4();
    crate::state::track_command(
        &state.pending_commands,
        command_id,
        Some(row.session_id.clone()),
        None,
    );
    let ask_picks = row.ask_picks.and_then(|v| serde_json::from_value(v).ok());
    let dispatch = crate::bus::dispatch(
        state,
        &row.session_id,
        cctui_proto::adapter::AdapterCommand::Reply {
            local_id: row.session_id.clone(),
            text: row.body,
            ask_picks,
            env,
            command_id: Some(command_id),
            turn_id: Some(row.turn_id),
        },
    )
    .await;
    match dispatch {
        Ok(()) => {
            let _ = sqlx::query(
                "UPDATE session_message_queue \
                 SET state = 'sent', sent_at = now(), last_error = NULL WHERE id = $1",
            )
            .bind(row.id)
            .execute(&state.pool)
            .await;
            Ok(())
        }
        Err(err) => {
            let err = err.to_string();
            record_failure(&state.pool, row.id, row.attempts, &err).await;
            Err(err)
        }
    }
}

pub async fn record_failure(pool: &sqlx::PgPool, id: uuid::Uuid, attempts: i32, err: &str) {
    match retry_after_failure(attempts) {
        Retry::Dead { attempts } => {
            mark_dead(pool, id, attempts, err).await;
            tracing::error!(queue_id = %id, attempts, "scheduled message dead-lettered: {err}");
        }
        Retry::After { attempts, secs } => {
            let _ = sqlx::query(
                "UPDATE session_message_queue \
                 SET state = 'scheduled', attempts = $2, last_error = $3, \
                     next_attempt_at = now() + ($4 || ' seconds')::interval \
                 WHERE id = $1",
            )
            .bind(id)
            .bind(attempts)
            .bind(err)
            .bind(secs.to_string())
            .execute(pool)
            .await;
            tracing::warn!(queue_id = %id, attempt = attempts, retry_in_secs = secs, "scheduled message delivery failed: {err}");
        }
    }
}

async fn mark_dead(pool: &sqlx::PgPool, id: uuid::Uuid, attempts: i32, reason: &str) {
    let _ = sqlx::query(
        "UPDATE session_message_queue SET state = 'dead', attempts = $2, last_error = $3 \
         WHERE id = $1",
    )
    .bind(id)
    .bind(attempts)
    .bind(reason)
    .execute(pool)
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn deliver_at_must_be_future_and_within_horizon() {
        assert_eq!(parse_deliver_at("nope", now()), Err(DeliverAtError::Malformed));
        assert_eq!(parse_deliver_at("2026-09-24T10:00:00Z", now()), Err(DeliverAtError::Past));
        assert_eq!(parse_deliver_at("2026-09-24T09:00:00Z", now()), Err(DeliverAtError::Past));
        assert_eq!(parse_deliver_at("2026-10-24T10:00:01Z", now()), Err(DeliverAtError::TooFar));
        assert_eq!(parse_deliver_at("2026-10-24T10:00:00Z", now()), Ok(now() + Duration::days(30)));
        assert_eq!(
            parse_deliver_at("2026-09-25T09:00:00+02:00", now()),
            Ok(now() + Duration::hours(21))
        );
    }

    #[test]
    fn failures_back_off_then_dead_letter() {
        assert_eq!(retry_after_failure(0), Retry::After { attempts: 1, secs: 10 });
        assert_eq!(retry_after_failure(1), Retry::After { attempts: 2, secs: 30 });
        assert_eq!(retry_after_failure(6), Retry::After { attempts: 7, secs: 3600 });
        assert_eq!(retry_after_failure(7), Retry::Dead { attempts: 8 });
        assert_eq!(retry_after_failure(20), Retry::Dead { attempts: 21 });
    }

    #[test]
    fn ended_and_archived_sessions_are_undeliverable() {
        assert!(undeliverable_reason(Some("ended")).is_some());
        assert!(undeliverable_reason(Some("archived")).is_some());
        assert!(undeliverable_reason(Some("failed")).is_some());
        assert!(undeliverable_reason(None).is_some());
        assert_eq!(undeliverable_reason(Some("active")), None);
        assert_eq!(undeliverable_reason(Some("inactive")), None);
    }

    async fn seed(pool: &sqlx::PgPool, name: &str) -> String {
        let uid = uuid::Uuid::new_v4();
        let machine = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("{name}-{uid}"))
            .bind(format!("h-{uid}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO machines (id, user_id, name, key_hash) VALUES ($1, $2, 'm', $3)")
            .bind(machine)
            .bind(uid)
            .bind(format!("mk-{machine}"))
            .execute(pool)
            .await
            .unwrap();
        let sid = format!("{name}-{uid}");
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, machine_uuid, user_id, working_dir, status, \
             adapter_id) VALUES ($1, $2, $2, $3, '/w', 'active', 'claude-code')",
        )
        .bind(&sid)
        .bind(machine)
        .bind(uid)
        .execute(pool)
        .await
        .unwrap();
        sid
    }

    async fn insert(pool: &sqlx::PgPool, sid: &str, due: bool) -> uuid::Uuid {
        let offset = if due { "-1 minute" } else { "1 hour" };
        sqlx::query_scalar(
            "INSERT INTO session_message_queue (session_id, body, deliver_at, next_attempt_at) \
             VALUES ($1, 'hi', now() + $2::interval, now() + $2::interval) RETURNING id",
        )
        .bind(sid)
        .bind(offset)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn due_rows_are_claimed_exactly_once_and_backoff_releases_them() {
        let name = "scheduled_claim_exactly_once";
        let Some(url) = crate::routes::gateway::test_db_url(name) else { return };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect test db");
        let sid = seed(&pool, name).await;
        let due = insert(&pool, &sid, true).await;
        let later = insert(&pool, &sid, false).await;

        let (a, b) = tokio::join!(claim_due(&pool), claim_due(&pool));
        let mine: Vec<uuid::Uuid> = a
            .unwrap()
            .into_iter()
            .chain(b.unwrap())
            .filter(|r| r.session_id == sid)
            .map(|r| r.id)
            .collect();
        assert_eq!(mine, vec![due], "the due row is claimed once, the future row not at all");
        assert!(claim_due(&pool).await.unwrap().iter().all(|r| r.id != due), "leased");
        assert!(
            claim_one(&pool, &sid, due).await.unwrap().is_none(),
            "send-now skips a claimed row"
        );

        record_failure(&pool, due, 0, "no daemon").await;
        let (st, attempts, err): (String, i32, Option<String>) = sqlx::query_as(
            "SELECT state, attempts, last_error FROM session_message_queue WHERE id = $1",
        )
        .bind(due)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((st.as_str(), attempts, err.as_deref()), ("scheduled", 1, Some("no daemon")));
        assert!(claim_due(&pool).await.unwrap().iter().all(|r| r.id != due), "backing off");

        record_failure(&pool, due, MAX_ATTEMPTS - 1, "still offline").await;
        let st: String =
            sqlx::query_scalar("SELECT state FROM session_message_queue WHERE id = $1")
                .bind(due)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(st, "dead");

        let one = claim_one(&pool, &sid, later).await.unwrap().expect("send-now claims early");
        assert_eq!(one.session_status.as_deref(), Some("active"));
    }
}
