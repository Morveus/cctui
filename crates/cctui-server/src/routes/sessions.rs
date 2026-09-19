use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use cctui_proto::adapter::SessionChild;
use cctui_proto::api::{
    ApiError, Label, MessageRequest, RegisterRequest, RegisterResponse, RenameRequest,
    SessionListItem, SessionListResponse, SpawnRequest, SpawnResponse,
};
use cctui_proto::classifier::{Bucket, ClassifyInput, PrStatus, PrStatusCache, classify};
use cctui_proto::models::{Attention, Liveness, Session, SessionEndReason, SessionStatus};

use crate::auth::AuthContext;
use crate::routes::spawn::{bad_request, resolve_owned_machine};
use crate::state::AppState;

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, (StatusCode, Json<ApiError>)> {
    // Use Claude's session_id directly — it's our primary key now
    let session_id = req.claude_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = Utc::now();
    let session = Session {
        id: session_id.clone(),
        parent_id: req.parent_session_id,
        account_id: None,
        machine_id: req.machine_id,
        working_dir: req.working_dir,
        status: SessionStatus::New,
        registered_at: now,
        last_heartbeat: now,
        metadata: req.metadata.unwrap_or_else(|| serde_json::json!({})),
        adapter_id: None,
    };

    // Best-effort resolve `req.machine_id` (freeform: UUID, friendly name,
    // or OS hostname) into `machines.id` so the admin UI can join sessions
    // to machine names without string heuristics. Historical sessions
    // pre-dating this code remain orphaned, but new registrations land
    // with the right link.
    let machine_uuid: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT id FROM machines WHERE id::text = $1 OR name = $1 LIMIT 1")
            .bind(&session.machine_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("db error (machine_uuid lookup): {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError { error: "database error".into() }),
                )
            })?;

    // A fresh registration is `new` — it becomes `active` on the first
    // transcript line / turn. Re-registration of an already-known session
    // (e.g. Claude restart) is treated the same way: status=new, let the
    // first activity promote it.
    sqlx::query(
        r"INSERT INTO sessions (id, parent_id, account_id, machine_id, machine_uuid, working_dir, status, registered_at, last_heartbeat, metadata, model)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULLIF($10->>'model', ''))
           ON CONFLICT (id) DO UPDATE SET status = 'new', last_heartbeat = $9, metadata = $10, machine_uuid = COALESCE(sessions.machine_uuid, EXCLUDED.machine_uuid), model = COALESCE(sessions.model, EXCLUDED.model)",
    )
    .bind(&session.id)
    .bind(&session.parent_id)
    .bind(&session.account_id)
    .bind(&session.machine_id)
    .bind(machine_uuid)
    .bind(&session.working_dir)
    .bind("new")
    .bind(session.registered_at)
    .bind(session.last_heartbeat)
    .bind(&session.metadata)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;

    let ws_url = format!(
        "{}/api/v1/stream/{}",
        state.config.external_url.replacen("http://", "ws://", 1).replacen("https://", "wss://", 1),
        session_id
    );
    {
        let mut registry = state.registry.write().await;
        registry.register(session.clone());
    }
    // Create (or reuse, on re-registration) the session's live stream channel
    // in the bus — delivery moved out of the registry handle.
    state.bus.register_session_stream(&session_id);

    tracing::info!(session_id = %session_id, machine = %session.machine_id, "session registered");
    Ok(Json(RegisterResponse { session_id, ws_url }))
}

pub async fn deregister(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Deregister just drops the live handle; the session stays in DB as
    // `inactive` and can be revived by a future turn/message.
    crate::store::sessions::set_inactive(&state.pool, &session_id, false).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    {
        let mut registry = state.registry.write().await;
        registry.deregister(&session_id);
    }
    state.bus.deregister_session_stream(&session_id);
    tracing::info!(session_id = %session_id, "session deregistered");
    Ok(StatusCode::NO_CONTENT)
}

// Per-session ownership is enforced by the `Resource(Session, …)` guard in
// `authz.rs`: the single-object session routes declare that policy and the
// `authz_layer` middleware resolves owner via `machine_uuid -> machines.user_id`
// before the handler runs (404 unknown / 403 cross-user / admin bypass). The
// batch routes below still filter inline (`filter_owned_ids`) because a yes/no
// guard can't express "act only on the ids you own".

/// Resolve the owning user for a batch of session ids in one query, then keep
/// only the ids the caller may act on (admins keep every requested id). Used by
/// the batch archive/pin routes so a caller can't sweep other users' sessions.
async fn filter_owned_ids(
    state: &AppState,
    ctx: &AuthContext,
    ids: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if ctx.is_admin() {
        return Ok(ids.to_vec());
    }
    let owned: Vec<String> = sqlx::query_scalar(
        "SELECT s.id \
         FROM sessions s LEFT JOIN machines m ON m.id = s.machine_uuid \
         WHERE s.id = ANY($1) AND m.user_id = $2",
    )
    .bind(ids)
    .bind(ctx.user_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(owned)
}

/// Derived-status thresholds. A session is considered:
/// - `Active` if its last heartbeat is within this window;
/// - `New` if it isn't active but was registered within this window;
/// - `Inactive` otherwise.
const STATUS_WINDOW_SECS: i64 = 5 * 60;

/// Liveness-dot thresholds (heartbeat age). `Active` (green) within the
/// active window, `Stale` (orange) up to the dead window, `Dead` (no dot)
/// beyond it.
pub const LIVENESS_ACTIVE_SECS: i64 = 5 * 60;
pub const LIVENESS_DEAD_SECS: i64 = 60 * 60;

/// Query params for `GET /sessions`. Archived sessions are hidden unless
/// `?include_archived=true`.
#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(sqlx::FromRow)]
pub struct DbSession {
    id: String,
    parent_id: Option<String>,
    machine_id: String,
    working_dir: String,
    status: String,
    registered_at: DateTime<Utc>,
    last_heartbeat: DateTime<Utc>,
    metadata: serde_json::Value,
    adapter_id: Option<String>,
    resolved_machine_name: Option<String>,
    resolved_machine_hue: Option<i16>,
    resolved_machine_kind: Option<String>,
}

pub fn derive_status(registered_at: DateTime<Utc>, last_heartbeat: DateTime<Utc>) -> SessionStatus {
    let now = Utc::now();
    if (now - last_heartbeat).num_seconds() < STATUS_WINDOW_SECS {
        SessionStatus::Active
    } else if (now - registered_at).num_seconds() < STATUS_WINDOW_SECS {
        SessionStatus::New
    } else {
        SessionStatus::Inactive
    }
}

/// Map heartbeat age onto the three-tier liveness dot.
pub fn derive_liveness(last_heartbeat: DateTime<Utc>) -> Liveness {
    let age = (Utc::now() - last_heartbeat).num_seconds();
    if age < LIVENESS_ACTIVE_SECS {
        Liveness::Active
    } else if age < LIVENESS_DEAD_SECS {
        Liveness::Stale
    } else {
        Liveness::Dead
    }
}

/// Sticky terminal statuses: persisted states that must NOT be
/// re-derived from heartbeat age. `ended` (`SessionEnded` received) and `failed`
/// (dispatch never launched) both mean "this session is over" — without this
/// they showed Active/green for ~5 min until the heartbeat aged out, masking
/// the end of unattended/dispatched jobs.
fn sticky_status(row_status: &str) -> Option<SessionStatus> {
    match row_status {
        "archived" => Some(SessionStatus::Archived),
        "ended" | "failed" => Some(SessionStatus::Inactive),
        // Draft: staged-not-running, never re-derived from heartbeat.
        "draft" => Some(SessionStatus::Draft),
        // Queued: waiting for RAM, launched by the reaper, not heartbeat-driven.
        "queued" => Some(SessionStatus::Queued),
        _ => None,
    }
}

/// Resolve the row's status + liveness, honouring sticky terminal states.
pub fn resolve_status_liveness(
    row_status: &str,
    registered_at: DateTime<Utc>,
    last_heartbeat: DateTime<Utc>,
) -> (SessionStatus, Liveness) {
    sticky_status(row_status).map_or_else(
        || (derive_status(registered_at, last_heartbeat), derive_liveness(last_heartbeat)),
        |s| {
            // Archived keeps its real liveness dot; ended/failed are terminal → Dead.
            // Queued is waiting, not over: `Stale` keeps clients that read
            // `Dead` as "finished" from giving up on it before it launches.
            let liveness = match s {
                SessionStatus::Archived => derive_liveness(last_heartbeat),
                SessionStatus::Queued => Liveness::Stale,
                _ => Liveness::Dead,
            };
            (s, liveness)
        },
    )
}

/// Classify a session into its bucket from the persisted signals. `children`
/// are the session's linked-PR references (`sessions.children`); the `Review`
/// bucket arises when one of them has an OPEN PR awaiting attention per the
/// `pr_cache` the GitHub connector maintains (empty cache degrades to no Review).
pub fn bucket_from_signals(
    tempo: Option<&str>,
    agent_state: Option<&str>,
    activity: Option<&str>,
    soft_limit_blocked: Option<&str>,
    children: &[SessionChild],
    pr_cache: &std::collections::HashMap<String, PrStatus<'_>>,
) -> Bucket {
    let input = ClassifyInput {
        tempo,
        state: agent_state,
        activity,
        children,
        q: None,
        soft_limit_blocked,
    };
    classify(&input, pr_cache)
}

/// The attention glyph is just the `Blocked` bucket surfaced as ✋ needs input.
pub const fn attention_from_bucket(bucket: Bucket) -> Option<Attention> {
    match bucket {
        Bucket::Blocked => Some(Attention::NeedsInput),
        _ => None,
    }
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub async fn list_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<ListParams>,
    req_headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, (StatusCode, Json<ApiError>)> {
    let uid = ctx.owner_filter();
    let db_err = |e: sqlx::Error| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    };

    // Live sessions from in-memory registry — keep registered_at for sorting.
    // Non-admins only see registry entries they own. The registry's
    // `machine_id` is freeform (UUID or hostname), so ownership can't be read
    // off the handle directly — resolve it from the DB: which of the live ids
    // are owned by this caller. A live session with no resolvable owner is
    // EXCLUDED for non-admins rather than leaked.
    let owned_live_ids: Option<HashSet<String>> = if ctx.is_admin() {
        None
    } else {
        let live_ids: Vec<String> = {
            let registry = state.registry.read().await;
            registry.list().into_iter().map(|h| h.session.id.clone()).collect()
        };
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT s.id FROM sessions s \
             LEFT JOIN machines m ON m.id = s.machine_uuid \
             WHERE s.id = ANY($1) AND m.user_id = $2",
        )
        .bind(&live_ids)
        .bind(ctx.user_id)
        .fetch_all(&state.pool)
        .await
        .map_err(db_err)?;
        Some(rows.into_iter().map(|(id,)| id).collect())
    };

    let mut with_ts: Vec<(DateTime<Utc>, SessionListItem)> = {
        let registry = state.registry.read().await;
        registry
            .list()
            .into_iter()
            .filter(|handle| {
                owned_live_ids.as_ref().is_none_or(|owned| owned.contains(&handle.session.id))
            })
            .map(|handle| {
                (
                    handle.session.registered_at,
                    SessionListItem {
                        id: handle.session.id.clone(),
                        parent_id: handle.session.parent_id.clone(),
                        machine_id: handle.session.machine_id.clone(),
                        working_dir: handle.session.working_dir.clone(),
                        status: derive_status(
                            handle.session.registered_at,
                            handle.session.last_heartbeat,
                        ),
                        liveness: derive_liveness(handle.session.last_heartbeat),
                        attention: None,
                        bucket: Bucket::Working,
                        token_usage: handle.token_usage.clone(),
                        metadata: handle.session.metadata.clone(),
                        adapter_id: handle.session.adapter_id.clone(),
                        machine_name: None,
                        machine_hue: None,
                        machine_kind: None,
                        last_message_text: None,
                        last_message_at: None,
                        registered_at: Some(handle.session.registered_at),
                        name: None,
                        model: None,
                        effort: None,
                        auto_approve: false,
                        match_snippet: None,
                        match_seq: None,
                        last_activity_at: None,
                        cache_cold: false,
                        estimated_burst_tokens: None,
                        hibernated: false,
                        pinned: false,
                        labels: Vec::new(),
                        last_heartbeat: Some(handle.session.last_heartbeat),
                        account_name: None,
                        unread_count: 0,
                        activity_detail: None,
                        last_tool_at: None,
                        last_tool_name: None,
                        tool_use_count: 0,
                        todos: Vec::new(),
                        has_token_credentials: false,
                        account_traffic_observed: false,
                        pr_links: Vec::new(),
                        end_reason: None,
                        end_detail: None,
                        ended_at: None,
                    },
                )
            })
            .collect()
    };

    // Historical inactive sessions from DB (not currently in the live registry).
    // Archived sessions are hidden unless explicitly requested.
    let live_ids: HashSet<String> = with_ts.iter().map(|(_, s)| s.id.clone()).collect();
    let cols = "s.id, s.parent_id, s.machine_id, s.working_dir, s.status, \
                s.registered_at, s.last_heartbeat, s.metadata, s.adapter_id, \
                COALESCE(m.display_name, m.name) AS resolved_machine_name, \
                m.hue AS resolved_machine_hue, m.kind AS resolved_machine_kind";
    // ALL non-archived sessions are always returned (no cap) so live/working
    // sessions are never silently truncated. The LIMIT 25 cap applies only to
    // the archived tail, and only when archived history is requested (the
    // webui paginates the archive list separately). The `$1::uuid IS NULL OR
    // m.user_id = $1` predicate scopes rows to the caller (NULL for admin).
    let non_archived_query = format!(
        "SELECT {cols} \
         FROM sessions s \
         LEFT JOIN machines m ON m.id = s.machine_uuid \
         WHERE s.status != 'archived' \
         AND ($1::uuid IS NULL OR m.user_id = $1) \
         ORDER BY s.registered_at DESC",
    );
    let mut rows: Vec<DbSession> = sqlx::query_as(sqlx::AssertSqlSafe(non_archived_query))
        .bind(uid)
        .fetch_all(&state.pool)
        .await
        .map_err(db_err)?;
    if params.include_archived {
        let archived_query = format!(
            "SELECT {cols} \
             FROM sessions s \
             LEFT JOIN machines m ON m.id = s.machine_uuid \
             WHERE s.status = 'archived' \
             AND ($1::uuid IS NULL OR m.user_id = $1) \
             ORDER BY s.registered_at DESC LIMIT 25",
        );
        let archived: Vec<DbSession> = sqlx::query_as(sqlx::AssertSqlSafe(archived_query))
            .bind(uid)
            .fetch_all(&state.pool)
            .await
            .map_err(db_err)?;
        rows.extend(archived);
    }

    for row in rows {
        if live_ids.contains(&row.id) {
            continue;
        }
        // Sticky terminal states (archived/ended/failed) are NOT re-derived
        // from heartbeat; everything else is time-based.
        let (status, liveness) =
            resolve_status_liveness(&row.status, row.registered_at, row.last_heartbeat);
        with_ts.push((
            row.registered_at,
            SessionListItem {
                id: row.id,
                parent_id: row.parent_id,
                machine_id: row.machine_id,
                working_dir: row.working_dir,
                status,
                liveness,
                attention: None,
                bucket: Bucket::Working,
                token_usage: cctui_proto::models::TokenUsage::default(),
                metadata: row.metadata,
                adapter_id: row.adapter_id.map(cctui_proto::adapter::AdapterId::new),
                machine_name: row.resolved_machine_name,
                machine_hue: row.resolved_machine_hue,
                machine_kind: row.resolved_machine_kind,
                last_message_text: None,
                last_message_at: None,
                registered_at: Some(row.registered_at),
                name: None,
                model: None,
                effort: None,
                auto_approve: false,
                match_snippet: None,
                match_seq: None,
                last_activity_at: None,
                cache_cold: false,
                estimated_burst_tokens: None,
                hibernated: false,
                pinned: false,
                labels: Vec::new(),
                last_heartbeat: Some(row.last_heartbeat),
                account_name: None,
                unread_count: 0,
                activity_detail: None,
                last_tool_at: None,
                last_tool_name: None,
                tool_use_count: 0,
                todos: Vec::new(),
                has_token_credentials: false,
                account_traffic_observed: false,
                pr_links: Vec::new(),
                end_reason: None,
                end_detail: None,
                ended_at: None,
            },
        ));
    }

    let sessions = enrich_and_sort(&state, Some(ctx.user_id), with_ts).await?;
    Ok(crate::http_cache::json_with_etag(&req_headers, &SessionListResponse { sessions }))
}

