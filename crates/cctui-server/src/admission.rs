//! Spawn admission: hold a launch back while its machine is over the RAM
//! ceiling its operator set, instead of letting a burst freeze the host.
//!
//! The ceiling is per machine (`machines.mem_ceiling_bytes`) and `NULL` by
//! default, which means no ceiling: a machine nobody configured behaves
//! exactly as before, with no bookkeeping at all.
//!
//! With a ceiling, a spawn is let through when the machine's memory in use,
//! plus an estimate for the launches let through in the last
//! [`RECENT_SECS`] seconds (a fresh session grows well after its dispatch, so
//! the last heartbeat has not seen it yet), plus the estimate for this one,
//! stays under the ceiling. Otherwise it becomes a `queued` session row, and
//! [`drain`] (called from the reaper, every 30 s) launches the queue of each
//! machine oldest first, as long as the same test passes.
//!
//! The decision runs under a row lock on the machine, so two replicas or two
//! concurrent requests never let the same headroom through twice.
//!
//! One launch is sent at most once: a row is marked `sending` (committed)
//! before anything goes out, and such a row is never picked up again. Handing
//! the command to the daemon's connection proves nothing about its arrival, so
//! the request is kept (`sent`) until the daemon's receipt for its
//! `command_id`, or until its session registers. Anything else, a broken send,
//! a server that died mid-attempt, a receipt that never comes, leaves an
//! outcome nobody can decide: the row goes to `uncertain`, keeping the request
//! whole, and only a human sends it again. See `docs/spawn-admission.md`.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use cctui_proto::adapter::BootstrapFile;
use cctui_proto::api::{ApiError, SpawnRequest, SpawnResponse};
use cctui_proto::models::{MachineLiveness, SessionStatus};
use cctui_proto::ws::ServerEvent;
use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::state::AppState;

/// What one freshly started session is expected to add, measured at 1.3 to
/// 1.4 GiB for an idle Claude Code session with its stdio MCP servers.
pub const SESSION_ESTIMATE_BYTES: u64 = 1536 * 1024 * 1024;

/// How long a launch counts as "not yet visible in the heartbeat figures".
const RECENT_SECS: i64 = 120;

/// A snapshot older than this says nothing about the machine: admit rather
/// than hold a launch forever behind a silent daemon.
const STALE_SNAPSHOT_SECS: i64 = 300;

/// Most launches one machine gets per reaper tick, even with room to spare.
const DRAIN_PER_TICK: usize = 10;

type ApiErr = (StatusCode, Json<ApiError>);

fn db_err(e: &sqlx::Error) -> ApiErr {
    tracing::error!("db error (admission): {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
}

/// The figures behind one admission decision, stored on a queued session's
/// `metadata.queued` so the UI can say what it waits for. All in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Figures {
    pub mem_used: u64,
    pub mem_total: u64,
    /// Estimate for the launches let through in the last [`RECENT_SECS`].
    pub recent: u64,
    pub ceiling: u64,
}

impl Figures {
    /// Whether one more session fits under the ceiling.
    #[must_use]
    pub const fn admits(&self) -> bool {
        self.mem_used.saturating_add(self.recent).saturating_add(SESSION_ESTIMATE_BYTES)
            <= self.ceiling
    }

    fn to_json(self) -> serde_json::Value {
        json!({
            "mem_used_bytes": self.mem_used,
            "mem_total_bytes": self.mem_total,
            "recent_bytes": self.recent,
            "ceiling_bytes": self.ceiling,
            "estimate_bytes": SESSION_ESTIMATE_BYTES,
            "checked_at": chrono::Utc::now().to_rfc3339(),
        })
    }
}

/// The outcome of [`decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No ceiling, no usable snapshot, or room left: launch now.
    Admit,
    /// Over the ceiling (or behind older queued spawns): queue it.
    Hold(Figures),
}

fn to_u64(v: i64) -> u64 {
    v.max(0).unsigned_abs()
}

/// Decide for one launch on `machine`, and when it is admitted under a
/// ceiling, record it so the next decision counts it. `jump_queue` is for the
/// drain itself (it launches the head of the queue) and for an explicit
/// "launch now": without it, a spawn waits behind older queued ones.
pub async fn decide(
    pool: &sqlx::PgPool,
    machine: Uuid,
    jump_queue: bool,
) -> Result<Decision, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let ceiling: Option<Option<i64>> =
        sqlx::query_scalar("SELECT mem_ceiling_bytes FROM machines WHERE id = $1 FOR UPDATE")
            .bind(machine)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(Some(ceiling)) = ceiling else {
        return Ok(Decision::Admit);
    };
    let snapshot: Option<(i64, i64)> = sqlx::query_as(
        "SELECT mem_used_bytes, mem_total_bytes FROM machine_resources \
         WHERE machine_id = $1 AND updated_at > now() - make_interval(secs => $2)",
    )
    .bind(machine)
    .bind(STALE_SNAPSHOT_SECS as f64)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((used, total)) = snapshot else {
        tracing::warn!(%machine, "RAM ceiling set but no fresh resource snapshot: admitting");
        return Ok(Decision::Admit);
    };
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM machine_admissions \
         WHERE machine_uuid = $1 AND admitted_at > now() - make_interval(secs => $2)",
    )
    .bind(machine)
    .bind(RECENT_SECS as f64)
    .fetch_one(&mut *tx)
    .await?;
    let figures = Figures {
        mem_used: to_u64(used),
        mem_total: to_u64(total),
        recent: to_u64(recent).saturating_mul(SESSION_ESTIMATE_BYTES),
        ceiling: to_u64(ceiling),
    };
    let queue_ahead = if jump_queue {
        false
    } else {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM spawn_queue WHERE machine_uuid = $1)")
            .bind(machine)
            .fetch_one(&mut *tx)
            .await?
    };
    if queue_ahead || !figures.admits() {
        return Ok(Decision::Hold(figures));
    }
    sqlx::query("INSERT INTO machine_admissions (machine_uuid) VALUES ($1)")
        .bind(machine)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Decision::Admit)
}

