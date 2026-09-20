//! `POST /api/v1/version/self-update` — deploy the newer release.
//!
//! The webui's update modal ends here, and there are two ways out of it. The
//! server knows *nothing* about how this deployment is updated (Kubernetes
//! rollout, Compose pull, a systemd unit…), and there are as many answers as
//! there are installations, so it never guesses. It asks whoever does know:
//!
//! 1. **The machine, if it was told.** A daemon with `CCTUI_UPDATE_COMMAND`
//!    set advertises a deterministic update hook; the server hands it the
//!    target version and the machine runs the operator's own command, verifies
//!    the served version and rolls back on failure. No model, no account, the
//!    same bytes every time. See `routes::update_hook` and
//!    `docs/update-hook.md`.
//!
//! 2. **A YOLO agent, otherwise.** The fallback for a deployment nobody has
//!    written a hook for: spawn a session on the configured self-update
//!    machine (see `routes::instance`) with a generic prompt, and let the
//!    agent read that machine's local instructions. The session runs under the
//!    caller's own accounts (`auto_account`), so the admin who clicks pays for
//!    it. It is the fallback, not the design: prefer a hook.
//!
//! Model floor: see [`launch_profile`]. The point of a self-update agent is
//! to read a deployment's runbook and act on infrastructure — never hand that
//! to a small model. Raise the floor when a new generation ships (AGENTS.md
//! keeps the rule).

use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use cctui_proto::adapter::PermissionMode;
use cctui_proto::api::{ApiError, SpawnRequest};
use serde::Serialize;
use tokio::sync::Mutex;
use ts_rs::TS;

use crate::auth::{AuthContext, Scope};
use crate::routes::{instance, spawn, update_hook};
use crate::state::AppState;
use crate::update_check;

const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// A second click inside this window is refused: the first agent is still at
/// work (a whole update is a few minutes), a duplicate would race it.
pub const RELAUNCH_COOLDOWN: Duration = Duration::from_mins(20);

/// The last self-update launched by this process: version + when.
#[derive(Default)]
pub struct SelfUpdateGuard {
    last: Mutex<Option<(String, Instant)>>,
}

impl SelfUpdateGuard {
    /// Claim the launch of `version`; `Err(remaining)` when one is still fresh.
    pub async fn claim(&self, version: &str) -> Result<(), Duration> {
        self.claim_at(version, Instant::now()).await
    }

    async fn claim_at(&self, version: &str, now: Instant) -> Result<(), Duration> {
        let mut slot = self.last.lock().await;
        let pending = slot.as_ref().and_then(|(v, at)| {
            let age = now.saturating_duration_since(*at);
            (v == version).then(|| RELAUNCH_COOLDOWN.checked_sub(age)).flatten()
        });
        if let Some(remaining) = pending.filter(|r| !r.is_zero()) {
            return Err(remaining);
        }
        *slot = Some((version.to_owned(), now));
        drop(slot);
        Ok(())
    }

    /// Forget a claim whose spawn was refused, so a retry is not locked out.
    pub async fn release(&self) {
        *self.last.lock().await = None;
    }
}

/// Model + effort the update agent is launched with, per adapter.
///
/// - claude-code: `opus` at `medium` effort — always above Sonnet.
/// - codex: the adapter's default frontier model at `medium` effort; never a
///   `mini` / `nano` variant.
///
/// Update this table whenever a newer generation replaces these names.
#[must_use]
pub fn launch_profile(adapter_id: &str) -> (Option<&'static str>, &'static str) {
    if adapter_id.starts_with("codex") { (None, "medium") } else { (Some("opus"), "medium") }
}

/// The generic instruction handed to the agent. Deployment-agnostic on
/// purpose: the machine's own instructions (CLAUDE.md, AGENTS.md, memory)
/// say how cctui is deployed here.
#[must_use]
pub fn prompt(current: &str, latest: &update_check::LatestRelease) -> String {
    format!(
        "A newer cctui release is available: v{latest} (this server runs v{current}).\n\
         Release page: {url}\n\n\
         Deploy cctui v{latest} on this system, the way this deployment is normally \
         updated. Read the local instructions of this machine (CLAUDE.md, AGENTS.md, \
         notes, memory) to find the procedure: pull or rebuild the server and webui \
         images or binaries for v{latest}, restart what needs restarting, and let \
         enrolled daemons update themselves if that is how this deployment works.\n\n\
         When done, verify that GET /api/v1/version on this deployment reports \
         version {latest}, then summarise what you did and anything that needs a human.\n\
         Do not upgrade anything other than cctui.",
        latest = latest.version,
        url = latest.url,
    )
}