/// Cap a raw unread `COUNT(*)` to the badge's display ceiling (99). Negative or
/// overflowing DB values clamp into `0..=99`.
fn cap_unread(n: i64) -> u32 {
    n.clamp(0, 99) as u32
}

/// The model catalog each session's usage is priced against, from the provider
/// row its newest session token binds to. Ungatewayed sessions are absent.
async fn session_catalogs(
    state: &AppState,
    session_ids: &[String],
) -> std::collections::HashMap<String, serde_json::Value> {
    sqlx::query_as::<_, (String, Option<serde_json::Value>)>(
        "SELECT DISTINCT ON (st.session_id) st.session_id, ap.models \
         FROM session_tokens st JOIN account_providers ap ON ap.id = st.account_id \
         WHERE st.session_id = ANY($1) \
         ORDER BY st.session_id, st.created_at DESC",
    )
    .bind(session_ids)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("db error (session catalogs): {e}");
        Vec::new()
    })
    .into_iter()
    .filter_map(|(sid, models)| Some((sid, models?)))
    .collect()
}

/// Shared enrichment for both the sessions list and search: resolve machine
/// names, aggregate token usage, attach last-message text, apply classifier
/// signals + display metadata + the in-memory auto-approve flag, then sort
/// most-recent-first. Returns the finished items ready to serialize.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
async fn enrich_and_sort(
    state: &AppState,
    viewer: Option<uuid::Uuid>,
    mut with_ts: Vec<(DateTime<Utc>, SessionListItem)>,
) -> Result<Vec<SessionListItem>, (StatusCode, Json<ApiError>)> {
    // Resolve machine names in one query. Historical sessions for purged
    // machines simply get `None`.
    let machine_ids: Vec<String> = with_ts
        .iter()
        .map(|(_, s)| s.machine_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !machine_ids.is_empty() {
        // `sessions.machine_id` is freeform: daemon-spawned sessions carry the
        // machine UUID, but legacy `/register` callers carry the OS hostname.
        // Match on either `id` or `name`, and prefer `display_name` (operator
        // override) over `name` for the resolved label.
        #[allow(clippy::type_complexity)]
        let rows: Vec<(String, String, Option<String>, Option<i16>, String)> = sqlx::query_as(
            "SELECT id::text, name, display_name, hue, kind FROM machines \
             WHERE id::text = ANY($1) OR name = ANY($1)",
        )
        .bind(&machine_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (machines lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_key: std::collections::HashMap<String, (String, Option<i16>, String)> =
            std::collections::HashMap::with_capacity(rows.len() * 2);
        for (id, name, display_name, hue, kind) in rows {
            let resolved = display_name.unwrap_or_else(|| name.clone());
            by_key.insert(id, (resolved.clone(), hue, kind.clone()));
            by_key.insert(name, (resolved, hue, kind));
        }
        for (_, s) in &mut with_ts {
            if s.machine_name.is_none()
                && let Some((name, hue, kind)) = by_key.get(&s.machine_id)
            {
                s.machine_name = Some(name.clone());
                s.machine_hue = *hue;
                s.machine_kind = Some(kind.clone());
            }
        }
    }

    // Aggregated token usage per session. Replaces the always-0
    // historical values; live sessions also pick up DB-aggregated counts
    // since the daemon persists every assistant turn.
    let session_ids: Vec<String> = with_ts.iter().map(|(_, s)| s.id.clone()).collect();
    if !session_ids.is_empty() {
        type TokenRow =
            (String, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>);
        let rows: Vec<TokenRow> = sqlx::query_as(
            "SELECT session_id, model, \
                        SUM(input_tokens)::bigint, SUM(output_tokens)::bigint, \
                        SUM(cache_read_tokens)::bigint, SUM(cache_creation_tokens)::bigint \
                 FROM session_token_usage \
                 WHERE session_id = ANY($1) \
                 GROUP BY session_id, model",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (token usage aggregate): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let catalogs = session_catalogs(state, &session_ids).await;
        let mut by_session: std::collections::HashMap<String, cctui_proto::models::TokenUsage> =
            std::collections::HashMap::new();
        for (sid, model, ti, to, cr, cc) in rows {
            let (input, cached_input, output, cache_creation) =
                (ti.unwrap_or(0), cr.unwrap_or(0), to.unwrap_or(0), cc.unwrap_or(0));
            let cost = crate::cost::tallies_cost_usd(
                catalogs.get(&sid),
                &[(model, crate::cost::TokenUsage { input, cached_input, output })],
            );
            let to_u64 = |v: i64| u64::try_from(v).unwrap_or(0);
            let e = by_session.entry(sid).or_default();
            e.tokens_in += to_u64(input);
            e.tokens_out += to_u64(output);
            e.cache_read_tokens += to_u64(cached_input);
            e.cache_creation_tokens += to_u64(cache_creation);
            e.cost_usd += cost;
        }
        for (_, s) in &mut with_ts {
            if let Some(usage) = by_session.remove(&s.id) {
                s.token_usage = usage;
            }
        }
    }

    // Last message text + timestamp per session, from stream_events.
    if !session_ids.is_empty() {
        let rows: Vec<(String, serde_json::Value, DateTime<Utc>)> = sqlx::query_as(
            "SELECT DISTINCT ON (session_id) session_id, payload, created_at \
             FROM stream_events \
             WHERE session_id = ANY($1) AND event_type = 'message' \
             ORDER BY session_id, created_at DESC",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (last message lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, (Option<String>, DateTime<Utc>)> =
            std::collections::HashMap::new();
        for (sid, payload, ts) in rows {
            let text = payload
                .get("text")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("content").and_then(|v| v.as_str()))
                .map(normalize_last_message);
            by_session.insert(sid, (text, ts));
        }
        for (_, s) in &mut with_ts {
            if let Some((text, ts)) = by_session.remove(&s.id) {
                s.last_message_text = text;
                s.last_message_at = Some(ts);
            }
        }
    }

    // Unread assistant `message` count per session for the calling user:
    // messages newer than the viewer's `session_reads.last_seen_at`,
    // all of them when the user has never seen the session. One batched query
    // over the same `session_ids` fan-out; capped at 99 to keep the badge tidy.
    if let Some(uid) = viewer
        && !session_ids.is_empty()
    {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT se.session_id, COUNT(*) \
             FROM stream_events se \
             LEFT JOIN session_reads sr \
               ON sr.session_id = se.session_id AND sr.user_id = $2 \
             WHERE se.session_id = ANY($1) \
               AND se.event_type = 'message' \
               AND (sr.last_seen_at IS NULL OR se.created_at > sr.last_seen_at) \
             GROUP BY se.session_id",
        )
        .bind(&session_ids)
        .bind(uid)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (unread count lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, u32> =
            rows.into_iter().map(|(sid, n)| (sid, cap_unread(n))).collect();
        for (_, s) in &mut with_ts {
            s.unread_count = by_session.remove(&s.id).unwrap_or(0);
        }
    }

    // Agent task lists. A separate round-trip rather than another `SignalRow`
    // column: that tuple is already at sqlx's 16-element `FromRow` ceiling.
    if !session_ids.is_empty() {
        let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT id, todos FROM sessions WHERE id = ANY($1) AND todos IS NOT NULL",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (todos lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, serde_json::Value> =
            rows.into_iter().collect();
        for (_, s) in &mut with_ts {
            if let Some(v) = by_session.remove(&s.id) {
                s.todos = serde_json::from_value(v).unwrap_or_default();
            }
        }
    }

    // Status signals + display metadata from the session row, applied
    // uniformly to live + historical items: the ✋ attention glyph (from the
    // classifier) and the name/model/effort columns.
    if !session_ids.is_empty() {
        type SignalRow = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<String>,
            i32,
            serde_json::Value,
            Option<String>,
            Option<String>,
            Option<DateTime<Utc>>,
        );
        let rows: Vec<SignalRow> = sqlx::query_as(
            "SELECT id, tempo, agent_state, activity, session_name, model, effort, pinned, \
                    soft_limit_reason, last_tool_at, last_tool_name, tool_use_count, \
                    children, end_reason, end_detail, ended_at \
             FROM sessions WHERE id = ANY($1)",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (status signals lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, SignalRow> =
            std::collections::HashMap::new();
        for row in rows {
            by_session.insert(row.0.clone(), row);
        }
        let pr_snapshot = state.pr_status_cache.snapshot();
        let pr_cache = PrStatusCache::borrow_map(&pr_snapshot);
        for (_, s) in &mut with_ts {
            if let Some((
                _,
                tempo,
                agent_state,
                activity,
                name,
                model,
                effort,
                pinned,
                soft_limit_reason,
                last_tool_at,
                last_tool_name,
                tool_use_count,
                children,
                end_reason,
                end_detail,
                ended_at,
            )) = by_session.remove(&s.id)
            {
                s.end_reason = end_reason.as_deref().map(SessionEndReason::parse);
                s.end_detail = end_detail;
                s.ended_at = ended_at;
                let children: Vec<SessionChild> =
                    serde_json::from_value(children).unwrap_or_default();
                let bucket = bucket_from_signals(
                    tempo.as_deref(),
                    agent_state.as_deref(),
                    activity.as_deref(),
                    soft_limit_reason.as_deref(),
                    &children,
                    &pr_cache,
                );
                s.pr_links =
                    children.iter().filter(|c| c.kind == "pr").map(|c| c.href.clone()).collect();
                s.bucket = bucket;
                s.attention = attention_from_bucket(bucket);
                // Worker exited but resumable on reply. The adapter
                // parks the marker in `tempo`; the next live snapshot after a
                // resume overwrites it.
                s.hibernated = tempo.as_deref() == Some("hibernated");
                s.name = name;
                s.model = model;
                s.effort = effort;
                s.pinned = pinned;
                s.activity_detail = activity;
                s.last_tool_at = last_tool_at;
                s.last_tool_name = last_tool_name;
                s.tool_use_count = tool_use_count.clamp(0, i32::MAX) as u32;
            }
        }
    }

    // Session labels: one batched join over the junction table, then
    // bucket the rows per session so each item carries its colored labels.
    if !session_ids.is_empty() {
        type LabelRow = (String, uuid::Uuid, String, String);
        let rows: Vec<LabelRow> = sqlx::query_as(
            "SELECT sl.session_id, l.id, l.name, l.color \
             FROM session_labels sl JOIN labels l ON l.id = sl.label_id \
             WHERE sl.session_id = ANY($1) \
             ORDER BY lower(l.name)",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (labels lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, Vec<Label>> =
            std::collections::HashMap::new();
        for (sid, id, name, color) in rows {
            by_session.entry(sid).or_default().push(Label { id: id.to_string(), name, color });
        }
        for (_, s) in &mut with_ts {
            if let Some(labels) = by_session.remove(&s.id) {
                s.labels = labels;
            }
        }
    }

    // Account each session runs under. `sessions.account_id` is
    // unused legacy; the real binding lives in `session_tokens` (minted at
    // dispatch/gateway). Resolve the most recent non-revoked token per session
    // to its identity's `accounts.name` (via `account_providers`) in one batched
    // query. `None` for sessions that never routed through the gateway (e.g.
    // plain local sessions).
    if !session_ids.is_empty() {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT ON (st.session_id) st.session_id, a.name \
             FROM session_tokens st \
             JOIN account_providers ap ON ap.id = st.account_id \
             JOIN accounts a ON a.id = ap.account_id \
             WHERE st.session_id = ANY($1) AND st.revoked_at IS NULL \
             ORDER BY st.session_id, st.created_at DESC",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (account lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, String> = rows.into_iter().collect();
        for (_, s) in &mut with_ts {
            if let Some(name) = by_session.remove(&s.id) {
                s.account_name = Some(name);
            }
        }
    }

    // Live token↔account credential binding. Independent of the
    // account-name join above: a token whose `accounts` row was deleted (or that
    // never resolved a name) still counts as holding credentials. `true` iff a
    // non-revoked `session_tokens` row with a present `encrypted_token` exists.
    if !session_ids.is_empty() {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT session_id FROM session_tokens \
             WHERE session_id = ANY($1) AND revoked_at IS NULL AND encrypted_token IS NOT NULL",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (credential lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let with_creds: std::collections::HashSet<String> =
            rows.into_iter().map(|(id,)| id).collect();
        for (_, s) in &mut with_ts {
            s.has_token_credentials = with_creds.contains(&s.id);
        }
    }

    // Observed gateway traffic per session: a live token that has actually been
    // presented at the gateway (`last_used_at` stamped). An account-bound
    // session missing from this set is bound in the DB but never routed through
    // the gateway — the webui's "no gateway traffic observed" warning.
    if !session_ids.is_empty() {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT session_id FROM session_tokens \
             WHERE session_id = ANY($1) AND revoked_at IS NULL AND last_used_at IS NOT NULL",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (traffic lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let observed: std::collections::HashSet<String> =
            rows.into_iter().map(|(id,)| id).collect();
        for (_, s) in &mut with_ts {
            s.account_traffic_observed = observed.contains(&s.id);
        }
    }

    // Reflect the in-memory auto-approve flag per session so clients
    // can render the toggle in its current state.
    {
        let store = state.permission_store.read().await;
        for (_, s) in &mut with_ts {
            s.auto_approve = store.is_auto_approve(&s.id);
        }
    }

    // Cold-cache surfacing. The per-message cache split lives in
    // `session_token_usage`; the SUM() aggregate above flattens it, so here we
    // pull only the *most recent* row per session to derive:
    //   - `cache_cold`     — that turn re-billed the full context
    //                        (cache_creation > 0 && cache_read == 0).
    //   - `last_activity_at` — its timestamp, so the client can predict cache
    //                        expiry (Anthropic's ~5-min sliding window) before
    //                        the next send.
    //   - `estimated_burst_tokens` — the cached-context size from the last
    //                        turn (≈ cache_read + cache_creation), i.e. how
    //                        many tokens get re-written on the next send.
    if !session_ids.is_empty() {
        type LastRow = (String, i64, i64, DateTime<Utc>);
        let rows: Vec<LastRow> = sqlx::query_as(
            "SELECT DISTINCT ON (session_id) session_id, \
                    cache_read_tokens, cache_creation_tokens, created_at \
             FROM session_token_usage \
             WHERE session_id = ANY($1) \
             ORDER BY session_id, created_at DESC",
        )
        .bind(&session_ids)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error (last token usage lookup): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        let mut by_session: std::collections::HashMap<String, (i64, i64, DateTime<Utc>)> =
            std::collections::HashMap::new();
        for (sid, cr, cc, ts) in rows {
            by_session.insert(sid, (cr, cc, ts));
        }
        for (_, s) in &mut with_ts {
            if let Some((cr, cc, ts)) = by_session.remove(&s.id) {
                s.last_activity_at = Some(ts);
                s.cache_cold = cc > 0 && cr == 0;
                // Context size that would be re-written to cache on the next
                // send (≈ the full cached prefix from the last turn).
                let burst_tokens = u64::try_from(cr.saturating_add(cc)).unwrap_or(0);
                if burst_tokens > 0 {
                    s.estimated_burst_tokens = Some(burst_tokens);
                }
            }
        }
    }

    // Pinned sessions sort above everything. Within each group, sort
    // by most recent message so active sessions float to the top; fall back to
    // registration time when a session has no messages yet.
    with_ts.sort_by(|a, b| {
        let key = |s: &SessionListItem, reg: DateTime<Utc>| s.last_message_at.unwrap_or(reg);
        b.1.pinned.cmp(&a.1.pinned).then_with(|| key(&b.1, b.0).cmp(&key(&a.1, a.0)))
    });
    Ok(with_ts.into_iter().map(|(_, s)| s).collect())
}

/// Query params for `GET /sessions/search`. `q` is a substring
/// matched (case-insensitively, trgm-accelerated) against a session's full
/// transcript plus its id/name/working-dir. `include_archived` sets the scope:
/// `false` searches live (non-archived) sessions only, `true` searches all.
/// With an empty `q` and `include_archived=true` this becomes "browse the
/// archive" — archived sessions, newest first. Offset pagination throughout.
#[derive(Debug, Default, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub include_archived: bool,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Shared SELECT + machine-name join for the session-search queries.
const SEARCH_SELECT: &str = "SELECT s.id, s.parent_id, s.machine_id, s.working_dir, s.status, \
            s.registered_at, s.last_heartbeat, s.metadata, s.adapter_id, \
            COALESCE(m.display_name, m.name) AS resolved_machine_name, \
            m.hue AS resolved_machine_hue, m.kind AS resolved_machine_kind \
     FROM sessions s \
     LEFT JOIN machines m ON m.id = s.machine_uuid";

const SEARCH_DEFAULT_LIMIT: i64 = 100;
const SEARCH_MAX_LIMIT: i64 = 500;

/// Escape LIKE/ILIKE wildcards so a user's literal `%`/`_` aren't treated as
/// pattern metacharacters, then wrap in `%…%` for a substring match. `\` is
/// the default ILIKE escape char.
pub fn ilike_contains(q: &str) -> String {
    let escaped = q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{escaped}%")
}

/// A positional bind for the compiled query — kept typed so `pinned` binds a
/// real `bool` and every other leaf binds text (raw for `=`, `%…%` for ILIKE).
enum SqlParam {
    Text(String),
    Bool(bool),
}

fn ph_text(params: &mut Vec<SqlParam>, s: String) -> usize {
    params.push(SqlParam::Text(s));
    params.len()
}

/// SQL predicate for one fielded value. The bind is appended to `params` and
/// the returned string references it by its `$N` position (1-based).
fn leaf_predicate(field: &str, value: &str, params: &mut Vec<SqlParam>) -> String {
    match field {
        "machine" => {
            let p = ph_text(params, value.to_string());
            format!(
                "(lower(COALESCE(m.display_name, m.name)) = lower(${p}) \
                  OR lower(s.machine_id) = lower(${p}))"
            )
        }
        "account" => {
            let p = ph_text(params, value.to_string());
            format!(
                "EXISTS (SELECT 1 FROM session_tokens st \
                  JOIN account_providers ap ON ap.id = st.account_id \
                  JOIN accounts a ON a.id = ap.account_id \
                  WHERE st.session_id = s.id AND st.revoked_at IS NULL \
                  AND lower(a.name) = lower(${p}))"
            )
        }
        "tag" => {
            let p = ph_text(params, value.to_string());
            format!(
                "EXISTS (SELECT 1 FROM session_labels sl \
                  JOIN labels l ON l.id = sl.label_id \
                  WHERE sl.session_id = s.id AND lower(l.name) = lower(${p}))"
            )
        }
        "title" => {
            let p = ph_text(params, ilike_contains(value));
            format!("COALESCE(s.session_name, '') ILIKE ${p}")
        }
        "status" => {
            let p = ph_text(params, value.to_string());
            format!("lower(s.status) = lower(${p})")
        }
        "model" => {
            let p = ph_text(params, ilike_contains(value));
            format!("COALESCE(s.model, s.metadata->>'model', '') ILIKE ${p}")
        }
        "effort" => {
            let p = ph_text(params, value.to_string());
            format!("lower(COALESCE(s.effort, '')) = lower(${p})")
        }
        "adapter" => {
            let p = ph_text(params, value.to_string());
            format!("lower(COALESCE(s.adapter_id, '')) = lower(${p})")
        }
        "pinned" => {
            params.push(SqlParam::Bool(value.eq_ignore_ascii_case("true")));
            format!("s.pinned = ${}", params.len())
        }
        "dir" => {
            let p = ph_text(params, ilike_contains(value));
            format!("s.working_dir ILIKE ${p}")
        }
        _ => {
            let p = ph_text(params, ilike_contains(value));
            free_text_predicate(p)
        }
    }
}

/// Must match the `idx_stream_events_search_trgm_capped` expression (migration
/// 065) exactly, or the ILIKE silently stops using the index.
const SEARCH_TEXT_CAP: u32 = 8192;

/// The residual free-text path: id / name / dir / trgm-accelerated transcript.
///
/// `(SELECT s.id)` must not be simplified to `s.id`: the wrapper keeps the
/// correlation as a Param, and a plain `s.id` lets the planner hoist the
/// sublink into a hashed subplan that rechecks every matching event in the
/// table rather than probing one session (13 s vs 0.35 s on 600k events).
fn free_text_predicate(p: usize) -> String {
    format!(
        "(s.id ILIKE ${p} OR COALESCE(s.session_name, '') ILIKE ${p} \
          OR s.working_dir ILIKE ${p} \
          OR EXISTS (SELECT 1 FROM stream_events e \
                     WHERE e.session_id = (SELECT s.id) \
                     AND left(e.search_text, {SEARCH_TEXT_CAP}) ILIKE ${p}))"
    )
}

fn compile_filter(filter: &cctui_query::Filter, params: &mut Vec<SqlParam>) -> String {
    let preds: Vec<String> =
        filter.values.iter().map(|v| leaf_predicate(&filter.field, v, params)).collect();
    let joined = match preds.len() {
        0 => "TRUE".to_string(),
        1 => preds.into_iter().next().unwrap(),
        _ => format!("({})", preds.join(" OR ")),
    };
    if filter.op == cctui_query::FilterOp::Ne { format!("(NOT {joined})") } else { joined }
}

/// Walk the query AST into a parameterised SQL `WHERE` fragment,
/// pushing binds onto `params` in `$1…$N` order. Fielded leaves become column
/// predicates / join-EXISTS, free text takes the `pg_trgm` path, and the
/// boolean/negation structure maps straight to `AND`/`OR`/`NOT`.
fn compile_node(node: &cctui_query::Node, params: &mut Vec<SqlParam>) -> String {
    use cctui_query::Node;
    match node {
        Node::Empty => "TRUE".to_string(),
        Node::Text { value } => {
            let p = ph_text(params, ilike_contains(value));
            free_text_predicate(p)
        }
        Node::Filter { filter } => compile_filter(filter, params),
        Node::Not { child } => format!("(NOT {})", compile_node(child, params)),
        Node::And { children } => join_children(children, "AND", params),
        Node::Or { children } => join_children(children, "OR", params),
    }
}

fn join_children(children: &[cctui_query::Node], sep: &str, params: &mut Vec<SqlParam>) -> String {
    let preds: Vec<String> = children.iter().map(|c| compile_node(c, params)).collect();
    if preds.is_empty() {
        "TRUE".to_string()
    } else {
        format!("({})", preds.join(&format!(" {sep} ")))
    }
}

/// Newest event matching any term, at most one row per session.
/// `$1` is the session-id array, `$2…` the ILIKE patterns.
fn snippet_sql(n_patterns: usize) -> String {
    let or = (2..=n_patterns + 1)
        .map(|i| format!("left(e.search_text, {SEARCH_TEXT_CAP}) ILIKE ${i}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "SELECT sid, hit.text, hit.id FROM unnest($1::text[]) AS sid \
         CROSS JOIN LATERAL ( \
           SELECT left(e.search_text, {SEARCH_TEXT_CAP}) AS text, e.id \
           FROM stream_events e \
           WHERE e.session_id = sid AND ({or}) \
           ORDER BY e.created_at DESC LIMIT 1 \
         ) hit"
    )
}

/// Build a ~200-char snippet of `text` centered on the earliest case-insensitive
/// occurrence of any `needle`, so the UI can show why a session matched.
fn make_snippet(text: &str, needles: &[String]) -> String {
    const WINDOW: usize = 200;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let hay = collapsed.to_lowercase();
    // Earliest byte hit among all needles → char offset to center on.
    let match_byte = needles.iter().filter_map(|n| hay.find(&n.to_lowercase())).min().unwrap_or(0);
    let match_char = collapsed[..match_byte].chars().count();
    let start = match_char.saturating_sub(WINDOW / 2);

    let mut offsets = collapsed.char_indices().map(|(i, _)| i).skip(start);
    let start_byte = offsets.next().unwrap_or(collapsed.len());
    let end_byte = offsets.nth(WINDOW - 1).unwrap_or(collapsed.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&collapsed[start_byte..end_byte]);
    if end_byte < collapsed.len() {
        out.push('…');
    }
    out
}

/// `GET /sessions/search?q=…&include_archived=…&limit=…&offset=…`.
/// Full-transcript substring search, scoped to live or all sessions; with an
/// empty `q` and `include_archived=true` it browses the archive. Returns the
/// same `SessionListItem` shape as the list (so clients reuse `SessionCard`),
/// each carrying a `match_snippet` when the hit was in the transcript.
#[allow(clippy::too_many_lines)]
pub async fn search_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SessionListResponse>, (StatusCode, Json<ApiError>)> {
    let uid = ctx.owner_filter();
    // Parse the raw `q` into the AST. A blank query → `Empty` → browse.
    // A plain keyword parses to a single free-text leaf, so back-compat holds.
    let root = cctui_query::parse(params.q.trim());
    let browse = root.is_empty();
    if browse && !params.include_archived {
        return Ok(Json(SessionListResponse { sessions: vec![] }));
    }
    let text_terms = root.free_text_terms();
    let limit = params.limit.unwrap_or(SEARCH_DEFAULT_LIMIT).clamp(1, SEARCH_MAX_LIMIT);
    let offset = params.offset.unwrap_or(0).max(0);

    let rows: Vec<DbSession> = if browse {
        // Browse the archive: archived sessions only, newest first, paginated.
        // `$3` scopes to the caller (NULL = admin).
        let sql = format!(
            "{SEARCH_SELECT} WHERE s.status = 'archived' \
             AND ($3::uuid IS NULL OR m.user_id = $3) \
             ORDER BY s.registered_at DESC LIMIT $1 OFFSET $2"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(limit)
            .bind(offset)
            .bind(uid)
            .fetch_all(&state.pool)
            .await
    } else {
        // Compile the AST to a WHERE tree; `ast_params` are the leading `$1…$N`
        // binds. Ownership/scope stay outer constraints appended after them.
        //
        // Live-only scope normally drops archived rows, but a session that's
        // currently live in the in-memory registry (e.g. a dispatched worker
        // auto-archived in the DB while still running) must stay searchable —
        // the bucketed live list surfaces those via the registry, so search
        // would otherwise be the one place they vanish (item 2). OR in
        // the live registry ids so they're never filtered by status. With
        // include_archived the scope is already open, so no extra bind is needed.
        let mut ast_params: Vec<SqlParam> = Vec::new();
        let where_sql = compile_node(&root, &mut ast_params);
        tracing::debug!(q = %params.q, %where_sql, "compiled session search");
        let n = ast_params.len();
        let live_ids: Vec<String> = if params.include_archived {
            Vec::new()
        } else {
            let registry = state.registry.read().await;
            registry.list().into_iter().map(|h| h.session.id.clone()).collect()
        };
        let scope = if params.include_archived {
            "TRUE".to_string()
        } else {
            format!("(s.status <> 'archived' OR s.id = ANY(${}))", n + 1)
        };
        // Whether we bound the live_ids array (1) or not (0) — shifts limit/offset.
        let extra = usize::from(!params.include_archived);
        let (li, oi) = (n + 1 + extra, n + 2 + extra);
        let ui = oi + 1;
        let sql = format!(
            "{SEARCH_SELECT} WHERE ({scope}) AND ({where_sql}) \
             AND (${ui}::uuid IS NULL OR m.user_id = ${ui}) \
             ORDER BY s.registered_at DESC LIMIT ${li} OFFSET ${oi}"
        );
        let mut query = sqlx::query_as::<_, DbSession>(sqlx::AssertSqlSafe(sql));
        for p in &ast_params {
            query = match p {
                SqlParam::Text(s) => query.bind(s.clone()),
                SqlParam::Bool(b) => query.bind(*b),
            };
        }
        if !params.include_archived {
            query = query.bind(live_ids);
        }
        query.bind(limit).bind(offset).bind(uid).fetch_all(&state.pool).await
    }
    .map_err(|e| {
        tracing::error!("db error (session search): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;

    let with_ts: Vec<(DateTime<Utc>, SessionListItem)> = rows
        .into_iter()
        .map(|row| {
            let (status, liveness) =
                resolve_status_liveness(&row.status, row.registered_at, row.last_heartbeat);
            (
                row.registered_at,
                SessionListItem {
                    id: row.id,
                    parent_id: row.parent_id,
                    machine_id: row.machine_id,
                    working_dir: row.working_dir,
                    status,
                    liveness,
                    attention: None,
                    bucket: Bucket::Working,
                    token_usage: cctui_proto::models::TokenUsage::default(),
                    metadata: row.metadata,
                    adapter_id: row.adapter_id.map(cctui_proto::adapter::AdapterId::new),
                    machine_name: row.resolved_machine_name,
                    machine_hue: row.resolved_machine_hue,
                    machine_kind: row.resolved_machine_kind,
                    last_message_text: None,
                    last_message_at: None,
                    registered_at: Some(row.registered_at),
                    name: None,
                    model: None,
                    effort: None,
                    auto_approve: false,
                    match_snippet: None,
                    match_seq: None,
                    last_activity_at: None,
                    cache_cold: false,
                    estimated_burst_tokens: None,
                    hibernated: false,
                    pinned: false,
                    labels: Vec::new(),
                    last_heartbeat: Some(row.last_heartbeat),
                    account_name: None,
                    unread_count: 0,
                    activity_detail: None,
                    last_tool_at: None,
                    last_tool_name: None,
                    tool_use_count: 0,
                    todos: Vec::new(),
                    has_token_credentials: false,
                    account_traffic_observed: false,
                    pr_links: Vec::new(),
                    end_reason: None,
                    end_detail: None,
                    ended_at: None,
                },
            )
        })
        .collect();

    let mut sessions = enrich_and_sort(&state, Some(ctx.user_id), with_ts).await?;

    // Attach a transcript snippet per session: the most recent matching event's
    // searchable text, windowed around the keyword. Sessions matched only by
    // id/name/dir have no transcript hit and keep `match_snippet = None`.
    let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
    if !browse && !ids.is_empty() && !text_terms.is_empty() {
        let patterns: Vec<String> = text_terms.iter().map(|t| ilike_contains(t)).collect();
        let sql = snippet_sql(patterns.len());
        let mut query =
            sqlx::query_as::<_, (String, String, i64)>(sqlx::AssertSqlSafe(sql)).bind(&ids);
        for p in &patterns {
            query = query.bind(p);
        }
        let snippet_rows = query.fetch_all(&state.pool).await.map_err(|e| {
            tracing::error!("db error (search snippets): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
        attach_transcript_hits(&mut sessions, snippet_rows, &text_terms);
    }

    Ok(Json(SessionListResponse { sessions }))
}

/// Sessions with no matching event keep both fields `None`; clients read that
/// as an id/name/dir-only match and open the drawer normally.
fn attach_transcript_hits(
    sessions: &mut [SessionListItem],
    rows: Vec<(String, String, i64)>,
    terms: &[String],
) {
    let mut by_session: std::collections::HashMap<String, (String, i64)> =
        rows.into_iter().map(|(id, text, seq)| (id, (make_snippet(&text, terms), seq))).collect();
    for s in sessions {
        if let Some((snippet, seq)) = by_session.remove(&s.id) {
            s.match_snippet = Some(snippet);
            s.match_seq = Some(seq);
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct FieldValuesParams {
    pub field: String,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
}

const FIELD_VALUES_LIMIT: i64 = 50;

/// DISTINCT-values query for a dynamic autocomplete field. Trailing binds are
/// `$N+1` = owner uuid, `$N+2` = the `q` ILIKE pattern (after the `$1..$N` from
/// `context`). Empty/whitespace `context` parses to `TRUE` (context-free).
fn field_values_sql(field: &str, context: &str) -> Option<(String, Vec<SqlParam>)> {
    let (joins, value_expr, guard): (&str, &str, &str) = match field {
        "machine" => (
            "",
            "COALESCE(m.display_name, m.name)",
            "m.kind = 'persistent' AND m.deleted_at IS NULL",
        ),
        "account" => (
            "JOIN session_tokens st ON st.session_id = s.id AND st.revoked_at IS NULL \
             JOIN account_providers ap ON ap.id = st.account_id \
             JOIN accounts a ON a.id = ap.account_id",
            "a.name",
            "TRUE",
        ),
        "tag" => (
            "JOIN session_labels sl ON sl.session_id = s.id \
             JOIN labels l ON l.id = sl.label_id",
            "l.name",
            "TRUE",
        ),
        "model" => (
            "",
            "COALESCE(s.model, s.metadata->>'model')",
            "COALESCE(s.model, s.metadata->>'model', '') <> ''",
        ),
        _ => return None,
    };

    let mut ctx_params: Vec<SqlParam> = Vec::new();
    let context_sql = compile_node(&cctui_query::parse(context.trim()), &mut ctx_params);
    let n = ctx_params.len();
    let (ui, qi) = (n + 1, n + 2);
    let sql = format!(
        "SELECT DISTINCT {value_expr} AS v \
         FROM sessions s \
         LEFT JOIN machines m ON m.id = s.machine_uuid \
         {joins} \
         WHERE ({context_sql}) AND ({guard}) \
         AND (${ui}::uuid IS NULL OR m.user_id = ${ui}) \
         AND (${qi}::text IS NULL OR {value_expr} ILIKE ${qi}) \
         ORDER BY v LIMIT {FIELD_VALUES_LIMIT}"
    );
    Some((sql, ctx_params))
}

/// `GET /sessions/search/values?field=…&q=…`: autocomplete suggestions
/// for a search field. Static enums come from the query registry; dynamic
/// fields (machine/account/tag/model) are distinct values from the caller's own
/// sessions (admin sees all). Machine suggestions exclude ephemeral/dispatch
/// worker machines and soft-deleted rows — searching them by name still works,
/// they're just not suggested. Unknown fields → empty list, never an error.
pub async fn search_field_values(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Query(params): Query<FieldValuesParams>,
) -> Result<Json<Vec<String>>, (StatusCode, Json<ApiError>)> {
    let uid = ctx.owner_filter();
    let Some(def) = cctui_query::resolve(&params.field) else {
        return Ok(Json(vec![]));
    };
    if !def.enum_values.is_empty() {
        let prefix = params.q.unwrap_or_default().to_lowercase();
        return Ok(Json(
            def.enum_values
                .iter()
                .filter(|v| v.to_lowercase().starts_with(&prefix))
                .map(ToString::to_string)
                .collect(),
        ));
    }
    let like = params.q.as_deref().filter(|s| !s.is_empty()).map(ilike_contains);

    let Some((sql, ctx_params)) =
        field_values_sql(def.name, params.context.as_deref().unwrap_or(""))
    else {
        return Ok(Json(vec![]));
    };

    let mut query = sqlx::query_as::<_, (String,)>(sqlx::AssertSqlSafe(sql));
    for p in &ctx_params {
        query = match p {
            SqlParam::Text(s) => query.bind(s.clone()),
            SqlParam::Bool(b) => query.bind(*b),
        };
    }
    let rows: Vec<(String,)> =
        query.bind(uid).bind(&like).fetch_all(&state.pool).await.map_err(|e| {
            tracing::error!("db error (field values): {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

type EndRow = (Option<String>, Option<String>, Option<DateTime<Utc>>, Option<serde_json::Value>);

#[allow(clippy::too_many_lines)]
pub async fn get_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionListItem>, (StatusCode, Json<ApiError>)> {
    // Live session — serve straight from the registry.
    {
        let registry = state.registry.read().await;
        if let Some(handle) = registry.get(&session_id) {
            let item = SessionListItem {
                id: handle.session.id.clone(),
                parent_id: handle.session.parent_id.clone(),
                machine_id: handle.session.machine_id.clone(),
                working_dir: handle.session.working_dir.clone(),
                status: derive_status(handle.session.registered_at, handle.session.last_heartbeat),
                liveness: derive_liveness(handle.session.last_heartbeat),
                attention: None,
                bucket: Bucket::Working,
                token_usage: handle.token_usage.clone(),
                metadata: handle.session.metadata.clone(),
                adapter_id: handle.session.adapter_id.clone(),
                machine_name: None,
                machine_hue: None,
                machine_kind: None,
                last_message_text: None,
                last_message_at: None,
                registered_at: Some(handle.session.registered_at),
                name: None,
                model: None,
                effort: None,
                auto_approve: state
                    .permission_store
                    .read()
                    .await
                    .is_auto_approve(&handle.session.id),
                match_snippet: None,
                match_seq: None,
                last_activity_at: None,
                cache_cold: false,
                estimated_burst_tokens: None,
                hibernated: false,
                pinned: false,
                labels: Vec::new(),
                last_heartbeat: Some(handle.session.last_heartbeat),
                account_name: None,
                unread_count: 0,
                activity_detail: None,
                last_tool_at: None,
                last_tool_name: None,
                tool_use_count: 0,
                todos: Vec::new(),
                has_token_credentials: false,
                account_traffic_observed: false,
                pr_links: Vec::new(),
                end_reason: None,
                end_detail: None,
                ended_at: None,
            };
            return Ok(Json(item));
        }
    }

    // Not live — fall back to the DB so archived/ended sessions still open
    // (read-only). A true 404 now means the session was actually deleted, not
    // just archived (item 6 — kills the spurious "not found or
    // archived" toast on refresh).
    let row: Option<DbSession> =
        crate::store::sessions::fetch_by_id(&state.pool, &session_id).await.map_err(|e| {
            tracing::error!("db error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;

    let row = row.ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(ApiError { error: "session not found".into() }))
    })?;

    let (status, liveness) =
        resolve_status_liveness(&row.status, row.registered_at, row.last_heartbeat);
    let mut item = SessionListItem {
        id: row.id.clone(),
        parent_id: row.parent_id,
        machine_id: row.machine_id,
        working_dir: row.working_dir,
        status,
        liveness,
        attention: None,
        bucket: Bucket::Working,
        token_usage: cctui_proto::models::TokenUsage::default(),
        metadata: row.metadata,
        adapter_id: row.adapter_id.map(cctui_proto::adapter::AdapterId::new),
        machine_name: row.resolved_machine_name,
        machine_hue: row.resolved_machine_hue,
        machine_kind: row.resolved_machine_kind,
        last_message_text: None,
        last_message_at: None,
        registered_at: Some(row.registered_at),
        name: None,
        model: None,
        effort: None,
        auto_approve: state.permission_store.read().await.is_auto_approve(&row.id),
        match_snippet: None,
        match_seq: None,
        last_activity_at: None,
        cache_cold: false,
        estimated_burst_tokens: None,
        hibernated: false,
        pinned: false,
        labels: Vec::new(),
        last_heartbeat: Some(row.last_heartbeat),
        account_name: None,
        unread_count: 0,
        activity_detail: None,
        last_tool_at: None,
        last_tool_name: None,
        tool_use_count: 0,
        todos: Vec::new(),
        has_token_credentials: false,
        account_traffic_observed: false,
        pr_links: Vec::new(),
        end_reason: None,
        end_detail: None,
        ended_at: None,
    };
    let end: Option<EndRow> = sqlx::query_as(
        "SELECT end_reason, end_detail, ended_at, todos FROM sessions WHERE id = $1",
    )
    .bind(&item.id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    if let Some((end_reason, end_detail, ended_at, todos)) = end {
        item.end_reason = end_reason.as_deref().map(SessionEndReason::parse);
        item.end_detail = end_detail;
        item.ended_at = ended_at;
        item.todos = todos.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    }
    Ok(Json(item))
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ConversationQuery {
    /// Newest `limit` events only; absent = full transcript.
    pub limit: Option<i64>,
    /// Exclusive upper `seq` bound for paging back.
    pub before: Option<i64>,
    /// Exclusive lower `seq` bound for delta catch-up.
    pub after: Option<i64>,
    /// Which end of the range `limit` takes: `desc` (default) keeps the newest
    /// events, `asc` the oldest. The response is always oldest-first.
    #[serde(default)]
    pub order: ConversationOrder,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationOrder {
    #[default]
    Desc,
    Asc,
}

const fn conversation_sql(order: ConversationOrder) -> &'static str {
    match order {
        ConversationOrder::Desc => {
            "SELECT id, event_type, payload, created_at FROM stream_events \
             WHERE session_id = $1 AND ($2::bigint IS NULL OR id < $2) \
               AND ($4::bigint IS NULL OR id > $4) \
             ORDER BY id DESC LIMIT $3"
        }
        ConversationOrder::Asc => {
            "SELECT id, event_type, payload, created_at FROM stream_events \
             WHERE session_id = $1 AND ($2::bigint IS NULL OR id < $2) \
               AND ($4::bigint IS NULL OR id > $4) \
             ORDER BY id ASC LIMIT $3"
        }
    }
}

pub async fn get_conversation(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(params): Query<ConversationQuery>,
    req_headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, (StatusCode, Json<ApiError>)> {
    let adapter: Option<String> =
        crate::store::sessions::adapter_id(&state.pool, &session_id).await.map_err(|e| {
            tracing::error!("db error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;

    // Order by `id` (the BIGSERIAL insert sequence), not `created_at`: `id` is
    // the causal `seq` and is a strict total order, so a late-flushed
    // AskUserQuestion card+preamble keep their insert position even when their
    // `created_at` ties or lands after the user's answer.
    let mut rows: Vec<(i64, String, serde_json::Value, DateTime<Utc>)> =
        sqlx::query_as(conversation_sql(params.order))
            .bind(&session_id)
            .bind(params.before)
            .bind(params.limit.map(|l| l.clamp(1, 10_000)))
            .bind(params.after)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("db error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError { error: "database error".into() }),
                )
            })?;
    if params.order == ConversationOrder::Desc {
        rows.reverse();
    }

    let usage_rows: Vec<(String, Option<String>, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT message_id, model, \
                input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens \
         FROM session_token_usage WHERE session_id = $1",
    )
    .bind(&session_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error (message usage): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    let catalog = session_catalogs(&state, std::slice::from_ref(&session_id))
        .await
        .remove(session_id.as_str());
    let usage_by_message: HashMap<String, cctui_proto::models::TokenUsage> = usage_rows
        .into_iter()
        .map(|(message_id, model, input, output, cache_read, cache_creation)| {
            let to_u64 = |v: i64| u64::try_from(v).unwrap_or(0);
            let cost = crate::cost::tallies_cost_usd(
                catalog.as_ref(),
                &[(model, crate::cost::TokenUsage { input, cached_input: cache_read, output })],
            );
            (
                message_id,
                cctui_proto::models::TokenUsage {
                    tokens_in: to_u64(input),
                    tokens_out: to_u64(output),
                    cost_usd: cost,
                    cache_read_tokens: to_u64(cache_read),
                    cache_creation_tokens: to_u64(cache_creation),
                },
            )
        })
        .collect();

    let adapter_id = adapter.as_deref().unwrap_or("claude-code");
    // Stamp each event with `ts` (unix millis, matching the live `AgentEvent`
    // shape) derived from `created_at`, so the client renders real timestamps
    // instead of "Invalid Date". Legacy payloads that already carry `ts` keep
    // their own value.
    let normalized: Vec<serde_json::Value> = rows
        .into_iter()
        .filter_map(|(id, event_type, payload, created_at)| {
            crate::normalize::for_client(adapter_id, &event_type, payload).map(|mut v| {
                if let Some(obj) = v.as_object_mut() {
                    obj.entry("ts")
                        .or_insert_with(|| serde_json::json!(created_at.timestamp_millis()));
                    obj.insert("seq".to_owned(), serde_json::json!(id));
                    if let Some(message_id) =
                        obj.get("message_id").and_then(serde_json::Value::as_str)
                        && let Some(usage) = usage_by_message.get(message_id)
                    {
                        obj.insert("usage".to_owned(), serde_json::json!(usage));
                    }
                }
                v
            })
        })
        .collect();
    Ok(crate::http_cache::json_with_etag(&req_headers, &normalized))
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<MessageRequest>,
) -> Result<(StatusCode, Json<cctui_proto::api::SpawnResponse>), (StatusCode, Json<ApiError>)> {
    // A Task-tool subagent is observe-only: it has no worker of its own (the
    // parent's `Task` call drives it), so no adapter can ever deliver a reply
    // to it. Accepting one with a 202 claimed a message had been queued while
    // nothing was written anywhere — the janitor's revive loop burned its
    // retries on rows that could not answer (claudo/inbox#210). Refuse, and
    // name the parent, which IS addressable.
    let subagent: Option<(bool, Option<String>)> = sqlx::query_as(
        "SELECT COALESCE(metadata->>'subagent', 'false') = 'true', parent_id \
         FROM sessions WHERE id = $1",
    )
    .bind(&session_id)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);
    if let Some((true, parent)) = subagent {
        let hint = parent
            .map_or_else(String::new, |p| format!(" Send it to its parent session {p} instead."));
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError {
                error: format!(
                    "session {session_id} is a Task subagent and has no worker to \
                     receive a message.{hint}"
                ),
            }),
        ));
    }
    // Carry re-minted gateway env so a reply-driven cold-resume revives the
    // worker with a fresh valid token rather than empty env.
    let env = crate::routes::gateway::resume_env_for_session(&state, &session_id).await;
    let command_id = uuid::Uuid::new_v4();
    crate::state::track_command(
        &state.pending_commands,
        command_id,
        Some(session_id.clone()),
        None,
    );
    let dispatch = crate::bus::dispatch(
        &state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::Reply {
            local_id: session_id.clone(),
            text: req.content,
            ask_picks: None,
            env,
            command_id: Some(command_id),
        },
    )
    .await;
    if let Err(err) = dispatch {
        use crate::bus::BusError;
        let status = match err {
            BusError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        tracing::warn!(%session_id, %err, "message dispatch failed");
        return Err((status, Json(ApiError { error: err.to_string() })));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(cctui_proto::api::SpawnResponse {
            command_id,
            status: "dispatched".into(),
            account: None,
            session_id: None,
        }),
    ))
}

pub async fn rename_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<RenameRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "name must not be empty".into() }),
        ));
    }
    // Persist immediately so the UI reflects the rename without waiting for
    // the daemon round-trip. The daemon write-through (below) keeps the
    // on-disk state.json in sync so the next status poll doesn't revert it.
    sqlx::query("UPDATE sessions SET session_name = $2 WHERE id = $1")
        .bind(&session_id)
        .bind(name)
        .execute(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
        })?;
    // Best-effort propagation to the owning daemon's adapter.
    let _ = crate::bus::dispatch(
        &state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::Rename {
            local_id: session_id.clone(),
            name: name.to_owned(),
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /sessions/{id}/seen`: mark this session's messages seen for
/// the calling user by upserting the read high-water mark to `now()`. `GREATEST`
/// keeps it monotonic so a stale/out-of-order call can never rewind the cursor.
/// Owner-scoped by the `sess_write` route guard.
pub async fn mark_seen(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    sqlx::query(
        "INSERT INTO session_reads (user_id, session_id, last_seen_at) \
         VALUES ($1, $2, now()) \
         ON CONFLICT (user_id, session_id) \
         DO UPDATE SET last_seen_at = GREATEST(session_reads.last_seen_at, EXCLUDED.last_seen_at)",
    )
    .bind(ctx.user_id)
    .bind(&session_id)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error (mark seen): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn kill_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Best-effort: also dispatch to the daemon so the running worker is
    // actually killed via the `claude daemon` socket. The DB update
    // below remains source-of-truth.
    let _ = crate::bus::dispatch(
        &state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::Kill { local_id: session_id.clone(), signal: None },
    )
    .await;
    // Kill drops the in-memory handle and marks the DB row inactive. The
    // session isn't archived — later activity can revive it.
    crate::store::sessions::set_inactive(&state.pool, &session_id, false).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    {
        let mut registry = state.registry.write().await;
        registry.deregister(&session_id);
    }
    state.bus.deregister_session_stream(&session_id);
    tracing::info!(session_id = %session_id, "session killed");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/sessions/{id}/interrupt` — stop the in-flight turn without
/// tearing the session down. Dispatches `Interrupt`, NOT a
/// kill: the earlier `Kill { signal: 15 }` terminated the worker on both
/// adapters (claude `kill`-op'd the PTY worker; codex sent `turn/interrupt`
/// *and then* `terminate_child`). `Interrupt` keeps the session alive — the
/// claude adapter injects an ESC keystroke into the worker PTY via `attach`,
/// the codex adapter sends `turn/interrupt` without terminating. The DB row
/// stays active and in the registry so the session keeps going.
///
/// Mints a `command_id` so the adapter can echo back an
/// [`AdapterEvent::CommandResult`] → `ServerEvent::CommandResult`; the webui
/// awaits it to surface whether the agent actually accepted the interrupt
/// instead of firing-and-forgetting. Returns the id in the response body.
pub async fn interrupt_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<(StatusCode, Json<cctui_proto::api::SpawnResponse>), (StatusCode, Json<ApiError>)> {
    let command_id = uuid::Uuid::new_v4();
    crate::state::track_command(
        &state.pending_commands,
        command_id,
        Some(session_id.clone()),
        None,
    );
    let _ = crate::bus::dispatch(
        &state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::Interrupt {
            local_id: session_id.clone(),
            command_id: Some(command_id),
        },
    )
    .await;
    tracing::info!(session_id = %session_id, %command_id, "session interrupted");
    Ok((
        StatusCode::ACCEPTED,
        Json(cctui_proto::api::SpawnResponse {
            command_id,
            status: "dispatched".into(),
            account: None,
            session_id: None,
        }),
    ))
}

/// Body of `POST /sessions/{id}/switch-account`. `account` is an
/// `accounts.name`, an identity UUID, or a credential UUID — a credential UUID
/// also names which per-family binding is rebound; the other forms use
/// `family` (`anthropic` | `openai` | `fireworks`), defaulting to the harness's
/// family.
#[derive(Deserialize)]
pub struct SwitchAccountRequest {
    pub account: String,
    #[serde(default)]
    pub family: Option<String>,
}

/// `POST /api/v1/sessions/{id}/switch-account` — rebind one of a session's
/// per-family credential bindings to another account of the same owner.
///
/// A worker carries one opaque gateway token per provider family
/// (`ANTHROPIC_AUTH_TOKEN` / `OPENAI_API_KEY`); the gateway resolves each per
/// request via `session_tokens`. Switching is a pure server-side rebind of the
/// token row in the target's family — same env tokens, no restart, and the
/// other family's binding survives (Codex spawns keep their account when the
/// Claude side moves). 409 when the session has no binding in the target's
/// family. Success clears the soft-limit block (`SoftLimitCleared`).
#[allow(clippy::too_many_lines)]
pub async fn switch_account(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<SwitchAccountRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    use crate::routes::gateway::Family;

    let err = |code: StatusCode, msg: &str| (code, Json(ApiError { error: msg.into() }));
    let db = |e: sqlx::Error| {
        tracing::error!("db error (switch-account): {e}");
        err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
    };

    let meta: Option<(uuid::Uuid, Option<String>)> = sqlx::query_as(
        "SELECT a.user_id, s.adapter_id \
         FROM session_tokens t \
         JOIN account_providers a ON a.id = t.account_id \
         LEFT JOIN sessions s ON s.id = t.session_id \
         WHERE t.session_id = $1 AND t.revoked_at IS NULL \
         LIMIT 1",
    )
    .bind(&session_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(db)?;
    let Some((owner_id, adapter_id)) = meta else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "session has no active gateway account binding to switch",
        ));
    };
    let default_family = match req.family.as_deref().map(str::trim) {
        None | Some("") => Family::from_adapter(adapter_id.as_deref().unwrap_or("claude-code")),
        Some(label) => match Family::from_label(label) {
            Some(f) => f,
            None => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "family must be `anthropic`, `openai` or `fireworks`",
                ));
            }
        },
    };

    // A UUID may be either level: a credential id — which names its
    // own family — or an identity id, resolved in `default_family` like a name.
    let target: Option<(uuid::Uuid, String)> =
        if let Ok(tid) = uuid::Uuid::parse_str(req.account.trim()) {
            let direct: Option<(uuid::Uuid, String)> = sqlx::query_as(
                "SELECT id, family FROM account_providers WHERE id = $1 AND user_id = $2",
            )
            .bind(tid)
            .bind(owner_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(db)?;
            if direct.is_some() {
                direct
            } else {
                sqlx::query_as(
                    "SELECT ap.id, ap.family \
                 FROM account_providers ap JOIN accounts a ON a.id = ap.account_id \
                 WHERE a.id = $1 AND ap.user_id = $2 AND ap.family = $3 \
                 LIMIT 1",
                )
                .bind(tid)
                .bind(owner_id)
                .bind(default_family.label())
                .fetch_optional(&state.pool)
                .await
                .map_err(db)?
            }
        } else {
            sqlx::query_as(
                "SELECT ap.id, ap.family \
             FROM account_providers ap JOIN accounts a ON a.id = ap.account_id \
             WHERE a.name = $1 AND ap.user_id = $2 AND ap.family = $3 \
             LIMIT 1",
            )
            .bind(req.account.trim())
            .bind(owner_id)
            .bind(default_family.label())
            .fetch_optional(&state.pool)
            .await
            .map_err(db)?
        };
    let Some((target_id, target_family)) = target else {
        return Err(err(StatusCode::NOT_FOUND, "no such account for this session's owner"));
    };

    let current: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT ap.id \
         FROM session_tokens t JOIN account_providers ap ON ap.id = t.account_id \
         WHERE t.session_id = $1 AND t.revoked_at IS NULL AND ap.family = $2 \
         ORDER BY t.created_at DESC LIMIT 1",
    )
    .bind(&session_id)
    .bind(&target_family)
    .fetch_optional(&state.pool)
    .await
    .map_err(db)?;
    let Some((current_account_id,)) = current else {
        return Err(err(
            StatusCode::CONFLICT,
            "session has no binding in the target's provider family; cross-provider switching is not supported",
        ));
    };

    if target_id == current_account_id {
        return Err(err(StatusCode::CONFLICT, "session is already on that account"));
    }

    // Repoint only this family's row: touching the other family's would hand
    // e.g. the worker's OPENAI_API_KEY an anthropic credential.
    let updated = sqlx::query(
        "UPDATE session_tokens SET account_id = $3 \
         WHERE session_id = $1 AND revoked_at IS NULL AND account_id = $2",
    )
    .bind(&session_id)
    .bind(current_account_id)
    .bind(target_id)
    .execute(&state.pool)
    .await
    .map_err(db)?;
    if updated.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no active token row to rebind"));
    }

    // The rebind reuses the SAME token string — clear any orphan-spam block on
    // its fingerprint so the fixed binding resolves immediately instead of
    // 401ing for the remainder of the (up to 300s) block window.
    crate::routes::gateway::clear_orphan_block_for_session(&state, &session_id).await;

    // Dismiss the per-chat soft-limit banner.
    crate::routes::gateway::clear_soft_limit_block(&state, &session_id).await;
    tracing::info!(
        session_id = %session_id,
        from = %current_account_id,
        to = %target_id,
        family = %target_family,
        "switched session account (soft-limit rebind)"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// One of a session's active per-family gateway credential bindings.
#[derive(serde::Serialize)]
pub struct SessionBinding {
    pub family: String,
    pub credential_id: uuid::Uuid,
    pub account_id: uuid::Uuid,
    pub account_name: String,
}

/// `GET /api/v1/sessions/{id}/bindings` — the session's active per-family
/// credential bindings, one entry per family it carries.
pub async fn session_bindings(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<SessionBinding>>, (StatusCode, Json<ApiError>)> {
    let rows: Vec<(String, uuid::Uuid, uuid::Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (ap.family) ap.family, ap.id, acc.id, acc.name \
         FROM session_tokens t \
         JOIN account_providers ap ON ap.id = t.account_id \
         JOIN accounts acc ON acc.id = ap.account_id \
         WHERE t.session_id = $1 AND t.revoked_at IS NULL \
         ORDER BY ap.family, t.created_at DESC",
    )
    .bind(&session_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error (session-bindings): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    Ok(Json(
        rows.into_iter()
            .map(|(family, credential_id, account_id, account_name)| SessionBinding {
                family,
                credential_id,
                account_id,
                account_name,
            })
            .collect(),
    ))
}

/// `POST /api/v1/sessions/{id}/resume` — explicitly revive an exited durable
/// conversation in place. The adapter reuses the original transcript identity;
/// this is not a fork.
pub async fn resume_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Pass the working_dir so the daemon can resume even after archiving ran
    // `claude rm` (which deletes the on-disk job state.json but keeps the
    // conversation transcript) — the daemon falls back to local_id + this cwd.
    let working_dir: Option<String> =
        crate::store::sessions::working_dir(&state.pool, &session_id).await.ok().flatten();

    // Re-mint the gateway env for the session's bound OAuth account so the
    // revived worker keeps routing through the gateway instead of hitting the
    // default upstream with no credential and 401ing.
    let env = crate::routes::gateway::resume_env_for_session(&state, &session_id).await;

    crate::bus::dispatch(
        &state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::Resume {
            local_id: session_id.clone(),
            working_dir,
            env,
        },
    )
    .await
    .map_err(|e| {
        tracing::warn!(%session_id, error = %e, "resume dispatch failed");
        (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: format!("resume failed: {e}") }))
    })?;
    crate::store::sessions::set_inactive(&state.pool, &session_id, true).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    tracing::info!(session_id = %session_id, "resume dispatched");
    Ok(StatusCode::ACCEPTED)
}

/// `POST /api/v1/sessions/{id}/set-model` — change the model and/or reasoning
/// effort of a running session in place. Agent-asymmetric: the codex
/// adapter records the override and carries it on the next `turn/start` (a
/// stable per-turn override), then echoes the resolved model/effort
/// back as an `AdapterEvent::Status` (which updates the DB row + chip); the
/// claude-code adapter rejects it with a clear "fork to change model" error
/// (the webui pre-empts that by offering the fork affordance for claude
/// sessions).
///
/// Mints a `command_id` so the adapter echoes back an
/// `AdapterEvent::CommandResult` → `ServerEvent::CommandResult`; the webui
/// awaits it before confirming the change, rather than the old fire-and-forget
/// 204 that reported success even when the app-server rejected the change.
/// Returns the id in the response body, mirroring interrupt.
pub async fn set_model(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<cctui_proto::api::SetModelRequest>,
) -> Result<(StatusCode, Json<cctui_proto::api::SpawnResponse>), (StatusCode, Json<ApiError>)> {
    let norm = |s: Option<String>| s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let model = norm(req.model);
    let effort = norm(req.effort);
    if model.is_none() && effort.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "model or effort must be set".into() }),
        ));
    }
    let command_id = uuid::Uuid::new_v4();
    crate::state::track_command(
        &state.pending_commands,
        command_id,
        Some(session_id.clone()),
        None,
    );
    let _ = crate::bus::dispatch(
        &state,
        &session_id,
        cctui_proto::adapter::AdapterCommand::SetModel {
            local_id: session_id.clone(),
            model: model.clone(),
            effort: effort.clone(),
            command_id: Some(command_id),
        },
    )
    .await;
    tracing::info!(session_id = %session_id, %command_id, ?model, ?effort, "set-model dispatched");
    Ok((
        StatusCode::ACCEPTED,
        Json(cctui_proto::api::SpawnResponse {
            command_id,
            status: "dispatched".into(),
            account: None,
            session_id: None,
        }),
    ))
}

/// `POST /api/v1/sessions/{id}/fork` — fork an existing conversation into a
/// brand-new session, optionally changing model/effort at fork time.
///
/// Resolves the parent's `(adapter_id, machine_uuid, working_dir)` from the DB,
/// builds a [`SessionSpec`] (`working_dir` inherited from the parent; model/effort
/// from the request, which the webui pre-fills with the parent's current
/// values), and dispatches an [`AdapterCommand::Fork`] to the owning daemon. The
/// child links back to the parent via `SessionMeta::parent_local_id` on its
/// `SessionStarted` (resolved into `parent_id` server-side) — the parent row is
/// left untouched, so reopening an archived session as a fork does not revive or
/// re-flip it. Returns a `command_id` the webui can await like a spawn.
pub async fn fork_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<cctui_proto::api::ForkRequest>,
) -> Result<(StatusCode, Json<cctui_proto::api::ForkResponse>), (StatusCode, Json<ApiError>)> {
    let norm = |s: Option<String>| s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());

    // Resolve the parent: adapter + machine + cwd. The fork inherits the
    // parent's working directory and adapter; only model/effort/name/prompt can
    // be overridden by the caller.
    let row: Option<(Option<String>, Option<uuid::Uuid>, String)> =
        sqlx::query_as("SELECT adapter_id, machine_uuid, working_dir FROM sessions WHERE id = $1")
            .bind(&session_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("db error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError { error: "database error".into() }),
                )
            })?;
    let Some((adapter_id, machine_uuid, working_dir)) = row else {
        return Err((StatusCode::NOT_FOUND, Json(ApiError { error: "session not found".into() })));
    };
    let Some(adapter_id) = adapter_id else {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError { error: "session has no adapter (legacy) — cannot fork".into() }),
        ));
    };
    let Some(machine_uuid) = machine_uuid else {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError { error: "session has no machine — cannot fork".into() }),
        ));
    };

    // Subset forks slice the parent's on-disk transcript — a claude
    // primitive only. Codex has no partial-fork mechanism, so reject it here
    // rather than let the daemon silently full-fork.
    if req.extract.is_some() && adapter_id != "claude-code" {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiError {
                error: "partial fork (from/after/selected messages) is only supported for claude sessions".into(),
            }),
        ));
    }

    let command_id = uuid::Uuid::new_v4();
    // Pre-mint the child session id for claude (which accepts a caller-supplied
    // `--session-id`) so we can return it and the webui can open the new
    // conversation right away. Codex mints its own thread id, so we
    // don't claim one for it.
    let is_claude = adapter_id == "claude-code";
    let child_session_id = is_claude.then(|| uuid::Uuid::new_v4().to_string());
    let service_tier = if crate::routes::gateway::Family::from_adapter(&adapter_id)
        == crate::routes::gateway::Family::Openai
    {
        let account_settings =
            crate::routes::gateway::resolve_session_settings(&state, &session_id).await;
        Some(crate::settings_catalog::codex::resolve_service_tier(None, account_settings.as_ref()))
    } else {
        None
    };
    let spec = cctui_proto::adapter::SessionSpec {
        adapter_id: cctui_proto::adapter::AdapterId::new(&adapter_id),
        working_dir: Some(working_dir),
        prompt: norm(req.prompt),
        name: norm(req.name),
        permission_mode: None,
        effort: norm(req.effort),
        model: norm(req.model),
        service_tier,
        env: std::collections::BTreeMap::new(),
        bootstrap: serde_json::Value::Null,
        parent_local_id: None,
    };
    let frame = cctui_proto::ws::DaemonFrameDown::Command {
        adapter_id: adapter_id.clone(),
        command: Box::new(cctui_proto::adapter::AdapterCommand::Fork {
            parent_local_id: session_id.clone(),
            spec,
            command_id: Some(command_id),
            session_id: child_session_id.clone(),
            extract: req.extract,
        }),
    };
    state.bus.command_daemon_for_session(machine_uuid, &session_id, frame).await.map_err(
        |err| match err {
            crate::bus::BusError::NoDaemon(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError { error: "daemon for that machine is offline".into() }),
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError { error: "daemon disconnected mid-dispatch".into() }),
            ),
        },
    )?;
    tracing::info!(parent = %session_id, %command_id, %adapter_id, child = ?child_session_id, "fork dispatched");
    Ok((
        StatusCode::ACCEPTED,
        Json(cctui_proto::api::ForkResponse {
            command_id,
            status: "dispatched".into(),
            session_id: child_session_id,
        }),
    ))
}