/// Undo the latest admission record of `machine`: the launch it stood for did
/// not go out, so it must not weigh on the next decisions.
async fn forget_admission(pool: &sqlx::PgPool, machine: Uuid) {
    let res = sqlx::query(
        "DELETE FROM machine_admissions WHERE ctid = (SELECT ctid FROM machine_admissions \
         WHERE machine_uuid = $1 ORDER BY admitted_at DESC LIMIT 1)",
    )
    .bind(machine)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(%e, %machine, "could not forget an admission");
    }
}

/// The spawn entry point for callers that may be held back: dispatch now when
/// the machine admits it, otherwise store it as a `queued` session and answer
/// `202` with `status = "queued"`.
pub async fn spawn_or_queue(
    state: &AppState,
    ctx: &AuthContext,
    req: SpawnRequest,
    uploads: Vec<BootstrapFile>,
) -> Result<(StatusCode, Json<SpawnResponse>), ApiErr> {
    let machine = crate::routes::spawn::resolve_owned_machine(state, ctx, &req.machine_id).await?;
    match decide(&state.pool, machine, false).await.map_err(|e| db_err(&e))? {
        Decision::Admit => {
            let out = crate::routes::spawn::dispatch_spawn(state, ctx, req, uploads).await;
            if out.is_err() {
                forget_admission(&state.pool, machine).await;
            }
            out
        }
        Decision::Hold(figures) => enqueue(state, ctx, machine, req, uploads, figures).await,
    }
}

async fn enqueue(
    state: &AppState,
    ctx: &AuthContext,
    machine: Uuid,
    mut req: SpawnRequest,
    uploads: Vec<BootstrapFile>,
    figures: Figures,
) -> Result<(StatusCode, Json<SpawnResponse>), ApiErr> {
    let adapter_id = req.adapter_id.clone().unwrap_or_else(|| "claude-code".to_owned());
    let owner: Uuid = sqlx::query_scalar("SELECT user_id FROM machines WHERE id = $1")
        .bind(machine)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| db_err(&e))?;
    // Secrets never sit in clear at rest: the env is sealed with the vault key.
    let env_enc = if req.env.is_empty() {
        None
    } else {
        let plain = serde_json::to_string(&req.env).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError { error: "could not encode env".into() }),
            )
        })?;
        Some(crate::crypto::encrypt(&plain, &crate::crypto::vault_key()))
    };
    req.env.clear();
    let request = serde_json::to_value(&req).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError { error: "could not encode request".into() }),
        )
    })?;
    let uploads = (!uploads.is_empty())
        .then(|| serde_json::to_value(&uploads))
        .transpose()
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError { error: "could not encode uploads".into() }),
            )
        })?;

    let id = Uuid::new_v4();
    let name = req.name.as_deref().filter(|n| !n.trim().is_empty());
    let model = req.model.as_deref().filter(|m| !m.trim().is_empty());
    let effort = req.effort.as_deref().filter(|e| !e.trim().is_empty());
    let metadata = json!({ "queued": figures.to_json() });
    let mut tx = state.pool.begin().await.map_err(|e| db_err(&e))?;
    sqlx::query(
        r"INSERT INTO sessions
            (id, machine_id, machine_uuid, user_id, working_dir, status, registered_at,
             last_heartbeat, metadata, adapter_id, session_name, model, effort)
          VALUES ($1, $2, $3, $4, $5, 'queued', now(), now(), $6, $7, $8, $9, $10)",
    )
    .bind(id.to_string())
    .bind(machine.to_string())
    .bind(machine)
    .bind(owner)
    .bind(&req.working_dir)
    .bind(&metadata)
    .bind(&adapter_id)
    .bind(name)
    .bind(model)
    .bind(effort)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_err(&e))?;
    sqlx::query(
        "INSERT INTO spawn_queue \
           (session_id, machine_uuid, caller_id, caller_key_id, request, env_enc, uploads, \
            command_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $1::uuid)",
    )
    .bind(id.to_string())
    .bind(machine)
    .bind(ctx.user_id)
    .bind(ctx.key_id)
    .bind(&request)
    .bind(env_enc)
    .bind(uploads)
    .execute(&mut *tx)
    .await
    .map_err(|e| db_err(&e))?;
    tx.commit().await.map_err(|e| db_err(&e))?;

    state.bus.publish_server(ServerEvent::Status {
        session_id: id.to_string(),
        status: SessionStatus::Queued,
    });
    tracing::info!(
        %machine,
        session = %id,
        mem_used = figures.mem_used,
        recent = figures.recent,
        ceiling = figures.ceiling,
        "spawn queued: machine over its RAM ceiling"
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(SpawnResponse {
            command_id: id,
            status: "queued".into(),
            account: None,
            // claude-code sessions register under this id once launched.
            session_id: (adapter_id == "claude-code").then_some(id),
        }),
    ))
}

#[derive(sqlx::FromRow)]
struct QueuedRow {
    session_id: String,
    machine_uuid: Uuid,
    caller_id: Uuid,
    caller_key_id: Uuid,
    request: serde_json::Value,
    env_enc: Option<String>,
    uploads: Option<serde_json::Value>,
    command_id: Uuid,
}

/// Why one dispatch attempt did not launch.
#[derive(Debug)]
enum DispatchError {
    /// Nothing was sent (daemon unreachable, database hiccup): try again later.
    NotSent(String),
    /// Nothing was sent, and never will be as stored: revoked caller, lost
    /// rights, gone machine or account, corrupt payload.
    Rejected(String),
    /// The send itself broke: the daemon may or may not have the command.
    Unknown(String),
}

/// What became of one launch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// Handed to the daemon's connection. Not proof it arrived: the request
    /// is kept until the receipt, or until its session registers.
    Sent,
    /// Nothing was sent; still waiting, tried again later.
    Retry(String),
    /// Nothing was sent and nothing will be: ended as `spawn_failed`.
    Failed(String),
    /// The send broke: nobody can say whether the daemon got it. The request
    /// is kept, nothing is sent again on its own, a human settles it.
    Uncertain(String),
    /// Not waiting (any more): absent, in flight, or already uncertain.
    NotLaunchable,
}

/// What a session in doubt carries, on its `metadata.launch_uncertain`, and
/// what the UI shows: nothing was lost, nothing was resent.
const UNCERTAIN_HELP: &str = "the launch was interrupted and may or may not have reached the \
                              machine: check there, then relaunch or cancel it";

