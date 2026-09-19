//! Per-machine host resource snapshot: persisted from the daemon heartbeat
//! (`machine_resources`, one row per machine) and served to the webui's
//! header gauge via `GET /api/v1/machines/resources`.

use axum::extract::State;
use axum::{Extension, Json};
use cctui_proto::models::MachineLiveness;
use cctui_proto::resources::MachineResources;
use chrono::{DateTime, Utc};
use serde::Serialize;
use ts_rs::TS;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::error::AppError;
use crate::state::AppState;

/// Upsert the daemon's latest snapshot. Fire-and-forget: a failed write only
/// loses one heartbeat's figures, the next one overwrites anyway.
pub async fn persist(state: &AppState, machine_id: Uuid, r: &MachineResources) {
    let res = sqlx::query(
        "INSERT INTO machine_resources \
           (machine_id, cpu_pct, mem_pct, mem_used_bytes, mem_total_bytes, \
            disk_pct, disk_used_bytes, disk_total_bytes, disk_path, load1, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now()) \
         ON CONFLICT (machine_id) DO UPDATE SET \
           cpu_pct = EXCLUDED.cpu_pct, mem_pct = EXCLUDED.mem_pct, \
           mem_used_bytes = EXCLUDED.mem_used_bytes, mem_total_bytes = EXCLUDED.mem_total_bytes, \
           disk_pct = EXCLUDED.disk_pct, disk_used_bytes = EXCLUDED.disk_used_bytes, \
           disk_total_bytes = EXCLUDED.disk_total_bytes, disk_path = EXCLUDED.disk_path, \
           load1 = EXCLUDED.load1, updated_at = now()",
    )
    .bind(machine_id)
    .bind(r.cpu_pct)
    .bind(r.mem_pct)
    .bind(i64::try_from(r.mem_used_bytes).unwrap_or(i64::MAX))
    .bind(i64::try_from(r.mem_total_bytes).unwrap_or(i64::MAX))
    .bind(r.disk_pct)
    .bind(i64::try_from(r.disk_used_bytes).unwrap_or(i64::MAX))
    .bind(i64::try_from(r.disk_total_bytes).unwrap_or(i64::MAX))
    .bind(&r.disk_path)
    .bind(r.load1)
    .execute(&state.pool)
    .await;
    if let Err(err) = res {
        tracing::warn!(%err, %machine_id, "machine_resources upsert failed");
    }
}

/// Persist a heartbeat snapshot, then push it to every webui/TUI so a pinned
/// header gauge follows the machine live rather than on the next poll.
pub async fn record_and_broadcast(state: &AppState, machine_id: Uuid, resources: MachineResources) {
    persist(state, machine_id, &resources).await;
    state
        .bus
        .publish_server(cctui_proto::ws::ServerEvent::MachineResources { machine_id, resources });
}

/// One enrolled daemon machine and its last-known resource snapshot, for the
/// Settings › Resource monitoring list and the header gauge.
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct MachineResourcesRow {
    pub machine_id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    /// Operator-set badge hue (0-359). `None` = hash of the name.
    pub hue: Option<i16>,
    pub liveness: MachineLiveness,
    /// `None` until the machine's daemon has sent a heartbeat carrying a
    /// snapshot (older daemon, non-Linux host): the gauge shows "?" then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<MachineResources>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// RAM ceiling for spawns on this machine, in bytes. `None` = no ceiling
    /// (the default): spawns are never held back.
    #[ts(type = "number | null")]
    pub mem_ceiling_bytes: Option<u64>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    name: String,
    display_name: Option<String>,
    hue: Option<i16>,
    last_seen_at: DateTime<Utc>,
    cpu_pct: Option<f32>,
    mem_pct: Option<f32>,
    mem_used_bytes: Option<i64>,
    mem_total_bytes: Option<i64>,
    disk_pct: Option<f32>,
    disk_used_bytes: Option<i64>,
    disk_total_bytes: Option<i64>,
    disk_path: Option<String>,
    load1: Option<f32>,
    updated_at: Option<DateTime<Utc>>,
    mem_ceiling_bytes: Option<i64>,
}