/// `POST /api/v1/sessions/{id}/auto-approve` — toggle cctui-side auto-approve.
/// When on, incoming permission requests for this session are
/// answered `allow` immediately. In-memory; reset on server restart.
pub async fn set_auto_approve(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(req): Json<cctui_proto::api::AutoApproveRequest>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    state.permission_store.write().await.set_auto_approve(&session_id, req.enabled);
    tracing::info!(session_id = %session_id, enabled = req.enabled, "auto-approve toggled");
    Ok(StatusCode::NO_CONTENT)
}

/// Archive a session: dismiss it from the default list. Best-effort dispatches
/// `Remove` to the owning daemon — for claude-code that stops the worker and
/// then runs `claude rm` so the session also disappears from Claude Code's
/// native `claude agents` view; for codex it is a plain kill. Then
/// marks the row `archived` and drops it from the live registry. The
/// conversation transcript is preserved. Reversible (cctui-side) via
/// `unarchive_session`, though the underlying claude job is gone by then.
///
/// A pinned session is the human's "do not lose this" mark: it is refused
/// with `409 Conflict` unless the caller passes `?force=true`, which only the
/// webui's deliberate single-row gesture does. Scripts, macros and sweeps
/// never force, so a pin is never undone by an automation.
pub async fn archive_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(q): Query<ArchiveQuery>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let outcome = archive_one(&state, &session_id, q.force).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    match outcome {
        ArchiveOutcome::Archived => Ok(StatusCode::NO_CONTENT),
        ArchiveOutcome::SkippedPinned => Err((
            StatusCode::CONFLICT,
            Json(ApiError { error: "session is pinned; unpin it or pass ?force=true".into() }),
        )),
    }
}

