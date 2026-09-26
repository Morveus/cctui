use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use cctui_proto::api::{ApiError, SpawnResponse};

use crate::auth::AuthContext;
use crate::scheduled_messages::{claim_one, deliver, parse_deliver_at};
use crate::state::AppState;

type ApiResult<T> = Result<T, (StatusCode, Json<ApiError>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: msg.into() }))
}

fn db_err(e: &sqlx::Error) -> (StatusCode, Json<ApiError>) {
    tracing::error!("db error (scheduled messages): {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
}

fn not_pending() -> (StatusCode, Json<ApiError>) {
    err(StatusCode::NOT_FOUND, "no pending scheduled message with that id")
}

fn validate_body(body: &str) -> Result<(), &'static str> {
    if body.trim().is_empty() { Err("message must not be empty") } else { Ok(()) }
}

#[derive(Debug, Deserialize)]
pub struct PatchScheduled {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub deliver_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ValidPatch {
    pub body: Option<String>,
    pub deliver_at: Option<DateTime<Utc>>,
}

pub fn validate_patch(patch: PatchScheduled, now: DateTime<Utc>) -> Result<ValidPatch, String> {
    if patch.body.is_none() && patch.deliver_at.is_none() {
        return Err("nothing to update: pass body and/or deliver_at".into());
    }
    if let Some(body) = &patch.body {
        validate_body(body)?;
    }
    let deliver_at = patch
        .deliver_at
        .as_deref()
        .map(|raw| parse_deliver_at(raw, now))
        .transpose()
        .map_err(|e| e.to_string())?;
    Ok(ValidPatch { body: patch.body, deliver_at })
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ScheduledMessage {
    pub id: uuid::Uuid,
    pub body: String,
    pub state: String,
    pub deliver_at: DateTime<Utc>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub turn_id: uuid::Uuid,
    pub origin: String,
    pub created_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
}

pub async fn schedule(
    state: &AppState,
    ctx: &AuthContext,
    session_id: &str,
    content: &str,
    raw_deliver_at: &str,
) -> ApiResult<(StatusCode, Json<SpawnResponse>)> {
    validate_body(content).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let deliver_at = parse_deliver_at(raw_deliver_at, Utc::now())
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO session_message_queue \
           (session_id, user_id, body, deliver_at, next_attempt_at, origin) \
         VALUES ($1, $2, $3, $4, $4, 'human') RETURNING id",
    )
    .bind(session_id)
    .bind(ctx.user_id)
    .bind(content)
    .bind(deliver_at)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SpawnResponse {
            command_id: id,
            status: "scheduled".into(),
            account: None,
            session_id: None,
        }),
    ))
}

/// Pending, dead-lettered, and the last day of delivered messages; the
/// delivered ones let the client mark live scheduled turns.
pub async fn list(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> ApiResult<Json<Vec<ScheduledMessage>>> {
    let rows = sqlx::query_as(
        "SELECT id, body, state, deliver_at, attempts, last_error, turn_id, origin, \
                created_at, sent_at \
         FROM session_message_queue \
         WHERE session_id = $1 \
           AND (state IN ('scheduled', 'sending', 'dead') \
                OR (state = 'sent' AND sent_at > now() - interval '1 day')) \
         ORDER BY deliver_at",
    )
    .bind(&session_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    Ok(Json(rows))
}

pub async fn update(
    State(state): State<AppState>,
    Path((session_id, queue_id)): Path<(String, uuid::Uuid)>,
    Json(patch): Json<PatchScheduled>,
) -> ApiResult<StatusCode> {
    let patch = validate_patch(patch, Utc::now()).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let res = sqlx::query(
        "UPDATE session_message_queue SET \
           body = COALESCE($3, body), \
           deliver_at = COALESCE($4, deliver_at), \
           next_attempt_at = COALESCE($4, next_attempt_at), \
           attempts = CASE WHEN $4 IS NULL THEN attempts ELSE 0 END \
         WHERE id = $1 AND session_id = $2 AND state = 'scheduled'",
    )
    .bind(queue_id)
    .bind(&session_id)
    .bind(patch.body)
    .bind(patch.deliver_at)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if res.rows_affected() == 0 {
        return Err(not_pending());
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn cancel(
    State(state): State<AppState>,
    Path((session_id, queue_id)): Path<(String, uuid::Uuid)>,
) -> ApiResult<StatusCode> {
    let res = sqlx::query(
        "UPDATE session_message_queue SET state = 'cancelled' \
         WHERE id = $1 AND session_id = $2 AND state IN ('scheduled', 'dead')",
    )
    .bind(queue_id)
    .bind(&session_id)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if res.rows_affected() == 0 {
        return Err(not_pending());
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn send_now(
    State(state): State<AppState>,
    Path((session_id, queue_id)): Path<(String, uuid::Uuid)>,
) -> ApiResult<StatusCode> {
    let row = claim_one(&state.pool, &session_id, queue_id)
        .await
        .map_err(|e| db_err(&e))?
        .ok_or_else(not_pending)?;
    deliver(&state, row).await.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn patch(body: Option<&str>, deliver_at: Option<&str>) -> PatchScheduled {
        PatchScheduled { body: body.map(Into::into), deliver_at: deliver_at.map(Into::into) }
    }

    #[test]
    fn patch_needs_a_field() {
        assert!(validate_patch(patch(None, None), now()).is_err());
    }

    #[test]
    fn patch_rejects_blank_body_and_bad_times() {
        assert!(validate_patch(patch(Some("  "), None), now()).is_err());
        assert!(validate_patch(patch(None, Some("2026-09-24T09:00:00Z")), now()).is_err());
        assert!(validate_patch(patch(None, Some("2026-11-01T00:00:00Z")), now()).is_err());
        assert!(validate_patch(patch(None, Some("tomorrow")), now()).is_err());
    }

    #[test]
    fn patch_accepts_body_and_reschedule() {
        let ok = validate_patch(patch(Some("new"), Some("2026-09-25T09:00:00Z")), now()).unwrap();
        assert_eq!(ok.body.as_deref(), Some("new"));
        assert_eq!(ok.deliver_at.map(|d| d.to_rfc3339()), Some("2026-09-25T09:00:00+00:00".into()));
    }

    #[test]
    fn schedule_body_must_not_be_blank() {
        assert!(validate_body("").is_err());
        assert!(validate_body("\n\t").is_err());
        assert!(validate_body("ping").is_ok());
    }
}