/// `GET /api/v1/machines/resources`: the caller's persistent, non-revoked
/// machines (every user's for an admin) with their latest snapshot.
pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<MachineResourcesRow>>, AppError> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT m.id, m.name, m.display_name, m.hue, m.last_seen_at, \
                r.cpu_pct, r.mem_pct, r.mem_used_bytes, r.mem_total_bytes, \
                r.disk_pct, r.disk_used_bytes, r.disk_total_bytes, r.disk_path, r.load1, \
                r.updated_at, m.mem_ceiling_bytes \
         FROM machines m LEFT JOIN machine_resources r ON r.machine_id = m.id \
         WHERE (m.user_id = $1 OR $2) AND m.kind = 'persistent' \
           AND m.revoked_at IS NULL AND m.deleted_at IS NULL \
         ORDER BY m.first_seen_at",
    )
    .bind(ctx.user_id)
    .bind(ctx.is_admin())
    .fetch_all(&state.pool)
    .await?;
    let out = rows
        .into_iter()
        .map(|r| {
            let resources = r.cpu_pct.map(|cpu_pct| MachineResources {
                cpu_pct,
                mem_pct: r.mem_pct.unwrap_or(0.0),
                mem_used_bytes: r.mem_used_bytes.unwrap_or(0).max(0).unsigned_abs(),
                mem_total_bytes: r.mem_total_bytes.unwrap_or(0).max(0).unsigned_abs(),
                disk_pct: r.disk_pct.unwrap_or(0.0),
                disk_used_bytes: r.disk_used_bytes.unwrap_or(0).max(0).unsigned_abs(),
                disk_total_bytes: r.disk_total_bytes.unwrap_or(0).max(0).unsigned_abs(),
                disk_path: r.disk_path.unwrap_or_default(),
                load1: r.load1,
            });
            MachineResourcesRow {
                machine_id: r.id,
                name: r.name,
                display_name: r.display_name,
                hue: r.hue,
                liveness: crate::machine_liveness::derive(r.last_seen_at),
                resources,
                updated_at: r.updated_at,
                mem_ceiling_bytes: r.mem_ceiling_bytes.map(|b| b.max(0).unsigned_abs()),
            }
        })
        .collect();
    Ok(Json(out))
}

/// Body of `PUT /api/v1/machines/{id}/mem-ceiling`.
#[derive(Debug, serde::Deserialize, TS)]
#[ts(export)]
pub struct MemCeilingRequest {
    /// Bytes; `null` removes the ceiling.
    #[ts(type = "number | null")]
    pub mem_ceiling_bytes: Option<u64>,
}

/// Smallest ceiling accepted: below one session's estimate nothing would ever
/// launch, which is a typo, not a policy.
const MIN_CEILING_BYTES: u64 = crate::admission::SESSION_ESTIMATE_BYTES;

/// `PUT /api/v1/machines/{id}/mem-ceiling`: the machine's owner (or an admin)
/// sets the RAM ceiling above which spawns wait in the queue, or clears it.
pub async fn set_mem_ceiling(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    axum::extract::Path(machine_id): axum::extract::Path<Uuid>,
    Json(req): Json<MemCeilingRequest>,
) -> Result<axum::http::StatusCode, AppError> {
    if let Some(b) = req.mem_ceiling_bytes
        && b < MIN_CEILING_BYTES
    {
        return Err(AppError::new(
            axum::http::StatusCode::BAD_REQUEST,
            format!("mem_ceiling_bytes must be at least {MIN_CEILING_BYTES} (one session)"),
        ));
    }
    let ceiling = req.mem_ceiling_bytes.map(|b| i64::try_from(b).unwrap_or(i64::MAX));
    let res = sqlx::query(
        "UPDATE machines SET mem_ceiling_bytes = $1 \
         WHERE id = $2 AND (user_id = $3 OR $4) AND deleted_at IS NULL",
    )
    .bind(ceiling)
    .bind(machine_id)
    .bind(ctx.user_id)
    .bind(ctx.is_admin())
    .execute(&state.pool)
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::new(axum::http::StatusCode::NOT_FOUND, "machine not found"));
    }
    tracing::info!(%machine_id, ?ceiling, "machine RAM ceiling set");
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    #[test]
    fn migration_pair_exists() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");
        let up = std::fs::read_to_string(format!("{dir}/097_machine_resources.up.sql")).unwrap();
        let down =
            std::fs::read_to_string(format!("{dir}/097_machine_resources.down.sql")).unwrap();
        assert!(up.contains("CREATE TABLE machine_resources"));
        assert!(down.contains("DROP TABLE IF EXISTS machine_resources"));
    }
}