/// Query string of `POST /sessions/{id}/archive`.
#[derive(Debug, Default, Deserialize)]
pub struct ArchiveQuery {
    /// Archive even when the session is pinned. Reserved for an explicit human
    /// gesture on that one session; never set by automations.
    #[serde(default)]
    pub force: bool,
}

/// What [`archive_one`] did with a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOutcome {
    Archived,
    /// The session is pinned and `force` was not set: nothing was touched.
    SkippedPinned,
}

/// Ask the owning daemon to remove `session_id`'s underlying job. Tracked so a
/// refusal surfaces as a `ServerEvent::CommandResult` rather than reading as a
/// clean archive; an undeliverable dispatch leaves the job for the daemon's
/// `ResumeMarks` reconcile to remove on reconnect.
pub async fn dispatch_remove(state: &AppState, session_id: &str) {
    let command_id = uuid::Uuid::new_v4();
    crate::state::track_command(
        &state.pending_commands,
        command_id,
        Some(session_id.to_owned()),
        None,
    );
    if let Err(err) = crate::bus::dispatch(
        state,
        session_id,
        cctui_proto::adapter::AdapterCommand::Remove {
            local_id: session_id.to_owned(),
            command_id: Some(command_id),
        },
    )
    .await
    {
        state.pending_commands.remove(&command_id);
        tracing::warn!(
            %session_id,
            %err,
            "archive could not reach the owning daemon; job removal deferred to reconcile"
        );
    }
}