/// An attempt still unacknowledged after this had its server die under it, or
/// its daemon never answer (a receipt takes seconds): the reaper settles it.
const ATTEMPT_GRACE_SECS: f64 = 600.0;

/// Take the row for one attempt, marking it in flight. The mark is committed
/// before anything is sent, so a second attempt can never start behind this
/// one's back: the command goes out at most once per row.
async fn claim(pool: &sqlx::PgPool, session_id: &str) -> Result<Option<QueuedRow>, sqlx::Error> {
    sqlx::query_as(
        "UPDATE spawn_queue SET state = 'sending', attempt_since = now() \
         WHERE session_id = $1 AND state = 'waiting' \
         RETURNING session_id, machine_uuid, caller_id, caller_key_id, request, env_enc, \
                   uploads, command_id",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

/// Put a row back in the queue: only for an attempt that sent nothing.
async fn release(pool: &sqlx::PgPool, session_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE spawn_queue SET state = 'waiting', attempt_since = NULL WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Proof of launch: drop the queue row and the placeholder. Only ever called
/// on the daemon's receipt, or on a session of its own registering. The live
/// session keeps its row (the same id for claude-code); if it already
/// registered, the status is no longer `queued` and only the figures go.
async fn finish_launched(pool: &sqlx::PgPool, session_id: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM spawn_queue WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE id = $1 AND status = 'queued'")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE sessions SET metadata = metadata - 'queued' - 'launch_uncertain' WHERE id = $1",
    )
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Nothing was sent and nothing will be: the placeholder becomes an ended
/// `spawn_failed` session carrying why, so the human sees it. Only ever used
/// when the outcome is known, never on a doubt.
async fn end_failed(pool: &sqlx::PgPool, session_id: &str, why: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM spawn_queue WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE sessions SET status = 'ended', ended_at = now(), \
           end_reason = 'spawn_failed', end_detail = $2, metadata = metadata - 'queued' \
         WHERE id = $1 AND status = 'queued'",
    )
    .bind(session_id)
    .bind(why)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// The command was handed to the daemon's connection. That is an in-memory
/// channel, not an acknowledgement: the request is kept, in `sent`, until the
/// daemon's receipt for its `command_id` or until its session registers. A
/// `sent` row is never picked up by a drain.
async fn mark_sent(pool: &sqlx::PgPool, session_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE spawn_queue SET state = 'sent', attempt_since = now() WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Nobody can say whether the daemon got this one. The request stays whole in
/// the queue, in `uncertain`: no drain will send it again, and the session is
/// flagged for a human to check the machine and then relaunch or cancel.
async fn mark_uncertain(
    pool: &sqlx::PgPool,
    session_id: &str,
    why: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE spawn_queue SET state = 'uncertain', attempt_since = NULL WHERE session_id = $1",
    )
    .bind(session_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE sessions SET metadata = jsonb_set(metadata, '{launch_uncertain}', $2) \
         WHERE id = $1 AND status = 'queued'",
    )
    .bind(session_id)
    .bind(json!({ "why": why, "help": UNCERTAIN_HELP, "since": chrono::Utc::now().to_rfc3339() }))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::warn!(session = %session_id, %why, "queued spawn in doubt: kept, never resent alone");
    Ok(())
}

/// One attempt: claim, send through `dispatch`, then settle the row by what
/// the send says. Only a send known not to have happened is ever retried.
async fn launch_with<F>(
    pool: &sqlx::PgPool,
    session_id: &str,
    dispatch: F,
) -> Result<LaunchOutcome, sqlx::Error>
where
    F: AsyncFnOnce(QueuedRow) -> Result<(), DispatchError>,
{
    let Some(row) = claim(pool, session_id).await? else {
        return Ok(LaunchOutcome::NotLaunchable);
    };
    match dispatch(row).await {
        Ok(()) => {
            mark_sent(pool, session_id).await?;
            Ok(LaunchOutcome::Sent)
        }
        Err(DispatchError::NotSent(why)) => {
            release(pool, session_id).await?;
            Ok(LaunchOutcome::Retry(why))
        }
        Err(DispatchError::Rejected(why)) => {
            end_failed(pool, session_id, &why).await?;
            Ok(LaunchOutcome::Failed(why))
        }
        Err(DispatchError::Unknown(why)) => {
            mark_uncertain(pool, session_id, &why).await?;
            Ok(LaunchOutcome::Uncertain(why))
        }
    }
}

/// [`launch_with`] for an explicit "launch now": the RAM it is about to take
/// is recorded only when the command did go out.
async fn launch_now_with<F>(
    pool: &sqlx::PgPool,
    session_id: &str,
    dispatch: F,
) -> Result<LaunchOutcome, sqlx::Error>
where
    F: AsyncFnOnce(QueuedRow) -> Result<(), DispatchError>,
{
    let machine: Option<Uuid> =
        sqlx::query_scalar("SELECT machine_uuid FROM spawn_queue WHERE session_id = $1")
            .bind(session_id)
            .fetch_optional(pool)
            .await?;
    let Some(machine) = machine else { return Ok(LaunchOutcome::NotLaunchable) };
    let outcome = launch_with(pool, session_id, dispatch).await?;
    if outcome == LaunchOutcome::Sent {
        sqlx::query("INSERT INTO machine_admissions (machine_uuid) VALUES ($1)")
            .bind(machine)
            .execute(pool)
            .await?;
    }
    Ok(outcome)
}

/// A human decides to send an uncertain launch again, having checked the
/// machine: back to `waiting`, for the drain or for "launch now".
pub async fn retry_uncertain(pool: &sqlx::PgPool, session_id: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE spawn_queue SET state = 'waiting', attempt_since = NULL \
         WHERE session_id = $1 AND state = 'uncertain'",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE sessions SET metadata = metadata - 'launch_uncertain' WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(true)
}

/// The queued launch a receipt belongs to, if any: receipts also come back
/// for every spawn that never went through the queue.
async fn queued_of_command(
    pool: &sqlx::PgPool,
    command_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT session_id FROM spawn_queue WHERE command_id = $1")
        .bind(command_id)
        .fetch_optional(pool)
        .await
}

/// The daemon's receipt for a launch cctui sent: the proof the queue waits
/// for. `ok` settles the row as launched; a refusal ends the placeholder with
/// what the daemon said, since that one did not start. Unknown command ids
/// (every spawn that never went through the queue) are ignored.
pub async fn settle_receipt(state: &AppState, command_id: Uuid, ok: bool, error: Option<&str>) {
    let session_id = match queued_of_command(&state.pool, command_id).await {
        Ok(Some(id)) => id,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%e, %command_id, "could not look up the queued launch of a receipt");
            return;
        }
    };
    let settled = if ok {
        finish_launched(&state.pool, &session_id).await
    } else {
        let why = error.unwrap_or("the machine refused the launch");
        end_failed(&state.pool, &session_id, why).await
    };
    match settled {
        Ok(()) => {
            tracing::info!(session = %session_id, %command_id, ok, "queued launch acknowledged");
            state.bus.publish_server(ServerEvent::SessionDeregistered { session_id });
        }
        Err(e) => tracing::warn!(%e, session = %session_id, "settling an acknowledged launch"),
    }
}

/// Reconcile what the attempts left behind.
///
/// A session that registered under the queued id did start (claude-code
/// launches under the id its row was shown with): its placeholder is settled
/// as launched, never as failed. An attempt left in flight, or sent and never
/// acknowledged, becomes `uncertain`: the request is kept whole and nothing is
/// sent again without a human. Returns the ids settled.
async fn reconcile(pool: &sqlx::PgPool) -> Result<Vec<String>, sqlx::Error> {
    let mut settled = Vec::new();
    // Whatever their state, rows whose session is live did start.
    let started: Vec<String> = sqlx::query_scalar(
        "SELECT q.session_id FROM spawn_queue q JOIN sessions s ON s.id = q.session_id \
         WHERE s.status <> 'queued'",
    )
    .fetch_all(pool)
    .await?;
    for id in started {
        finish_launched(pool, &id).await?;
        tracing::info!(session = %id, "queued spawn reconciled: its session is live");
        settled.push(id);
    }
    // In flight with no server left to finish it, or handed over and never
    // acknowledged: nobody can say whether the machine got it. Kept in doubt
    // rather than lost or called failed.
    let stranded: Vec<String> = sqlx::query_scalar(
        "SELECT session_id FROM spawn_queue \
         WHERE state IN ('sending', 'sent') \
           AND attempt_since < now() - make_interval(secs => $1)",
    )
    .bind(ATTEMPT_GRACE_SECS)
    .fetch_all(pool)
    .await?;
    for id in stranded {
        mark_uncertain(pool, &id, "the machine never acknowledged the launch").await?;
        settled.push(id);
    }
    Ok(settled)
}

/// The next row of `machine` the drain may try: oldest first, waiting only
/// (never one in flight, never one in doubt).
async fn next_head(pool: &sqlx::PgPool, machine: Uuid) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT session_id FROM spawn_queue WHERE machine_uuid = $1 AND state = 'waiting' \
         ORDER BY queued_at LIMIT 1",
    )
    .bind(machine)
    .fetch_optional(pool)
    .await
}

