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

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use cctui_proto::adapter::BootstrapFile;
use cctui_proto::api::{ApiError, SpawnRequest, SpawnResponse};
use cctui_proto::models::SessionStatus;
use cctui_proto::ws::ServerEvent;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{AuthContext, Scope};
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
    let scopes: Vec<&str> = ctx.scopes.iter().map(|s| s.as_str()).collect();

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
           (session_id, machine_uuid, caller_id, caller_scopes, request, env_enc, uploads) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id.to_string())
    .bind(machine)
    .bind(ctx.user_id)
    .bind(&scopes)
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
    caller_id: Uuid,
    caller_scopes: Vec<String>,
    request: serde_json::Value,
    env_enc: Option<String>,
    uploads: Option<serde_json::Value>,
}

/// Why a queued launch did not go out.
enum LaunchError {
    /// Worth another try on the next tick (daemon offline, database hiccup).
    Transient(String),
    /// Will never succeed as stored (unknown account, corrupt payload).
    Permanent(String),
}

/// Launch one queued spawn, whatever the machine's figures. The queue row is
/// locked for the whole attempt so no other replica launches it too; it is
/// only removed once the dispatch went out, or failed for good.
async fn launch(state: &AppState, session_id: &str) -> Result<bool, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let row: Option<QueuedRow> = sqlx::query_as(
        "SELECT session_id, caller_id, caller_scopes, request, env_enc, uploads \
         FROM spawn_queue WHERE session_id = $1 FOR UPDATE SKIP LOCKED",
    )
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else { return Ok(false) };

    match dispatch_row(state, &row).await {
        Err(LaunchError::Transient(why)) => {
            tracing::debug!(session = %session_id, %why, "queued spawn not launched yet");
            return Ok(false);
        }
        Err(LaunchError::Permanent(why)) => {
            tracing::warn!(session = %session_id, %why, "queued spawn dropped");
            sqlx::query("DELETE FROM spawn_queue WHERE session_id = $1")
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE sessions SET status = 'ended', ended_at = now(), \
                   end_reason = 'spawn_failed', end_detail = $2, \
                   metadata = metadata - 'queued' \
                 WHERE id = $1 AND status = 'queued'",
            )
            .bind(session_id)
            .bind(&why)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            state.bus.publish_server(ServerEvent::SessionDeregistered {
                session_id: session_id.to_owned(),
            });
            return Ok(true);
        }
        Ok(()) => {}
    }
    sqlx::query("DELETE FROM spawn_queue WHERE session_id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    // The placeholder goes: the live session registers under its own row (the
    // same id for claude-code). If it already registered, the status is no
    // longer `queued` and only the stale figures are dropped.
    sqlx::query("DELETE FROM sessions WHERE id = $1 AND status = 'queued'")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET metadata = metadata - 'queued' WHERE id = $1")
        .bind(session_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state
        .bus
        .publish_server(ServerEvent::SessionDeregistered { session_id: session_id.to_owned() });
    tracing::info!(session = %session_id, "queued spawn launched");
    Ok(true)
}

async fn dispatch_row(state: &AppState, row: &QueuedRow) -> Result<(), LaunchError> {
    let mut req: SpawnRequest = serde_json::from_value(row.request.clone())
        .map_err(|e| LaunchError::Permanent(format!("corrupt queued request: {e}")))?;
    if let Some(enc) = &row.env_enc {
        let plain = crate::crypto::decrypt(enc, &crate::crypto::vault_key())
            .ok_or_else(|| LaunchError::Permanent("queued env could not be decrypted".into()))?;
        req.env = serde_json::from_str::<BTreeMap<String, String>>(&plain)
            .map_err(|e| LaunchError::Permanent(format!("corrupt queued env: {e}")))?;
    }
    let uploads: Vec<BootstrapFile> = match &row.uploads {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| LaunchError::Permanent(format!("corrupt queued uploads: {e}")))?,
        None => Vec::new(),
    };
    let ctx = AuthContext {
        user_id: row.caller_id,
        key_id: Uuid::nil(),
        machine_id: None,
        scopes: row.caller_scopes.iter().filter_map(|s| Scope::parse(s)).collect(),
    };
    let preset = Uuid::parse_str(&row.session_id).ok();
    match crate::routes::spawn::dispatch_spawn_as(state, &ctx, req, uploads, preset).await {
        Ok(_) => Ok(()),
        Err((code, Json(err)))
            if code == StatusCode::SERVICE_UNAVAILABLE || code.is_server_error() =>
        {
            Err(LaunchError::Transient(err.error))
        }
        Err((_, Json(err))) => Err(LaunchError::Permanent(err.error)),
    }
}

/// `POST /sessions/{id}/launch` on a queued session: the human overrides the
/// ceiling and launches it now. `Ok(false)` when it is not queued (or another
/// replica is launching it right now).
pub async fn launch_now(state: &AppState, session_id: &str) -> Result<bool, sqlx::Error> {
    let machine: Option<Uuid> =
        sqlx::query_scalar("SELECT machine_uuid FROM spawn_queue WHERE session_id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await?;
    let Some(machine) = machine else { return Ok(false) };
    let launched = launch(state, session_id).await?;
    if launched {
        // Count it, so the launches that follow see the RAM it is about to take.
        sqlx::query("INSERT INTO machine_admissions (machine_uuid) VALUES ($1)")
            .bind(machine)
            .execute(&state.pool)
            .await?;
    }
    Ok(launched)
}

/// Reaper step: launch what each machine's ceiling now lets through, oldest
/// first, refresh the figures shown on what still waits, and forget old
/// admission records.
pub async fn drain(state: &AppState) {
    if let Err(e) = drain_inner(state).await {
        tracing::warn!(%e, "spawn queue drain failed");
    }
}

async fn drain_inner(state: &AppState) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM machine_admissions WHERE admitted_at < now() - interval '1 hour'")
        .execute(&state.pool)
        .await?;
    let machines: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT machine_uuid FROM spawn_queue")
        .fetch_all(&state.pool)
        .await?;
    for machine in machines {
        for _ in 0..DRAIN_PER_TICK {
            let head: Option<String> = sqlx::query_scalar(
                "SELECT session_id FROM spawn_queue WHERE machine_uuid = $1 \
                 ORDER BY queued_at LIMIT 1",
            )
            .bind(machine)
            .fetch_optional(&state.pool)
            .await?;
            let Some(head) = head else { break };
            match decide(&state.pool, machine, true).await? {
                Decision::Admit => {
                    if !launch(state, &head).await? {
                        forget_admission(&state.pool, machine).await;
                        break;
                    }
                }
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
        let sid = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, machine_uuid, working_dir, status) \
             VALUES ($1, $2, $3, '/tmp', 'queued')",
        )
        .bind(&sid)
        .bind(q.to_string())
        .bind(q)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO spawn_queue (session_id, machine_uuid, caller_id, caller_scopes, request) \
             VALUES ($1, $2, $3, '{read}', '{}')",
        )
        .bind(&sid)
        .bind(q)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
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

    #[test]
    fn migration_pair_exists() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");
        let up = std::fs::read_to_string(format!("{dir}/117_spawn_admission.up.sql")).unwrap();
        let down = std::fs::read_to_string(format!("{dir}/117_spawn_admission.down.sql")).unwrap();
        assert!(up.contains("mem_ceiling_bytes"));
        assert!(up.contains("CREATE TABLE IF NOT EXISTS spawn_queue"));
        assert!(down.contains("DROP TABLE IF EXISTS spawn_queue"));
    }
}