/// Archive a single session (+ its subagents) — the reusable core shared by the
/// single-session route and the batch route. Dispatches `Remove`, marks the row
/// `archived`, clears classifier signals, and drops it from the live registry.
///
/// Pinned sessions are protected: without `force` a pinned session (or a
/// pinned child of the session) is left untouched and the call reports
/// [`ArchiveOutcome::SkippedPinned`]. `force` is the human's explicit
/// single-session gesture; every automation passes `false`.
pub async fn archive_one(
    state: &AppState,
    session_id: &str,
    force: bool,
) -> Result<ArchiveOutcome, sqlx::Error> {
    if !force {
        let pinned: Option<bool> = sqlx::query_scalar("SELECT pinned FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&state.pool)
            .await?;
        if pinned == Some(true) {
            tracing::info!(session_id = %session_id, "archive skipped: session is pinned");
            return Ok(ArchiveOutcome::SkippedPinned);
        }
    }
    dispatch_remove(state, session_id).await;
    // Archive the session AND any Task-tool subagents nested under it:
    // a parent's children should never outlive it in the list.
    // Subagents are observe-only (no worker), so they need no `claude rm` —
    // only the parent does, handled by the dispatch above. Archiving a
    // *child* does not touch the parent (no `parent_id` cascade upward).
    // A pinned child is never swept along with its parent unless forced.
    let children: Vec<String> =
        crate::store::sessions::child_ids(&state.pool, session_id).await.unwrap_or_default();
    // Clear the classifier signals on archive so a session that was waiting on
    // input doesn't keep its ✋ "needs input" glyph in the archived view — an
    // archived session is, by definition, no longer waiting on anyone.
    let archived: Vec<String> = sqlx::query_scalar(
        "UPDATE sessions SET status = 'archived', tempo = NULL, agent_state = NULL, \
                activity = NULL, soft_limit_reason = NULL \
         WHERE (id = $1 OR parent_id = $1) AND (pinned = false OR $2) \
         RETURNING id",
    )
    .bind(session_id)
    .bind(force)
    .fetch_all(&state.pool)
    .await?;
    let children: Vec<String> = children.into_iter().filter(|c| archived.contains(c)).collect();
    {
        let mut registry = state.registry.write().await;
        registry.deregister(session_id);
        for child in &children {
            registry.deregister(child);
        }
    }
    state.bus.deregister_session_stream(session_id);
    crate::state::drop_usage_notice_buckets(&state.usage_notice_buckets, session_id);
    for child in &children {
        state.bus.deregister_session_stream(child);
        crate::state::drop_usage_notice_buckets(&state.usage_notice_buckets, child);
    }
    tracing::info!(session_id = %session_id, children = children.len(), "session archived");
    Ok(ArchiveOutcome::Archived)
}

/// Un-archive a session: clear the sticky `archived` state back to
/// `inactive` so it reappears in the default list and can re-derive its
/// status from activity.
pub async fn unarchive_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    unarchive_one(&state, &session_id).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unarchive_one(state: &AppState, session_id: &str) -> Result<(), sqlx::Error> {
    crate::store::sessions::set_inactive(&state.pool, session_id, true).await?;
    tracing::info!(session_id = %session_id, "session unarchived");
    Ok(())
}

/// Pin (star) a session: it sorts above everything in the live list
/// and is exempt from the auto-archive reaper regardless of heartbeat age.
/// Pinning an already-archived session also un-archives it so it reappears.
pub async fn pin_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    pin_one(&state, &session_id).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn pin_one(state: &AppState, session_id: &str) -> Result<(), sqlx::Error> {
    // Pinning un-archives so a pinned session is always visible in the live
    // list. `archived` -> `inactive`; other statuses are left untouched.
    sqlx::query(
        "UPDATE sessions \
         SET pinned = true, pinned_at = now(), \
             status = CASE WHEN status = 'archived' THEN 'inactive' ELSE status END \
         WHERE id = $1",
    )
    .bind(session_id)
    .execute(&state.pool)
    .await?;
    tracing::info!(session_id = %session_id, "session pinned");
    Ok(())
}

/// Unpin a session — returns it to normal recency sorting and makes it eligible
/// for auto-archive again.
pub async fn unpin_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    unpin_one(&state, &session_id).await.map_err(|e| {
        tracing::error!("db error: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unpin_one(state: &AppState, session_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET pinned = false, pinned_at = NULL WHERE id = $1")
        .bind(session_id)
        .execute(&state.pool)
        .await?;
    tracing::info!(session_id = %session_id, "session unpinned");
    Ok(())
}

/// `POST /api/v1/sessions/pin` — pin many sessions in one request. Mirrors the
/// batch archive route; per-id failures are logged but don't abort the batch.
// Linear batch loop with per-id error handling; complexity is per-id, not nesting.
#[allow(clippy::cognitive_complexity)]
pub async fn pin_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<BatchIds>,
) -> StatusCode {
    let ids = match filter_owned_ids(&state, &ctx, &req.ids).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("db error (batch pin authz): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };
    let mut ok = 0usize;
    for id in &ids {
        match pin_one(&state, id).await {
            Ok(()) => ok += 1,
            Err(e) => tracing::error!(session_id = %id, "batch pin db error: {e}"),
        }
    }
    tracing::info!(pinned = ok, requested = req.ids.len(), "batch pin");
    StatusCode::NO_CONTENT
}

/// `POST /api/v1/sessions/unpin` — the batch mirror of `unpin_session`.
// Linear batch loop with per-id error handling; complexity is per-id, not nesting.
#[allow(clippy::cognitive_complexity)]
pub async fn unpin_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<BatchIds>,
) -> StatusCode {
    let ids = match filter_owned_ids(&state, &ctx, &req.ids).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("db error (batch unpin authz): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };
    let mut ok = 0usize;
    for id in &ids {
        match unpin_one(&state, id).await {
            Ok(()) => ok += 1,
            Err(e) => tracing::error!(session_id = %id, "batch unpin db error: {e}"),
        }
    }
    tracing::info!(unpinned = ok, requested = req.ids.len(), "batch unpin");
    StatusCode::NO_CONTENT
}

/// Body for the batch archive/unarchive routes.
#[derive(Deserialize)]
pub struct BatchIds {
    pub ids: Vec<String>,
}

/// `POST /api/v1/sessions/archive` — archive many sessions in one request
/// (multi-select). Each id is processed via [`archive_one`]; a per-id
/// failure is logged but does not abort the rest of the batch. Idempotent:
/// re-archiving an already-archived id is a no-op.
// Linear batch loop with per-id error handling; complexity is per-id, not nesting.
#[allow(clippy::cognitive_complexity)]
pub async fn archive_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<BatchIds>,
) -> StatusCode {
    let ids = match filter_owned_ids(&state, &ctx, &req.ids).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("db error (batch archive authz): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };
    // A batch never touches a pinned session: a star means "do not lose this",
    // and a multi-select or a section sweep is exactly where one slips in.
    let mut ok = 0usize;
    let mut pinned = 0usize;
    for id in &ids {
        match archive_one(&state, id, false).await {
            Ok(ArchiveOutcome::Archived) => ok += 1,
            Ok(ArchiveOutcome::SkippedPinned) => pinned += 1,
            Err(e) => tracing::error!(session_id = %id, "batch archive db error: {e}"),
        }
    }
    tracing::info!(
        archived = ok,
        skipped_pinned = pinned,
        requested = req.ids.len(),
        "batch archive"
    );
    StatusCode::NO_CONTENT
}

/// `POST /api/v1/sessions/unarchive` — the batch mirror of `unarchive_session`.
// Linear batch loop with per-id error handling; complexity is per-id, not nesting.
#[allow(clippy::cognitive_complexity)]
pub async fn unarchive_sessions(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<BatchIds>,
) -> StatusCode {
    let ids = match filter_owned_ids(&state, &ctx, &req.ids).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("db error (batch unarchive authz): {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };
    let mut ok = 0usize;
    for id in &ids {
        match unarchive_one(&state, id).await {
            Ok(()) => ok += 1,
            Err(e) => tracing::error!(session_id = %id, "batch unarchive db error: {e}"),
        }
    }
    tracing::info!(unarchived = ok, requested = req.ids.len(), "batch unarchive");
    StatusCode::NO_CONTENT
}

pub async fn set_session_policy(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(rules): Json<Vec<crate::policy::PolicyRule>>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    {
        let mut registry = state.registry.write().await;
        registry.set_policy(&session_id, rules);
    }
    Ok(StatusCode::OK)
}

/// normalize a session's last-message text for the sessions
/// table. Collapses every run of ASCII/Unicode whitespace (newlines,
/// tabs, NBSP) into a single space so multi-line snippets don't blow
/// up row height when CSS doesn't fully suppress wrapping, and caps at
/// 200 chars + ellipsis as a backstop. CSS handles the *visual* truncation
/// to the column's actual rendered width (`text-overflow: ellipsis`).
fn normalize_last_message(s: &str) -> String {
    const MAX_CHARS: usize = 200;
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut iter = collapsed.chars();
    let head: String = iter.by_ref().take(MAX_CHARS).collect();
    if iter.next().is_some() { format!("{head}…") } else { head }
}

/// `PUT /api/v1/sessions/{id}/draft`. Replace a draft's stored spawn payload
/// in place, so an edit or autosave never has to discard and recreate the
/// row. Only `draft` rows are touched; env values are never stored.
pub async fn update_draft(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(session_id): Path<String>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<SpawnResponse>, (StatusCode, Json<ApiError>)> {
    let draft_id =
        uuid::Uuid::parse_str(&session_id).map_err(|_| bad_request("session id must be a uuid"))?;
    let machine_uuid = resolve_owned_machine(&state, &ctx, &req.machine_id).await?;
    let fields = DraftRowFields::from_request(&req);
    let draft_json = serde_json::to_value(fields.payload).map_err(|e| {
        tracing::error!("serializing draft payload: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError { error: "could not encode draft".into() }),
        )
    })?;
    let metadata =
        serde_json::json!({ "draft": draft_json, "draft_saved_at": Utc::now().to_rfc3339() });
    let res = sqlx::query(
        r"UPDATE sessions
          SET machine_id = $2, machine_uuid = $3, working_dir = $4, metadata = $5,
              adapter_id = $6, session_name = $7, model = $8, effort = $9,
              last_heartbeat = now()
          WHERE id = $1 AND status = 'draft'",
    )
    .bind(&session_id)
    .bind(&req.machine_id)
    .bind(machine_uuid)
    .bind(&req.working_dir)
    .bind(&metadata)
    .bind(&fields.adapter_id)
    .bind(fields.name)
    .bind(fields.model)
    .bind(fields.effort)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error (update draft): {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: "database error".into() }))
    })?;
    if res.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, Json(ApiError { error: "draft not found".into() })));
    }
    tracing::info!(draft = %session_id, "draft updated");
    Ok(Json(SpawnResponse {
        command_id: draft_id,
        status: "draft".into(),
        account: None,
        session_id: None,
    }))
}