/// Send one claimed row as the caller who queued it, with the rights that
/// caller holds now, not those it held when it queued.
async fn dispatch_row(state: &AppState, row: QueuedRow) -> Result<(), DispatchError> {
    use crate::routes::spawn::{DAEMON_OFFLINE, DISCONNECTED_MID_DISPATCH};

    let mut req: SpawnRequest = serde_json::from_value(row.request)
        .map_err(|e| DispatchError::Rejected(format!("corrupt queued request: {e}")))?;
    if let Some(enc) = &row.env_enc {
        let plain = crate::crypto::decrypt(enc, &crate::crypto::vault_key())
            .ok_or_else(|| DispatchError::Rejected("queued env could not be decrypted".into()))?;
        req.env = serde_json::from_str::<BTreeMap<String, String>>(&plain)
            .map_err(|e| DispatchError::Rejected(format!("corrupt queued env: {e}")))?;
    }
    let uploads: Vec<BootstrapFile> = match row.uploads {
        Some(v) => serde_json::from_value(v)
            .map_err(|e| DispatchError::Rejected(format!("corrupt queued uploads: {e}")))?,
        None => Vec::new(),
    };
    let not_sent = |e: sqlx::Error| DispatchError::NotSent(format!("database error: {e}"));
    let ctx = state
        .auth_config
        .revalidate(row.caller_id, row.caller_key_id)
        .await
        .map_err(not_sent)?
        .ok_or_else(|| {
            DispatchError::Rejected(
                "the key that queued this launch is no longer valid (revoked, expired or its \
                 user disabled)"
                    .into(),
            )
        })?;
    let machine: Option<(bool, DateTime<Utc>)> = sqlx::query_as(
        "SELECT revoked_at IS NULL AND deleted_at IS NULL, last_seen_at FROM machines \
         WHERE id = $1",
    )
    .bind(row.machine_uuid)
    .fetch_optional(&state.pool)
    .await
    .map_err(not_sent)?;
    let Some((machine_live, last_seen)) = machine else {
        return Err(DispatchError::Rejected("machine not found".into()));
    };
    if !machine_live {
        return Err(DispatchError::Rejected("machine revoked or deleted".into()));
    }
    // Nothing reaches a daemon nobody holds: that is the retryable case. A
    // failure of the send itself is not, whatever it says.
    let reachable = state.bus.daemon_connected(row.machine_uuid)
        || crate::machine_liveness::derive(last_seen) == MachineLiveness::Online;
    if !reachable {
        return Err(DispatchError::NotSent(DAEMON_OFFLINE.into()));
    }
    let preset = Uuid::parse_str(&row.session_id).ok();
    let sent = crate::routes::spawn::dispatch_spawn_as(
        state,
        &ctx,
        req,
        uploads,
        preset,
        Some(row.command_id),
    )
    .await;
    match sent {
        Ok(_) => Ok(()),
        Err((_, Json(err)))
            if err.error == DAEMON_OFFLINE || err.error == DISCONNECTED_MID_DISPATCH =>
        {
            Err(DispatchError::Unknown(err.error))
        }
        Err((code, Json(err))) if code.is_server_error() => Err(DispatchError::NotSent(err.error)),
        Err((_, Json(err))) => Err(DispatchError::Rejected(err.error)),
    }
}