/// Which of the two paths took the job, and what to watch as a result.
#[derive(Serialize, TS)]
#[ts(export)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SelfUpdateResponse {
    /// The machine's own update command is running. There is no session to
    /// open: progress is the run, polled from `GET /version/self-update`.
    Hook {
        run_id: uuid::Uuid,
        /// Version the machine was asked to deploy.
        version: String,
    },
    /// No hook on the target machine, so an agent got the job.
    Agent {
        /// Spawn command id, to await on the websocket like a manual spawn.
        command_id: uuid::Uuid,
        /// The id the new session registers under (claude-code pre-mints it),
        /// so the webui can jump to it; `null` for adapters that mint their own.
        session_id: Option<uuid::Uuid>,
        /// Version the agent was asked to deploy.
        version: String,
        /// Account the spawn bound (see `SpawnResponse::account`).
        account: Option<String>,
    },
}

fn conflict(msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (StatusCode::CONFLICT, Json(ApiError { error: msg.into() }))
}

pub async fn launch(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<(StatusCode, Json<SelfUpdateResponse>), (StatusCode, Json<ApiError>)> {
    ctx.requires(Scope::Admin)
        .map_err(|s| (s, Json(ApiError { error: "admin token required".into() })))?;
    if !update_check::enabled_from_env() {
        return Err(conflict("update check is disabled on this server (CCTUI_UPDATE_CHECK=0)"));
    }
    let Some(latest) = state.update_check.newer().await else {
        return Err(conflict("cctui is already up to date"));
    };
    let Some(target) = instance::read_self_update_target(&state.pool).await.target else {
        return Err(conflict(
            "no self-update machine configured: set one in Settings > Instance (or CCTUI_SELF_UPDATE_MACHINE + CCTUI_SELF_UPDATE_DIR)",
        ));
    };
    if let Err(remaining) = state.self_update.claim(&latest.version).await {
        return Err(conflict(format!(
            "an update to v{} was launched {} minutes ago; wait for it to finish",
            latest.version,
            RELAUNCH_COOLDOWN.saturating_sub(remaining).as_secs().div_ceil(60)
        )));
    }

    // The deterministic path, when the machine was told how to update this
    // deployment. Preferred over the agent whenever it exists.
    let machine_uuid = uuid::Uuid::parse_str(&target.machine_id).ok();
    if let Some(machine) = machine_uuid
        && update_hook::machine_has_hook(&state.pool, machine).await
    {
        return match launch_hook(&state, &ctx, machine, &latest).await {
            Ok(res) => Ok((StatusCode::ACCEPTED, Json(res))),
            Err(e) => {
                state.self_update.release().await;
                Err(e)
            }
        };
    }

    let adapter_id = target.adapter_id.clone().unwrap_or_else(|| "claude-code".to_owned());
    let (model, effort) = launch_profile(&adapter_id);
    let req = SpawnRequest {
        machine_id: target.machine_id.clone(),
        working_dir: target.working_dir.clone(),
        prompt: Some(prompt(CURRENT, &latest)),
        prompt_name: None,
        name: Some(format!("cctui update v{}", latest.version)),
        adapter_id: Some(adapter_id),
        permission_mode: Some(PermissionMode::Yolo),
        effort: Some(effort.to_owned()),
        model: model.map(str::to_owned),
        service_tier: None,
        env: std::collections::BTreeMap::default(),
        account: None,
        provider: None,
        no_account: false,
        auto_account: true,
        // No pool: the self-update agent is deliberately allowed the widest
        // account set, since it must run whatever the state of any one of them.
        pool: None,
        save_draft: false,
        auto_archive: false,
        env_keys: Vec::new(),
        attachment_names: Vec::new(),
        spawn_capability: None,
    };
    match spawn::dispatch_spawn(&state, &ctx, req, Vec::new(), Vec::new()).await {
        Ok((status, Json(res))) => {
            tracing::info!(
                version = %latest.version,
                machine = %target.machine_id,
                account = ?res.account,
                "self-update session launched"
            );
            Ok((
                status,
                Json(SelfUpdateResponse::Agent {
                    command_id: res.command_id,
                    session_id: res.session_id,
                    version: latest.version,
                    account: res.account,
                }),
            ))
        }
        Err(e) => {
            state.self_update.release().await;
            Err(e)
        }
    }
}

/// Open a run row, then hand the frame to the daemon. The row goes first on
/// purpose: the hook restarts this process, so by the time the daemon reports
/// anything, the only thing that still remembers the run is Postgres.
async fn launch_hook(
    state: &AppState,
    ctx: &AuthContext,
    machine: uuid::Uuid,
    latest: &update_check::LatestRelease,
) -> Result<SelfUpdateResponse, (StatusCode, Json<ApiError>)> {
    let run_id =
        update_hook::open_run(&state.pool, machine, &latest.version, CURRENT, Some(ctx.user_id))
            .await
            .map_err(|err| {
                tracing::error!(%err, "could not open a self-update run");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError { error: "could not record the update run".into() }),
                )
            })?;

    let frame = cctui_proto::ws::DaemonFrameDown::RunUpdateHook {
        run_id,
        version: latest.version.clone(),
        release_url: latest.url.clone(),
    };
    if let Err(err) = state.bus.command_daemon(machine, frame).await {
        update_hook::abandon_run(&state.pool, run_id).await;
        return Err(conflict(format!("could not reach the update machine: {err}")));
    }

    tracing::info!(%run_id, %machine, version = %latest.version, "update hook dispatched");
    Ok(SelfUpdateResponse::Hook { run_id, version: latest.version.clone() })
}