/// The row columns mirrored from a draft payload. Blank strings become NULL
/// so the list renders the row exactly like a freshly saved draft.
struct DraftRowFields<'a> {
    payload: SpawnRequest,
    adapter_id: String,
    name: Option<&'a str>,
    model: Option<&'a str>,
    effort: Option<&'a str>,
}

impl<'a> DraftRowFields<'a> {
    fn from_request(req: &'a SpawnRequest) -> Self {
        let mut payload = req.clone();
        payload.env.clear();
        payload.save_draft = false;
        let non_blank = |v: &'a Option<String>| v.as_deref().filter(|s| !s.trim().is_empty());
        Self {
            payload,
            adapter_id: req.adapter_id.clone().unwrap_or_else(|| "claude-code".to_owned()),
            name: non_blank(&req.name),
            model: non_blank(&req.model),
            effort: non_blank(&req.effort),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Bucket, SessionChild, SqlParam, attention_from_bucket, bucket_from_signals, cap_unread,
        compile_node, derive_liveness, field_values_sql, make_snippet, normalize_last_message,
        snippet_sql,
    };
    use cctui_proto::models::{Attention, Liveness};
    use chrono::{Duration, Utc};

    #[test]
    fn a_queued_session_is_waiting_not_dead() {
        let old = Utc::now() - Duration::hours(3);
        let (status, liveness) = super::resolve_status_liveness("queued", old, old);
        assert_eq!(status, cctui_proto::models::SessionStatus::Queued);
        assert_eq!(liveness, Liveness::Stale, "clients read Dead as finished");
        let (_, ended) = super::resolve_status_liveness("ended", old, old);
        assert_eq!(ended, Liveness::Dead);
    }

    fn bare_session(id: &str) -> cctui_proto::api::SessionListItem {
        serde_json::from_value(serde_json::json!({
            "id": id, "parent_id": null, "machine_id": "m", "working_dir": "/w",
            "status": "active", "metadata": {},
            "token_usage": serde_json::to_value(
                cctui_proto::models::TokenUsage::default()
            ).unwrap(),
        }))
        .unwrap()
    }

    #[test]
    fn transcript_hits_attach_seq_and_leave_id_only_matches_none() {
        let mut sessions = vec![bare_session("s-transcript"), bare_session("s-id-only")];
        let rows =
            vec![("s-transcript".to_owned(), "the quick brown fox jumps".to_owned(), 4242_i64)];

        super::attach_transcript_hits(&mut sessions, rows, &["brown".to_owned()]);

        assert_eq!(sessions[0].match_seq, Some(4242));
        assert!(sessions[0].match_snippet.as_deref().unwrap().contains("brown"));

        assert_eq!(sessions[1].match_seq, None);
        assert_eq!(sessions[1].match_snippet, None);
    }

    #[test]
    fn conversation_order_defaults_to_desc() {
        let q: super::ConversationQuery =
            serde_json::from_value(serde_json::json!({"limit": 20})).unwrap();
        assert_eq!(q.order, super::ConversationOrder::Desc);
        assert!(super::conversation_sql(q.order).ends_with("ORDER BY id DESC LIMIT $3"));
    }

    #[test]
    fn conversation_order_asc_flips_the_inner_order_by() {
        let q: super::ConversationQuery =
            serde_json::from_value(serde_json::json!({"limit": 20, "order": "asc"})).unwrap();
        assert_eq!(q.order, super::ConversationOrder::Asc);
        assert!(super::conversation_sql(q.order).ends_with("ORDER BY id ASC LIMIT $3"));
        let desc = super::conversation_sql(super::ConversationOrder::Desc);
        assert_eq!(
            super::conversation_sql(super::ConversationOrder::Asc).replace("id ASC", "id DESC"),
            desc,
            "asc/desc must differ only in the ORDER BY direction"
        );
    }

    #[test]
    fn draft_row_fields_strip_env_and_blank_columns() {
        let req: super::SpawnRequest = serde_json::from_value(serde_json::json!({
            "machine_id": "m", "working_dir": "/w", "prompt": "hi", "prompt_name": null,
            "name": "  ", "model": "opus", "effort": "",
            "env": {"TOKEN": "secret"}, "env_keys": ["TOKEN"], "save_draft": true
        }))
        .unwrap();
        let f = super::DraftRowFields::from_request(&req);
        assert_eq!(f.adapter_id, "claude-code");
        assert_eq!(f.name, None);
        assert_eq!(f.model, Some("opus"));
        assert_eq!(f.effort, None);
        assert!(f.payload.env.is_empty());
        assert!(!f.payload.save_draft);
        assert_eq!(f.payload.env_keys, vec!["TOKEN"]);
        let json = serde_json::to_value(&f.payload).unwrap();
        assert!(json.get("env").is_none(), "{json}");
    }

    fn compile(q: &str) -> (String, Vec<String>) {
        let mut params = Vec::new();
        let sql = compile_node(&cctui_query::parse(q), &mut params);
        let texts = params
            .into_iter()
            .map(|p| match p {
                SqlParam::Text(s) => s,
                SqlParam::Bool(b) => b.to_string(),
            })
            .collect();
        (sql, texts)
    }

    #[test]
    fn field_values_empty_context_is_context_free() {
        let (sql, ctx) = field_values_sql("model", "").expect("model is dynamic");
        assert!(ctx.is_empty());
        assert!(sql.contains("WHERE (TRUE) AND"), "{sql}");
        assert!(sql.contains("$1::uuid IS NULL"), "{sql}");
        assert!(sql.contains("$2::text IS NULL"), "{sql}");
    }

    #[test]
    fn field_values_context_ands_in_and_shifts_binds() {
        let (sql, ctx) = field_values_sql("model", "account=acctbot").expect("model is dynamic");
        assert_eq!(ctx.len(), 1);
        match &ctx[0] {
            SqlParam::Text(s) => assert_eq!(s, "acctbot"),
            SqlParam::Bool(_) => panic!("expected text bind"),
        }
        assert!(sql.contains("account_providers"), "context predicate present:\n{sql}");
        assert!(sql.contains("lower(${1}") || sql.contains("lower($1"), "{sql}");
        assert!(sql.contains("$2::uuid IS NULL"), "{sql}");
        assert!(sql.contains("$3::text IS NULL"), "{sql}");
    }

    #[test]
    fn field_values_unknown_field_is_none() {
        assert!(field_values_sql("nope", "").is_none());
    }

    #[test]
    fn cap_unread_clamps_to_badge_ceiling() {
        assert_eq!(cap_unread(0), 0);
        assert_eq!(cap_unread(5), 5);
        assert_eq!(cap_unread(99), 99);
        assert_eq!(cap_unread(100), 99);
        assert_eq!(cap_unread(1_000_000), 99);
        assert_eq!(cap_unread(-1), 0);
    }

    #[test]
    fn compile_plain_keyword_matches_legacy_shape() {
        let (sql, params) = compile("hello");
        assert!(sql.contains("s.id ILIKE $1"));
        assert!(sql.contains("s.working_dir ILIKE $1"));
        assert!(sql.contains("left(e.search_text, 8192) ILIKE $1"));
        assert_eq!(params, vec!["%hello%".to_string()]);
    }

    #[test]
    fn compile_multi_keywords_and() {
        let (sql, params) = compile("foo bar");
        assert!(sql.contains(" AND "));
        assert_eq!(params, vec!["%foo%".to_string(), "%bar%".to_string()]);
        assert!(sql.contains("$1") && sql.contains("$2"));
    }

    #[test]
    fn compile_or_between_keywords() {
        let (sql, _) = compile("foo OR bar");
        // Terms joined by OR at the top level; the only AND is the one inside
        // the free-text EXISTS subquery, never a top-level `) AND (` join.
        assert!(sql.contains(") OR ("), "top-level OR join:\n{sql}");
        assert!(!sql.contains(") AND ("), "no top-level AND join:\n{sql}");
    }

    #[test]
    fn compile_machine_filter() {
        let (sql, params) = compile("machine:dev1");
        assert!(sql.contains("COALESCE(m.display_name, m.name)) = lower($1)"));
        assert!(sql.contains("lower(s.machine_id) = lower($1)"));
        assert_eq!(params, vec!["dev1".to_string()]);
    }

    #[test]
    fn compile_tag_filter_uses_label_join() {
        let (sql, params) = compile("tag:infra");
        assert!(sql.contains("session_labels"));
        assert!(sql.contains("lower(l.name) = lower($1)"));
        assert_eq!(params, vec!["infra".to_string()]);
    }

    #[test]
    fn compile_account_filter_uses_token_join() {
        let (sql, _) = compile("account:personal");
        assert!(sql.contains("session_tokens"));
        assert!(sql.contains("lower(a.name) = lower($1)"));
    }

    #[test]
    fn compile_title_contains() {
        let (sql, params) = compile("title:fix");
        assert!(sql.contains("s.session_name, '') ILIKE $1"));
        assert_eq!(params, vec!["%fix%".to_string()]);
    }

    #[test]
    fn compile_model_filter_falls_back_to_metadata() {
        let (sql, params) = compile("model:claude-opus-4-7[1m]");
        assert!(sql.contains("COALESCE(s.model, s.metadata->>'model', '') ILIKE $1"));
        assert_eq!(params, vec!["%claude-opus-4-7[1m]%".to_string()]);
    }

    #[test]
    fn compile_negation() {
        let (sql, _) = compile("-machine:dev1");
        assert!(sql.starts_with("(NOT "));
    }

    #[test]
    fn compile_group_or_and_keyword() {
        let (sql, params) = compile("( machine:m2pro OR machine:dev1 ) AND keyword");
        assert!(sql.contains(" OR "));
        assert!(sql.contains(" AND "));
        assert_eq!(params.len(), 3);
        assert_eq!(params[2], "%keyword%");
    }

    #[test]
    fn compile_pinned_binds_bool() {
        let mut params = Vec::new();
        let sql = compile_node(&cctui_query::parse("pinned:true"), &mut params);
        assert!(sql.contains("s.pinned = $1"));
        assert!(matches!(params.as_slice(), [SqlParam::Bool(true)]));
    }

    #[test]
    fn compile_status_in_list() {
        let (sql, params) = compile("status:active,inactive");
        assert!(sql.contains("lower(s.status) = lower($1)"));
        assert!(sql.contains("lower(s.status) = lower($2)"));
        assert!(sql.contains(" OR "));
        assert_eq!(params, vec!["active".to_string(), "inactive".to_string()]);
    }

    #[test]
    fn compile_malformed_degrades_to_text_not_error() {
        let (sql, params) = compile("( machine:dev1 AND");
        assert!(sql.contains("lower(s.machine_id) = lower($1)"));
        assert_eq!(params, vec!["dev1".to_string()]);
        let (sql, _) = compile("unknown:field:mess ((");
        assert!(sql.contains("ILIKE $1"));
    }

    #[test]
    fn compile_empty_is_true() {
        let (sql, params) = compile("");
        assert_eq!(sql, "TRUE");
        assert!(params.is_empty());
    }

    #[test]
    fn compile_dir_and_effort() {
        let (sql, params) = compile("dir:cctui effort:high");
        assert!(sql.contains("s.working_dir ILIKE $1"));
        assert!(sql.contains("lower(COALESCE(s.effort, '')) = lower($2)"));
        assert_eq!(params, vec!["%cctui%".to_string(), "high".to_string()]);
    }

    fn attention_from_signals(
        tempo: Option<&str>,
        agent_state: Option<&str>,
        activity: Option<&str>,
    ) -> Option<Attention> {
        attention_from_bucket(bucket_from_signals(
            tempo,
            agent_state,
            activity,
            None,
            &[],
            &std::collections::HashMap::new(),
        ))
    }

    #[test]
    fn liveness_active_within_active_window() {
        let hb = Utc::now() - Duration::seconds(60);
        assert_eq!(derive_liveness(hb), Liveness::Active);
    }

    #[test]
    fn liveness_stale_between_windows() {
        let hb = Utc::now() - Duration::minutes(20);
        assert_eq!(derive_liveness(hb), Liveness::Stale);
    }

    #[test]
    fn liveness_dead_past_dead_window() {
        let hb = Utc::now() - Duration::hours(3);
        assert_eq!(derive_liveness(hb), Liveness::Dead);
    }

    #[test]
    fn attention_needs_input_when_tempo_blocked() {
        assert_eq!(
            attention_from_signals(Some("blocked"), Some("working"), None),
            Some(Attention::NeedsInput),
        );
    }

    #[test]
    fn no_attention_when_tempo_active() {
        assert_eq!(attention_from_signals(Some("active"), Some("working"), None), None);
    }

    #[test]
    fn no_attention_when_blocked_but_already_failed() {
        // activity=failure short-circuits to Done before the blocked check.
        assert_eq!(attention_from_signals(Some("blocked"), None, Some("failure")), None);
    }

    #[test]
    fn no_attention_when_no_signals() {
        assert_eq!(attention_from_signals(None, None, None), None);
    }

    fn bucket4(
        tempo: Option<&str>,
        agent_state: Option<&str>,
        activity: Option<&str>,
        soft_limit: Option<&str>,
    ) -> Bucket {
        bucket_from_signals(
            tempo,
            agent_state,
            activity,
            soft_limit,
            &[],
            &std::collections::HashMap::new(),
        )
    }

    #[test]
    fn bucket_reflects_signals() {
        use cctui_proto::classifier::Bucket;
        assert_eq!(bucket4(Some("blocked"), Some("working"), None, None), Bucket::Blocked);
        assert_eq!(bucket4(Some("active"), None, None, None), Bucket::Working);
        assert_eq!(bucket4(None, Some("stopped"), None, None), Bucket::Done);
        assert_eq!(bucket4(None, None, Some("success"), None), Bucket::Done);
        // a durable soft-limit block forces Blocked even over an
        // otherwise-Done/active session.
        assert_eq!(
            bucket4(Some("active"), None, Some("success"), Some("rate-limited")),
            Bucket::Blocked
        );
    }

    #[test]
    fn pr_children_open_review_lands_in_review_bucket() {
        use cctui_proto::classifier::{OwnedPrStatus, PrStatusCache};
        let children = [SessionChild {
            id: "1".into(),
            href: "https://github.com/o/r/pull/1".into(),
            kind: "pr".into(),
        }];
        let cache = PrStatusCache::new();
        cache.upsert(
            "https://github.com/o/r/pull/1",
            OwnedPrStatus {
                state: "OPEN".into(),
                checks_passed: 3,
                checks_failed: 1,
                checks_pending: 0,
                review: "REVIEW_REQUIRED".into(),
            },
        );
        let snap = cache.snapshot();
        let map = PrStatusCache::borrow_map(&snap);
        assert_eq!(
            bucket_from_signals(None, Some("working"), None, None, &children, &map),
            Bucket::Review
        );
        cache.remove("https://github.com/o/r/pull/1");
        let snap2 = cache.snapshot();
        let map2 = PrStatusCache::borrow_map(&snap2);
        assert_eq!(
            bucket_from_signals(None, Some("working"), None, None, &children, &map2),
            Bucket::Working
        );
    }

    #[test]
    fn collapses_newlines_and_runs_of_whitespace() {
        let raw = "line one\n\nline two\t\t  line three";
        assert_eq!(normalize_last_message(raw), "line one line two line three");
    }

    #[test]
    fn truncates_with_ellipsis_past_max() {
        let raw = "a".repeat(250);
        let out = normalize_last_message(&raw);
        // 200 chars + the ellipsis.
        assert_eq!(out.chars().count(), 201);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn does_not_ellipsis_short_text() {
        let raw = "short message";
        assert_eq!(normalize_last_message(raw), "short message");
    }

    #[test]
    fn handles_cjk_and_emoji_by_char_count_not_bytes() {
        // 200 of these = 200 chars, well under the cap.
        let raw = "漢".repeat(200);
        let out = normalize_last_message(&raw);
        assert_eq!(out.chars().count(), 200);
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn truncates_cjk_at_char_boundary_not_byte() {
        // 250 CJK chars (3 bytes each in UTF-8 = 750 bytes).
        let raw = "漢".repeat(250);
        let out = normalize_last_message(&raw);
        assert_eq!(out.chars().count(), 201);
        assert!(out.ends_with('…'));
    }

    /// The pre-CCT-1006 `make_snippet`, kept verbatim as the oracle: the
    /// optimisation may not change a single byte of user-visible snippet.
    fn make_snippet_reference(text: &str, needles: &[String]) -> String {
        const WINDOW: usize = 200;
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let hay = collapsed.to_lowercase();
        let chars: Vec<char> = collapsed.chars().collect();
        let match_byte =
            needles.iter().filter_map(|n| hay.find(&n.to_lowercase())).min().unwrap_or(0);
        let match_char = collapsed[..match_byte].chars().count();
        let start = match_char.saturating_sub(WINDOW / 2);
        let end = (start + WINDOW).min(chars.len());
        let mut out = String::new();
        if start > 0 {
            out.push('\u{2026}');
        }
        out.extend(&chars[start..end]);
        if end < chars.len() {
            out.push('\u{2026}');
        }
        out
    }

    #[test]
    fn snippet_output_is_byte_identical_to_the_reference() {
        let long = "lorem ipsum dolor sit amet ".repeat(60);
        let unicode =
            "\u{3053}\u{3093}\u{306b}\u{3061}\u{306f} caf\u{e9} na\u{ef}ve \u{1f980} crab "
                .repeat(40);
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("", vec!["x".into()]),
            ("short text", vec![]),
            ("short text", vec!["nomatch".into()]),
            ("the quick brown fox", vec!["brown".into()]),
            ("The Quick BROWN Fox", vec!["brown".into()]),
            ("  collapse   all \n\t whitespace  runs ", vec!["all".into()]),
            (long.as_str(), vec!["dolor".into()]),
            (long.as_str(), vec!["nomatch".into()]),
            (unicode.as_str(), vec!["crab".into()]),
            (unicode.as_str(), vec!["caf\u{e9}".into()]),
            (unicode.as_str(), vec!["\u{1f980}".into()]),
            // Multi-needle: the window centres on the earliest hit, not the first listed.
            (long.as_str(), vec!["amet".into(), "lorem".into()]),
            ("exactly at the boundary", vec!["boundary".into()]),
        ];
        for (text, needles) in cases {
            assert_eq!(
                make_snippet(text, &needles),
                make_snippet_reference(text, &needles),
                "snippet drifted for {text:?} / {needles:?}"
            );
        }
    }

    #[test]
    fn snippet_never_exceeds_the_window() {
        let long = "abcdefghij ".repeat(200);
        let out = make_snippet(&long, &["abcdefghij".to_string()]);
        // 200 chars plus at most one ellipsis on each side.
        assert!(out.chars().count() <= 202, "{}", out.chars().count());
    }

    #[test]
    fn snippet_sql_returns_one_row_per_session() {
        let sql = snippet_sql(1);
        assert!(sql.contains("CROSS JOIN LATERAL"), "{sql}");
        assert!(sql.contains("ORDER BY e.created_at DESC LIMIT 1"), "{sql}");
        assert!(sql.contains("unnest($1::text[])"), "{sql}");
        // A per-session LATERAL replaces the whole-table DISTINCT ON: bringing
        // that back re-scans every matching event of every session.
        assert!(!sql.contains("DISTINCT ON"), "{sql}");
    }

    #[test]
    fn snippet_sql_binds_every_pattern_and_keeps_the_indexed_cap() {
        let sql = snippet_sql(3);
        for i in 2..=4 {
            assert!(sql.contains(&format!("left(e.search_text, 8192) ILIKE ${i}")), "{sql}");
        }
        assert!(!sql.contains("$5"), "{sql}");
    }

    #[test]
    fn free_text_exists_keeps_the_correlation_barrier() {
        let (sql, _) = compile("hello");
        // `(SELECT s.id)` is what stops the planner hoisting the sublink into a
        // hashed subplan that rechecks every matching event in the table.
        assert!(sql.contains("e.session_id = (SELECT s.id)"), "{sql}");
    }
}