/// Tell the UIs what happened to the row, and log it.
fn announce(state: &AppState, session_id: &str, outcome: &LaunchOutcome) {
    match outcome {
        LaunchOutcome::Sent => {
            tracing::info!(session = %session_id, "queued spawn sent, waiting for its receipt");
        }
        LaunchOutcome::Failed(why) => {
            tracing::warn!(session = %session_id, %why, "queued spawn dropped");
        }
        LaunchOutcome::Retry(why) => {
            tracing::debug!(session = %session_id, %why, "queued spawn not launched yet");
        }
        LaunchOutcome::Uncertain(_) | LaunchOutcome::NotLaunchable => {}
    }
    match outcome {
        LaunchOutcome::Failed(_) => {
            state.bus.publish_server(ServerEvent::SessionDeregistered {
                session_id: session_id.to_owned(),
            });
        }
        LaunchOutcome::Sent | LaunchOutcome::Uncertain(_) => {
            state.bus.publish_server(ServerEvent::Status {
                session_id: session_id.to_owned(),
                status: SessionStatus::Queued,
            });
        }
        LaunchOutcome::Retry(_) | LaunchOutcome::NotLaunchable => {}
    }
}

async fn launch(state: &AppState, session_id: &str) -> Result<LaunchOutcome, sqlx::Error> {
    let outcome =
        launch_with(&state.pool, session_id, async |row| dispatch_row(state, row).await).await?;
    announce(state, session_id, &outcome);
    Ok(outcome)
}

/// `POST /sessions/{id}/launch` on a queued session: the human overrides the
/// ceiling and launches it now. A session in doubt is sent again only here,
/// on that explicit decision, after the human has checked the machine.
pub async fn launch_now(state: &AppState, session_id: &str) -> Result<LaunchOutcome, sqlx::Error> {
    retry_uncertain(&state.pool, session_id).await?;
    let outcome =
        launch_now_with(&state.pool, session_id, async |row| dispatch_row(state, row).await)
            .await?;
    announce(state, session_id, &outcome);
    Ok(outcome)
}

/// Reaper step: reconcile what the last attempts left behind, launch what each
/// machine's ceiling now lets through, oldest first, refresh the figures shown
/// on what still waits, and forget old admission records.
pub async fn drain(state: &AppState) {
    if let Err(e) = drain_inner(state).await {
        tracing::warn!(%e, "spawn queue drain failed");
    }
}