/// The most recent hook run, for the webui's progress panel. `null` when this
/// deployment has never run one (every agent-path deployment, forever).
///
/// Readable by any authenticated user: an outage everyone can see is not a
/// secret, and knowing an update is mid-flight is exactly what stops a second
/// person from starting another one.
pub async fn status(State(state): State<AppState>) -> Json<Option<update_hook::SelfUpdateRun>> {
    Json(update_hook::latest_run(&state.pool).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guard_blocks_a_second_launch_of_the_same_version() {
        let g = SelfUpdateGuard::default();
        let t0 = Instant::now();
        assert!(g.claim_at("1.0.0", t0).await.is_ok());
        let remaining = g.claim_at("1.0.0", t0 + Duration::from_mins(5)).await.unwrap_err();
        assert_eq!(remaining, RELAUNCH_COOLDOWN.saturating_sub(Duration::from_mins(5)));
        // A newer release supersedes the pending one.
        assert!(g.claim_at("1.0.1", t0 + Duration::from_mins(5)).await.is_ok());
        // And the window expires.
        assert!(g.claim_at("1.0.1", t0 + RELAUNCH_COOLDOWN + Duration::from_mins(6)).await.is_ok());
        // A refused spawn releases the claim right away.
        g.release().await;
        assert!(g.claim_at("1.0.1", t0 + Duration::from_mins(7)).await.is_ok());
    }

    #[test]
    fn launch_profile_never_picks_a_small_model() {
        assert_eq!(launch_profile("claude-code"), (Some("opus"), "medium"));
        assert_eq!(launch_profile("codex"), (None, "medium"));
        for (model, _) in [launch_profile("claude-code"), launch_profile("codex")] {
            let m = model.unwrap_or_default();
            assert!(
                !m.contains("haiku")
                    && !m.contains("sonnet")
                    && !m.contains("mini")
                    && !m.contains("nano")
            );
        }
    }

    #[test]
    fn prompt_names_the_version_and_stays_generic() {
        let p = prompt(
            "0.7.305",
            &update_check::LatestRelease { version: "0.7.309".into(), url: "https://x/y".into() },
        );
        assert!(p.contains("v0.7.309") && p.contains("v0.7.305") && p.contains("https://x/y"));
        assert!(!p.contains("kubectl") && !p.contains("docker"));
    }
}
