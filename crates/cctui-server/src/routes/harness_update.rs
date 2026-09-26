//! Harness auto-update policy: instance default in
//! `instance_settings.harness_autoupdate`, per-machine override in
//! `machines.harness_autoupdate`, and the per-machine report the heartbeat
//! carries. The policy reaches a daemon in reply to its heartbeat, only when
//! the one it holds differs, so a daemon that cannot parse the frame (it sends
//! no report) never gets one and an absent setting sends nothing.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use cctui_proto::api::ApiError;
use cctui_proto::harness::{HarnessReport, HarnessUpdatePolicy};
use cctui_proto::ws::DaemonFrameDown;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use crate::auth::{AuthContext, Scope};
use crate::state::AppState;

const KEY: &str = "harness_autoupdate";

#[derive(Serialize, TS)]
#[ts(export)]
pub struct MachineHarnessInfo {
    pub machine_id: String,
    pub name: String,
    /// `null` inherits the instance default.
    pub policy: Option<HarnessUpdatePolicy>,
    pub effective: HarnessUpdatePolicy,
    pub report: Option<HarnessReport>,
    #[ts(type = "string | null")]
    pub report_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct HarnessAutoupdateInfo {
    pub instance: Option<HarnessUpdatePolicy>,
    pub machines: Vec<MachineHarnessInfo>,
}

#[derive(Deserialize, TS)]
#[ts(export)]
pub struct HarnessPolicyRequest {
    /// `null` clears: the instance default falls back to off, a machine
    /// override falls back to the instance default.
    pub policy: Option<HarnessUpdatePolicy>,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<ApiError>)>;

fn db_err(e: &sqlx::Error) -> (StatusCode, Json<ApiError>) {
    tracing::error!("harness auto-update settings failed: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
}

fn admin(ctx: &AuthContext) -> Result<(), (StatusCode, Json<ApiError>)> {
    ctx.requires(Scope::Admin)
        .map_err(|s| (s, Json(ApiError { error: "admin token required".into() })))
}

fn parse(v: Option<serde_json::Value>) -> Option<HarnessUpdatePolicy> {
    v.and_then(|v| serde_json::from_value::<HarnessUpdatePolicy>(v).ok())
        .map(HarnessUpdatePolicy::normalized)
}

async fn instance_policy(pool: &sqlx::PgPool) -> Option<HarnessUpdatePolicy> {
    parse(
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT value FROM instance_settings WHERE key = $1",
        )
        .bind(KEY)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten(),
    )
}

async fn machine_policy(pool: &sqlx::PgPool, machine_id: Uuid) -> Option<HarnessUpdatePolicy> {
    parse(
        sqlx::query_scalar::<_, Option<serde_json::Value>>(
            "SELECT harness_autoupdate FROM machines WHERE id = $1",
        )
        .bind(machine_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten(),
    )
}

pub async fn effective_policy(pool: &sqlx::PgPool, machine_id: Uuid) -> HarnessUpdatePolicy {
    let instance = instance_policy(pool).await;
    let machine = machine_policy(pool, machine_id).await;
    HarnessUpdatePolicy::resolve(instance.as_ref(), machine.as_ref())
}

/// Persist the heartbeat's report and resend the policy if the daemon holds
/// a different one.
pub async fn on_heartbeat(state: &AppState, machine_id: Uuid, report: &HarnessReport) {
    let stored = HarnessReport { policy: None, ..report.clone() };
    if let Err(err) = sqlx::query(
        "UPDATE machines SET harness_report = $2, harness_report_at = now() \
         WHERE id = $1 AND harness_report IS DISTINCT FROM $2",
    )
    .bind(machine_id)
    .bind(serde_json::to_value(&stored).expect("serializable"))
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%err, %machine_id, "harness report write failed");
    }
    let policy = effective_policy(&state.pool, machine_id).await;
    if !report.needs_policy(&policy) {
        return;
    }
    if let Err(err) =
        state.bus.command_daemon(machine_id, DaemonFrameDown::HarnessUpdatePolicy { policy }).await
    {
        tracing::warn!(%err, %machine_id, "could not send HarnessUpdatePolicy");
    }
}

async fn read_info(pool: &sqlx::PgPool) -> Result<HarnessAutoupdateInfo, sqlx::Error> {
    type Row = (
        Uuid,
        String,
        Option<serde_json::Value>,
        Option<serde_json::Value>,
        Option<chrono::DateTime<chrono::Utc>>,
    );
    let instance = instance_policy(pool).await;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, COALESCE(display_name, name), harness_autoupdate, harness_report, \
                harness_report_at \
         FROM machines WHERE revoked_at IS NULL AND deleted_at IS NULL AND kind = 'persistent' \
         ORDER BY COALESCE(display_name, name)",
    )
    .fetch_all(pool)
    .await?;
    let machines = rows
        .into_iter()
        .map(|(id, name, policy, report, report_at)| {
            let policy = parse(policy);
            MachineHarnessInfo {
                machine_id: id.to_string(),
                name,
                effective: HarnessUpdatePolicy::resolve(instance.as_ref(), policy.as_ref()),
                policy,
                report: report.and_then(|v| serde_json::from_value(v).ok()),
                report_at,
            }
        })
        .collect();
    Ok(HarnessAutoupdateInfo { instance, machines })
}

pub async fn read(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<HarnessAutoupdateInfo> {
    admin(&ctx)?;
    Ok(Json(read_info(&state.pool).await.map_err(|e| db_err(&e))?))
}

pub async fn set_instance(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<HarnessPolicyRequest>,
) -> ApiResult<HarnessAutoupdateInfo> {
    admin(&ctx)?;
    match req.policy.map(HarnessUpdatePolicy::normalized) {
        Some(policy) => {
            sqlx::query(
                "INSERT INTO instance_settings (key, value, updated_at) VALUES ($1, $2, now()) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
            )
            .bind(KEY)
            .bind(serde_json::to_value(&policy).expect("serializable"))
            .execute(&state.pool)
            .await
            .map_err(|e| db_err(&e))?;
        }
        None => {
            sqlx::query("DELETE FROM instance_settings WHERE key = $1")
                .bind(KEY)
                .execute(&state.pool)
                .await
                .map_err(|e| db_err(&e))?;
        }
    }
    Ok(Json(read_info(&state.pool).await.map_err(|e| db_err(&e))?))
}

pub async fn set_machine(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(machine_id): Path<Uuid>,
    Json(req): Json<HarnessPolicyRequest>,
) -> ApiResult<HarnessAutoupdateInfo> {
    admin(&ctx)?;
    let value = req.policy.map(|p| serde_json::to_value(p.normalized()).expect("serializable"));
    let res = sqlx::query(
        "UPDATE machines SET harness_autoupdate = $2 WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(machine_id)
    .bind(value)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if res.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, Json(ApiError { error: "machine not found".into() })));
    }
    Ok(Json(read_info(&state.pool).await.map_err(|e| db_err(&e))?))
}