async fn drain_inner(state: &AppState) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM machine_admissions WHERE admitted_at < now() - interval '1 hour'")
        .execute(&state.pool)
        .await?;
    for id in reconcile(&state.pool).await? {
        state.bus.publish_server(ServerEvent::SessionDeregistered { session_id: id });
    }
    let machines: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT machine_uuid FROM spawn_queue WHERE state = 'waiting'")
            .fetch_all(&state.pool)
            .await?;
    for machine in machines {
        for _ in 0..DRAIN_PER_TICK {
            let Some(head) = next_head(&state.pool, machine).await? else { break };
            match decide(&state.pool, machine, true).await? {
                Decision::Admit => match launch(state, &head).await? {
                    LaunchOutcome::Sent => {}
                    LaunchOutcome::Failed(_)
                    | LaunchOutcome::Uncertain(_)
                    | LaunchOutcome::NotLaunchable => {
                        forget_admission(&state.pool, machine).await;
                    }
                    LaunchOutcome::Retry(_) => {
                        forget_admission(&state.pool, machine).await;
                        break;
                    }
                },
                Decision::Hold(figures) => {
                    sqlx::query(
                        "UPDATE sessions SET metadata = jsonb_set(metadata, '{queued}', $2) \
                         WHERE status = 'queued' AND id IN \
                           (SELECT session_id FROM spawn_queue WHERE machine_uuid = $1)",
                    )
                    .bind(machine)
                    .bind(figures.to_json())
                    .execute(&state.pool)
                    .await?;
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn figures(used: u64, recent: u64, ceiling: u64) -> Figures {
        Figures {
            mem_used: used * GIB,
            mem_total: 61 * GIB,
            recent: recent * SESSION_ESTIMATE_BYTES,
            ceiling: ceiling * GIB,
        }
    }

    #[test]
    fn a_session_fits_while_its_estimate_stays_under_the_ceiling() {
        assert!(figures(40, 0, 45).admits());
        assert!(!figures(44, 0, 45).admits(), "44 GiB + 1.5 GiB is over 45");
    }

    #[test]
    fn recent_launches_count_before_the_heartbeat_sees_them() {
        assert!(figures(40, 2, 45).admits(), "40 + 3 + 1.5 fits in 45");
        assert!(!figures(40, 3, 45).admits(), "40 + 4.5 + 1.5 does not");
    }

    /// A machine with `used` GiB in use, and `ceiling` GiB as its ceiling.
    async fn machine(pool: &sqlx::PgPool, used: u64, ceiling: Option<u64>) -> Uuid {
        let (uid, mid) = (Uuid::new_v4(), Uuid::new_v4());
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("adm-{uid}"))
            .bind(format!("h-{uid}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO machines (id, user_id, name, key_hash, mem_ceiling_bytes) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(mid)
        .bind(uid)
        .bind(format!("m-{mid}"))
        .bind(format!("k-{mid}"))
        .bind(ceiling.map(|c| i64::try_from(c * GIB).unwrap()))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO machine_resources (machine_id, cpu_pct, mem_pct, mem_used_bytes, \
               mem_total_bytes, disk_pct, disk_used_bytes, disk_total_bytes, disk_path, updated_at) \
             VALUES ($1, 1, 50, $2, $3, 1, 1, 1, '/', now())",
        )
        .bind(mid)
        .bind(i64::try_from(used * GIB).unwrap())
        .bind(i64::try_from(61 * GIB).unwrap())
        .execute(pool)
        .await
        .unwrap();
        mid
    }

    /// A queued spawn on `machine`: its placeholder session and its queue row.
    async fn queue_row(pool: &sqlx::PgPool, machine: Uuid, adapter: &str) -> String {
        let sid = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, machine_uuid, working_dir, status, adapter_id) \
             VALUES ($1, $2, $3, '/tmp', 'queued', $4)",
        )
        .bind(&sid)
        .bind(machine.to_string())
        .bind(machine)
        .bind(adapter)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO spawn_queue \
               (session_id, machine_uuid, caller_id, caller_key_id, request, command_id) \
             VALUES ($1, $2, $3, $4, '{}', $5)",
        )
        .bind(&sid)
        .bind(machine)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
        sid
    }

    #[allow(clippy::type_complexity)]
    async fn session_row(
        pool: &sqlx::PgPool,
        sid: &str,
    ) -> Option<(String, Option<String>, serde_json::Value)> {
        sqlx::query_as("SELECT status, end_reason, metadata FROM sessions WHERE id = $1")
            .bind(sid)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn command_of(pool: &sqlx::PgPool, sid: &str) -> Uuid {
        sqlx::query_scalar("SELECT command_id FROM spawn_queue WHERE session_id = $1")
            .bind(sid)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn queue_state(pool: &sqlx::PgPool, sid: &str) -> Option<String> {
        sqlx::query_scalar("SELECT state FROM spawn_queue WHERE session_id = $1")
            .bind(sid)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    /// Age the in-flight attempt past the grace, as if its server had died.
    async fn age_attempt(pool: &sqlx::PgPool, sid: &str) {
        sqlx::query(
            "UPDATE spawn_queue SET attempt_since = now() - interval '1 hour' \
             WHERE session_id = $1",
        )
        .bind(sid)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn in_queue(pool: &sqlx::PgPool, sid: &str) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM spawn_queue WHERE session_id = $1)")
            .bind(sid)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn test_pool(name: &str) -> Option<sqlx::PgPool> {
        let url = crate::routes::gateway::test_db_url(name)?;
        Some(sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(&url).await.unwrap())
    }

    async fn admissions(pool: &sqlx::PgPool, mid: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM machine_admissions WHERE machine_uuid = $1")
            .bind(mid)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn decide_against_the_database() {
        let Some(url) = crate::routes::gateway::test_db_url("admission_decide") else { return };
        let pool =
            sqlx::postgres::PgPoolOptions::new().max_connections(2).connect(&url).await.unwrap();

        // No ceiling: admitted, and nothing is recorded.
        let free = machine(&pool, 60, None).await;
        assert_eq!(decide(&pool, free, false).await.unwrap(), Decision::Admit);
        assert_eq!(admissions(&pool, free).await, 0);

        // 40 GiB used under a 45 GiB ceiling: three launches fit (40 + 3 x 1.5
        // = 44.5 with the last one), the fourth waits.
        let m = machine(&pool, 40, Some(45)).await;
        for _ in 0..3 {
            assert_eq!(decide(&pool, m, false).await.unwrap(), Decision::Admit);
        }
        assert_eq!(admissions(&pool, m).await, 3);
        let Decision::Hold(f) = decide(&pool, m, false).await.unwrap() else {
            panic!("the fourth launch must wait");
        };
        assert_eq!(f.recent, 3 * SESSION_ESTIMATE_BYTES);
        assert_eq!(f.ceiling, 45 * GIB);
        assert_eq!(admissions(&pool, m).await, 3, "a held launch is not recorded");

        // A queued spawn makes newcomers wait behind it, not the drain.
        let q = machine(&pool, 10, Some(45)).await;
        let sid = queue_row(&pool, q, "claude-code").await;
        assert!(matches!(decide(&pool, q, false).await.unwrap(), Decision::Hold(_)));
        assert_eq!(decide(&pool, q, true).await.unwrap(), Decision::Admit);

        // Deleting the placeholder takes its payload with it.
        sqlx::query("DELETE FROM sessions WHERE id = $1").bind(&sid).execute(&pool).await.unwrap();
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM spawn_queue WHERE session_id = $1")
                .bind(&sid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0);

        // A silent daemon: no fresh snapshot, so the ceiling cannot be judged.
        let stale = machine(&pool, 60, Some(45)).await;
        sqlx::query(
            "UPDATE machine_resources SET updated_at = now() - interval '1 hour' \
             WHERE machine_id = $1",
        )
        .bind(stale)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(decide(&pool, stale, false).await.unwrap(), Decision::Admit);
    }

    /// Review P1, crash BEFORE the send: the request is kept whole, nothing
    /// is sent behind the human's back, and it is never called failed.
    #[tokio::test]
    async fn a_launch_whose_server_died_before_sending_is_kept_in_doubt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let Some(pool) = test_pool("admission_crash_before_send").await else { return };
        let m = machine(&pool, 10, Some(45)).await;
        let sid = queue_row(&pool, m, "claude-code").await;
        let sent = AtomicUsize::new(0);
        let sent = &sent;
        let count = || {
            async move |_row: QueuedRow| -> Result<(), DispatchError> {
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };

        // The attempt marks the row in flight, durably, and the server dies
        // right there: another connection already sees `sending`.
        claim(&pool, &sid).await.unwrap().expect("waiting row");
        assert_eq!(queue_state(&pool, &sid).await.as_deref(), Some("sending"));
        assert_eq!(next_head(&pool, m).await.unwrap(), None, "no second attempt starts");
        assert_eq!(launch_with(&pool, &sid, count()).await.unwrap(), LaunchOutcome::NotLaunchable);

        // Too early to settle: the attempt may still be running.
        assert!(reconcile(&pool).await.unwrap().is_empty());
        age_attempt(&pool, &sid).await;
        // A restart changes nothing: the state is in the database, not in the
        // process. Reconciling keeps the request, in doubt, not failed.
        let after_restart = test_pool("admission_crash_before_send").await.unwrap();
        assert!(reconcile(&after_restart).await.unwrap().contains(&sid));
        assert_eq!(queue_state(&pool, &sid).await.as_deref(), Some("uncertain"));
        let (status, reason, meta) = session_row(&pool, &sid).await.expect("session kept");
        assert_eq!(status, "queued", "still waiting, not ended");
        assert!(reason.is_none(), "never a failure that did not happen");
        assert!(meta["launch_uncertain"]["help"].is_string(), "the UI is told what to do");
        assert!(in_queue(&pool, &sid).await, "the request is kept whole");

        // No drain, and no "launch now", sends it again on its own.
        assert_eq!(next_head(&pool, m).await.unwrap(), None);
        assert_eq!(launch_with(&pool, &sid, count()).await.unwrap(), LaunchOutcome::NotLaunchable);
        assert_eq!(sent.load(Ordering::SeqCst), 0, "nothing was ever sent");

        // Only the human, having checked the machine, sends it again.
        assert!(retry_uncertain(&pool, &sid).await.unwrap());
        assert_eq!(launch_now_with(&pool, &sid, count()).await.unwrap(), LaunchOutcome::Sent);
        assert_eq!(sent.load(Ordering::SeqCst), 1);
    }

    /// The contre-revue's case: the command reached the daemon's connection,
    /// which is an in-memory channel and no acknowledgement. A crash right
    /// there must not lose the request, and must not claim it started.
    #[tokio::test]
    async fn a_command_handed_over_is_kept_until_its_receipt() {
        let Some(pool) = test_pool("admission_awaits_receipt").await else { return };
        let m = machine(&pool, 10, Some(45)).await;
        let sid = queue_row(&pool, m, "codex").await;
        let command = command_of(&pool, &sid).await;
        let handed_over = async |_row: QueuedRow| Ok(());
        assert_eq!(launch_with(&pool, &sid, handed_over).await.unwrap(), LaunchOutcome::Sent);

        // The request is still there, with its payload, awaiting the receipt.
        assert_eq!(queue_state(&pool, &sid).await.as_deref(), Some("sent"));
        assert!(in_queue(&pool, &sid).await, "nothing is dropped on a bare channel send");
        assert_eq!(session_row(&pool, &sid).await.unwrap().0, "queued");
        // And no drain picks it up again.
        assert_eq!(next_head(&pool, m).await.unwrap(), None);

        // The server dies before any receipt: kept, in doubt, never resent.
        age_attempt(&pool, &sid).await;
        let after_restart = test_pool("admission_awaits_receipt").await.unwrap();
        assert!(reconcile(&after_restart).await.unwrap().contains(&sid));
        assert_eq!(queue_state(&pool, &sid).await.as_deref(), Some("uncertain"));
        assert!(in_queue(&pool, &sid).await);
        let (status, reason, meta) = session_row(&pool, &sid).await.unwrap();
        assert_eq!(status, "queued");
        assert!(reason.is_none(), "unacknowledged is not failed");
        assert!(meta["launch_uncertain"]["why"].is_string());
        assert_eq!(queued_of_command(&pool, command).await.unwrap().as_deref(), Some(sid.as_str()));
    }

    /// The receipt is the proof: it settles the row, one way or the other,
    /// and a receipt for a spawn that never queued is ignored.
    #[tokio::test]
    async fn a_receipt_settles_the_request_it_belongs_to() {
        let Some(pool) = test_pool("admission_receipt").await else { return };
        let m = machine(&pool, 10, Some(45)).await;

        let ok = queue_row(&pool, m, "codex").await;
        let handed_over = async |_row: QueuedRow| Ok(());
        launch_with(&pool, &ok, handed_over).await.unwrap();
        finish_launched(&pool, &ok).await.unwrap();
        assert!(!in_queue(&pool, &ok).await, "acknowledged: the request is done");
        assert_eq!(session_row(&pool, &ok).await, None, "the placeholder made way");

        let refused = queue_row(&pool, m, "codex").await;
        let handed_over = async |_row: QueuedRow| Ok(());
        launch_with(&pool, &refused, handed_over).await.unwrap();
        end_failed(&pool, &refused, "claude: no such working directory").await.unwrap();
        assert!(!in_queue(&pool, &refused).await);
        let (status, reason, _) = session_row(&pool, &refused).await.unwrap();
        assert_eq!((status.as_str(), reason.as_deref()), ("ended", Some("spawn_failed")));

        assert_eq!(queued_of_command(&pool, Uuid::new_v4()).await.unwrap(), None);
    }

    /// Review P1, crash AFTER the send: a session that registered is
    /// reconciled as launched (no false failure), whatever the adapter, and
    /// the daemon may have restarted meanwhile.
    #[tokio::test]
    async fn a_session_that_registered_reconciles_its_placeholder() {
        let Some(pool) = test_pool("admission_reconcile_live").await else { return };
        let m = machine(&pool, 10, Some(45)).await;
        let sid = queue_row(&pool, m, "claude-code").await;
        claim(&pool, &sid).await.unwrap().expect("waiting row");
        // The command went out, then the server died: the worker registered on
        // its own (the daemon upsert flips the placeholder to active), which
        // also covers the daemon having restarted and re-registered it.
        sqlx::query("UPDATE sessions SET status = 'active' WHERE id = $1")
            .bind(&sid)
            .execute(&pool)
            .await
            .unwrap();
        age_attempt(&pool, &sid).await;
        assert!(reconcile(&pool).await.unwrap().contains(&sid));
        assert!(!in_queue(&pool, &sid).await, "settled: it did start");
        let (status, reason, meta) = session_row(&pool, &sid).await.unwrap();
        assert_eq!(status, "active", "the live session is kept as it is");
        assert!(reason.is_none());
        assert!(meta.get("queued").is_none(), "the waiting figures are gone");
    }

    /// A codex spawn cannot be recognised by its id (codex mints its own), so
    /// an interrupted send stays in doubt: kept, visible, never resent alone,
    /// and never reported as a failure that did not happen.
    #[tokio::test]
    async fn an_interrupted_codex_launch_stays_in_doubt_rather_than_failed() {
        let Some(pool) = test_pool("admission_codex_doubt").await else { return };
        let m = machine(&pool, 10, Some(45)).await;
        let sid = queue_row(&pool, m, "codex").await;
        let broken = async |_row: QueuedRow| Err(DispatchError::Unknown("peer timeout".into()));
        let LaunchOutcome::Uncertain(why) = launch_with(&pool, &sid, broken).await.unwrap() else {
            panic!("a broken send is not a failure and not a success");
        };
        assert_eq!(why, "peer timeout");
        assert_eq!(queue_state(&pool, &sid).await.as_deref(), Some("uncertain"));
        let (status, reason, _) = session_row(&pool, &sid).await.unwrap();
        assert_eq!(status, "queued");
        assert!(reason.is_none(), "not a failure: nobody knows yet");
        // It is the codex thread that will tell: until a human settles it,
        // neither the drain nor a reconcile touches it.
        assert_eq!(next_head(&pool, m).await.unwrap(), None);
        assert!(reconcile(&pool).await.unwrap().is_empty());
        assert!(in_queue(&pool, &sid).await);
    }

    /// Only a send known not to have happened is retried on its own.
    #[tokio::test]
    async fn nothing_sent_is_retried_and_a_rejection_ends_it() {
        let Some(pool) = test_pool("admission_retry").await else { return };
        let m = machine(&pool, 10, Some(45)).await;
        let sid = queue_row(&pool, m, "claude-code").await;

        let offline = async |_row: QueuedRow| Err(DispatchError::NotSent("offline".into()));
        assert_eq!(
            launch_with(&pool, &sid, offline).await.unwrap(),
            LaunchOutcome::Retry("offline".into())
        );
        assert_eq!(next_head(&pool, m).await.unwrap(), Some(sid.clone()), "back in the queue");

        // Nothing was sent and nothing will be: that one is a real failure.
        let gone = async |_row: QueuedRow| Err(DispatchError::Rejected("account gone".into()));
        assert_eq!(
            launch_with(&pool, &sid, gone).await.unwrap(),
            LaunchOutcome::Failed("account gone".into())
        );
        assert!(!in_queue(&pool, &sid).await);
        let (status, reason, _) = session_row(&pool, &sid).await.unwrap();
        assert_eq!((status.as_str(), reason.as_deref()), ("ended", Some("spawn_failed")));
    }

    /// Review P2: "launch now" says what really happened, and reserves RAM
    /// only for a launch that went out.
    #[tokio::test]
    async fn launch_now_reports_the_real_outcome_and_reserves_nothing_otherwise() {
        let Some(pool) = test_pool("admission_launch_now").await else { return };
        let m = machine(&pool, 10, Some(45)).await;

        let failed = queue_row(&pool, m, "claude-code").await;
        let gone = async |_row: QueuedRow| Err(DispatchError::Rejected("account gone".into()));
        assert_eq!(
            launch_now_with(&pool, &failed, gone).await.unwrap(),
            LaunchOutcome::Failed("account gone".into())
        );
        assert_eq!(admissions(&pool, m).await, 0, "nothing launched, nothing reserved");

        let doubtful = queue_row(&pool, m, "codex").await;
        let broken = async |_row: QueuedRow| Err(DispatchError::Unknown("cut".into()));
        assert_eq!(
            launch_now_with(&pool, &doubtful, broken).await.unwrap(),
            LaunchOutcome::Uncertain("cut".into())
        );
        assert_eq!(admissions(&pool, m).await, 0, "a doubt reserves nothing either");

        let ok = queue_row(&pool, m, "claude-code").await;
        let sent = async |_row: QueuedRow| Ok(());
        assert_eq!(launch_now_with(&pool, &ok, sent).await.unwrap(), LaunchOutcome::Sent);
        assert_eq!(admissions(&pool, m).await, 1);
        assert_eq!(session_row(&pool, &ok).await, None, "placeholder gone once launched");
    }

    /// Review P1: the queue keeps who asked, not their rights. At launch the
    /// key is re-validated and its scopes re-derived from the current ACLs.
    #[tokio::test]
    async fn revalidation_follows_revocation_disabling_and_demotion() {
        let Some(pool) = test_pool("admission_revalidate").await else { return };
        let auth = crate::auth::AuthConfig::new(Vec::new(), pool.clone());
        let uid = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("rv-{uid}"))
            .bind(format!("h-{uid}"))
            .execute(&pool)
            .await
            .unwrap();
        for scope in ["read", "dispatch", "admin"] {
            sqlx::query("INSERT INTO user_acls (user_id, scope) VALUES ($1, $2)")
                .bind(uid)
                .bind(scope)
                .execute(&pool)
                .await
                .unwrap();
        }
        let key = async || -> Uuid {
            let kid: Uuid = sqlx::query_scalar(
                "INSERT INTO auth_keys (user_id, key_hash, kind) VALUES ($1, $2, 'user') \
                 RETURNING id",
            )
            .bind(uid)
            .bind(format!("k-{}", Uuid::new_v4()))
            .fetch_one(&pool)
            .await
            .unwrap();
            for scope in ["read", "dispatch", "admin"] {
                sqlx::query("INSERT INTO key_acls (key_id, scope) VALUES ($1, $2)")
                    .bind(kid)
                    .bind(scope)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            kid
        };
        let k1 = key().await;
        assert!(auth.revalidate(uid, k1).await.unwrap().expect("live key").is_admin());

        // Demoted: the admin scope held at queue time no longer counts.
        sqlx::query("DELETE FROM user_acls WHERE user_id = $1 AND scope = 'admin'")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        let ctx = auth.revalidate(uid, k1).await.unwrap().expect("still live");
        assert!(!ctx.is_admin(), "demotion applies to queued work");

        // Key revoked: nothing launches on its behalf.
        sqlx::query("UPDATE auth_keys SET revoked_at = now() WHERE id = $1")
            .bind(k1)
            .execute(&pool)
            .await
            .unwrap();
        assert!(auth.revalidate(uid, k1).await.unwrap().is_none());

        // User disabled: none of their keys count.
        let k2 = key().await;
        assert!(auth.revalidate(uid, k2).await.unwrap().is_some());
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        assert!(auth.revalidate(uid, k2).await.unwrap().is_none());

        // A key belongs to one user: another user's id does not borrow it.
        assert!(auth.revalidate(Uuid::new_v4(), k2).await.unwrap().is_none());

        // The env admin fallback holds only while an env token is configured.
        assert!(auth.revalidate(Uuid::nil(), Uuid::nil()).await.unwrap().is_none());
        let with_env = crate::auth::AuthConfig::new(vec!["t".into()], pool.clone());
        assert!(with_env.revalidate(Uuid::nil(), Uuid::nil()).await.unwrap().unwrap().is_admin());
    }

    #[test]
    fn migration_pair_exists() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");
        let up = std::fs::read_to_string(format!("{dir}/117_spawn_admission.up.sql")).unwrap();
        let down = std::fs::read_to_string(format!("{dir}/117_spawn_admission.down.sql")).unwrap();
        assert!(up.contains("mem_ceiling_bytes"));
        assert!(up.contains("CREATE TABLE IF NOT EXISTS spawn_queue"));
        assert!(up.contains("uncertain") && up.contains("caller_key_id"));
        assert!(up.contains("'sent'"), "the request outlives a bare channel send");
        assert!(down.contains("DROP TABLE IF EXISTS spawn_queue"));
    }
}
