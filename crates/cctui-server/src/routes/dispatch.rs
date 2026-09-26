//! `POST /api/v1/sessions/dispatch`.
//!
//! Routes a [`DispatchRequest`] to the named [`Dispatcher`] and returns the
//! handle. It does NOT create a session row — the worker pod's `cctui-daemon`
//! registers the real session directly under the shared `dispatch` machine,
//! so a pre-minted placeholder can't strand alongside it.
//!
//! Auth: any authenticated caller. A user-scoped token also gets the caller's
//! stable `dispatch` machine key injected into the forwarded payload so the
//! worker runs as that one machine.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};

use cctui_proto::api::{ApiError, DispatchRequest, DispatchResponse};

use crate::auth::{AuthContext, machine_token, mint_secret, sha256_hex};
use crate::dispatchers::enrolled::EnrolledDispatcher;
use crate::dispatchers::{DispatchError, DispatchSpec, Dispatcher};
use crate::ntfy::{self, Notification};
use crate::state::AppState;

/// Resolve a user's display name for notifications. Falls back to the raw uuid
/// (or `anonymous` for tokenless callers) so a notification always identifies
/// the caller even if the lookup misses.
async fn caller_label(state: &AppState, user_id: Option<uuid::Uuid>) -> String {
    let Some(uid) = user_id else {
        return "anonymous (admin token)".into();
    };
    sqlx::query_scalar::<_, String>("SELECT name FROM users WHERE id = $1")
        .bind(uid)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map_or_else(|| uid.to_string(), |name| format!("{name} ({uid})"))
}

/// Best-effort one-line summary of what's being run, pulled from the opaque
/// payload. The dispatch payload carries no literal prompt — the actual prompt
/// lives in a named template (`prompt_file`) resolved worker-side — so we
/// surface the keys that identify the run: flow, prompt file, model, etc. The
/// full payload is shown separately; this is just a glanceable header. Returns
/// `(no recognized fields)` when none of the known keys are present.
fn summarize(payload: &serde_json::Value) -> String {
    let mut parts = Vec::new();
    for key in ["flow", "prompt_file", "model", "effort", "repo"] {
        if let Some(s) = payload.get(key).and_then(|v| v.as_str()) {
            parts.push(format!("{key}={s}"));
        }
    }
    // Common nested location for the target project/repo context.
    if let Some(proj) = payload.pointer("/context/project").and_then(|v| v.as_str()) {
        parts.push(format!("project={proj}"));
    }
    if parts.is_empty() { "(no recognized fields)".into() } else { parts.join("  ") }
}

/// Render the payload for a notification with secrets redacted — the injected
/// machine key (a bearer secret) and any `env` map (user-supplied environment
/// secrets) — and truncated so a huge payload doesn't blow up the push
/// body.
fn payload_for_notify(payload: &serde_json::Value) -> String {
    const MAX: usize = 1500;
    let mut p = payload.clone();
    if let Some(obj) = p.as_object_mut() {
        if obj.contains_key("cctui_machine_key") {
            obj.insert("cctui_machine_key".into(), serde_json::Value::String("<redacted>".into()));
        }
        // `payload.env` carries environment secrets for the k8s worker (the
        // external dispatcher turns them into pod env / an ephemeral Secret).
        // Redact the values — they must never land in a notification.
        if let Some(env) = obj.get("env").and_then(serde_json::Value::as_object) {
            let redacted: serde_json::Map<String, serde_json::Value> = env
                .keys()
                .map(|k| (k.clone(), serde_json::Value::String("<redacted>".into())))
                .collect();
            obj.insert("env".into(), serde_json::Value::Object(redacted));
        }
    }
    let mut s = serde_json::to_string_pretty(&p).unwrap_or_else(|_| p.to_string());
    if s.len() > MAX {
        s.truncate(MAX);
        s.push_str("\n… (truncated)");
    }
    s
}

/// Lazily fetch (or create) the caller's single persistent "dispatch" machine
/// and return its `(machine_id, machine_key)`.
///
/// Every dispatched worker pod runs a `cctui-daemon` that authenticates with
/// THIS one key, so all dispatched sessions register under one stable machine
/// — no per-pod enroll/deenroll churn and no `dispatch:<origin>` placeholder.
/// The key is stored plaintext (`machines.dispatch_key`) because the server
/// must hand it to pods verbatim; it ends up in pod env regardless, so the DB
/// is not a meaningfully weaker home for it. Reused across concurrent pods:
/// they share the machine row and key, and are told apart by `session_id`.
async fn ensure_dispatch_machine(
    state: &AppState,
    user_id: uuid::Uuid,
) -> anyhow::Result<(uuid::Uuid, String)> {
    if let Some((id, key)) = sqlx::query_as::<_, (uuid::Uuid, Option<String>)>(
        "SELECT id, dispatch_key FROM machines \
         WHERE user_id = $1 AND kind = 'dispatch' AND deleted_at IS NULL \
         ORDER BY first_seen_at LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    {
        if let Some(key) = key {
            return Ok((id, key));
        }
        // A dispatch machine exists but predates `dispatch_key` (or it was
        // cleared): rotate a fresh key into it rather than orphaning the row.
        let secret = mint_secret();
        let token = machine_token(&secret);
        let key_hash = sha256_hex(&token);
        sqlx::query("UPDATE machines SET dispatch_key = $2, key_hash = $3 WHERE id = $1")
            .bind(id)
            .bind(&token)
            .bind(&key_hash)
            .execute(&state.pool)
            .await?;
        return Ok((id, token));
    }

    let machine_id = uuid::Uuid::new_v4();
    let secret = mint_secret();
    let token = machine_token(&secret);
    let key_hash = sha256_hex(&token);
    sqlx::query(
        "INSERT INTO machines (id, user_id, name, key_hash, kind, dispatch_key) \
         VALUES ($1, $2, 'dispatch', $3, 'dispatch', $4)",
    )
    .bind(machine_id)
    .bind(user_id)
    .bind(&key_hash)
    .bind(&token)
    .execute(&state.pool)
    .await?;
    tracing::info!(%user_id, %machine_id, "created dispatch machine");
    Ok((machine_id, token))
}

/// Mint a per-session EPHEMERAL machine credential for a dispatched worker,
/// bound to the pre-minted `session_id` and the user's shared
/// `dispatch` machine (`machine_id`), expiring at the session deadline + grace.
///
/// This replaces the shared per-user `dispatch_key` as the credential
/// handed to the worker pod: a leaked worker key now authenticates only its own
/// session and dies with it, instead of impersonating every dispatched session
/// of the user. It is an additive `auth_keys` row — the auth path
/// already enforces `expires_at` and carries `machine_id` transparently, so the
/// daemon accepts it exactly like any other machine key — with `kind`
/// `'ephemeral'` and `session_id` set so the reaper can revoke it on the
/// session's terminal state. Scopes mirror the owner's ceiling (same as the
/// shared dispatch key did via the legacy machine path). Returns the plaintext
/// token to inject as `CCTUI_MACHINE_KEY`.
async fn mint_ephemeral_dispatch_key(
    state: &AppState,
    user_id: uuid::Uuid,
    machine_id: uuid::Uuid,
    session_id: &str,
    timeout_minutes: Option<u32>,
) -> anyhow::Result<String> {
    // TTL = session deadline + grace. Fall back to a generous default when the
    // dispatch carries no timeout so the key still outlives a long run, then is
    // swept by the reaper. Grace covers post-deadline teardown/heartbeat.
    const GRACE_SECS: i64 = 30 * 60;
    const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;
    let secret = mint_secret();
    let token = machine_token(&secret);
    let key_hash = sha256_hex(&token);
    let lifetime = timeout_minutes
        .map_or(DEFAULT_TTL_SECS, |m| i64::from(m).saturating_mul(60))
        .saturating_add(GRACE_SECS);
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(lifetime);

    let scopes = crate::auth::ceiling_of(&state.pool, user_id).await;
    let key_id: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO auth_keys \
           (user_id, key_hash, key_preview, label, kind, machine_id, session_id, expires_at) \
         VALUES ($1, $2, $3, $4, 'ephemeral', $5, $6, $7) \
         ON CONFLICT (key_hash) DO UPDATE SET label = EXCLUDED.label \
         RETURNING id",
    )
    .bind(user_id)
    .bind(&key_hash)
    .bind(crate::auth::token_preview(&token))
    .bind(format!("dispatch session {session_id}"))
    .bind(machine_id)
    .bind(session_id)
    .bind(expires_at)
    .fetch_one(&state.pool)
    .await?;
    for scope in scopes {
        sqlx::query("INSERT INTO key_acls (key_id, scope) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(key_id.0)
            .bind(scope.as_str())
            .execute(&state.pool)
            .await?;
    }
    tracing::info!(%user_id, %machine_id, %session_id, "minted ephemeral dispatch key");
    Ok(token)
}

/// The provider family a pool election is scoped to. Dispatch carries no
/// adapter of its own: the entry's provider hint names the family when there is
/// one, otherwise the forwarded payload's adapter does, defaulting to
/// `claude-code` as the rest of the dispatch path does.
fn binding_family(hint: Option<&str>, payload_adapter: &str) -> crate::routes::gateway::Family {
    hint.map(str::trim).filter(|p| !p.is_empty()).map_or_else(
        || crate::routes::gateway::Family::from_adapter(payload_adapter),
        crate::routes::gateway::Family::from_provider,
    )
}

/// Map a shared resolver failure onto the dispatch error surface. Callers here
/// are machines: a rejection is a 400 whose detail names every account that was
/// considered and why it was skipped.
fn dispatch_resolve_err(e: crate::account_resolve::ResolveError) -> (StatusCode, Json<ApiError>) {
    match e {
        crate::account_resolve::ResolveError::Rejected(msg) => {
            (StatusCode::BAD_REQUEST, Json(ApiError { error: msg }))
        }
        crate::account_resolve::ResolveError::Db => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError { error: "could not provision account session".into() }),
        ),
    }
}

/// The account or pool name a dispatch should route through, after applying the
/// fallback precedence: an explicit `req.account` always wins; otherwise the
/// dispatcher's bound default account (if any) is used. The optional provider
/// hint constrains the mint to that provider's family; without one the
/// account's EVERY provider row is minted — the dispatcher default is
/// an identity, so it always mints all providers.
///
/// Pure so the precedence is unit-testable without a DB. `None` means "no
/// account at all" — dispatch then behaves as it did before any account
/// routing existed (no gateway env injected).
fn resolve_dispatch_account(
    explicit_account: Option<&str>,
    explicit_provider: Option<&str>,
    default_account: Option<&str>,
) -> Option<(String, Option<String>)> {
    if let Some(name) = explicit_account.map(str::trim).filter(|a| !a.is_empty()) {
        return Some((
            name.to_string(),
            explicit_provider.map(str::trim).filter(|p| !p.is_empty()).map(str::to_string),
        ));
    }
    default_account.map(|name| (name.to_owned(), None))
}

/// The first provider family that appears twice in the expanded mint set
/// — two same-family provider rows would mint the same env keys
/// (e.g. `ANTHROPIC_AUTH_TOKEN`) and the second mint would silently repoint the
/// session's family token, so the dispatch is rejected instead. Pure
/// for unit-testability.
fn colliding_family(
    families: impl IntoIterator<Item = crate::routes::gateway::Family>,
) -> Option<crate::routes::gateway::Family> {
    let mut seen: Vec<crate::routes::gateway::Family> = Vec::new();
    for f in families {
        if seen.contains(&f) {
            return Some(f);
        }
        seen.push(f);
    }
    None
}

/// Resolve the `(session_id, display_name, dedup_key)` for a dispatch.
///
/// `session_id` is the per-dispatch correlation id the worker registers under
/// and the gateway token binds to. It is ALWAYS a fresh UUID so isolated
/// short-lived pods never chain their logs into one growing conversation.
/// Idempotency rides `dedup_key`, which the dispatcher hashes into the Job name.
/// Claude's daemon derives `short = session_id[..8]` and rejects a dispatch whose
/// `short` isn't `/^[a-f0-9]{8}$/`, so a v4 UUID still satisfies the shape
/// constraint.
///
/// - `None` → fresh UUID, no display name, no dedup (each dispatch is unique).
/// - an already-valid UUID → used as-is (a deliberate retry/resume target) and
///   as its own dedup key, no display name.
/// - any other (human-readable) id → a FRESH UUID session, the original carried
///   as both the display name and the dedup key (so repeat dispatches of the
///   same logical key still coalesce onto one Job while each running round keeps
///   its own isolated session).
fn resolve_dispatch_session_id(logical: Option<&str>) -> (String, Option<String>, Option<String>) {
    match logical {
        None => (uuid::Uuid::new_v4().to_string(), None, None),
        Some(s) if uuid::Uuid::parse_str(s).is_ok() => (s.to_owned(), None, Some(s.to_owned())),
        Some(s) => (uuid::Uuid::new_v4().to_string(), Some(s.to_owned()), Some(s.to_owned())),
    }
}

/// Rewrite `payload.model` to `mapped` iff it differs from `raw`, returning
/// whether a rewrite happened. The dispatch model-alias decision,
/// factored out of the async per-account resolution loop so it's unit-testable
/// without a DB: an unchanged mapping (an alias miss, since `resolve_account_model`
/// fails soft to its input) is a no-op that leaves `model` — and every other
/// key, e.g. `effort` — verbatim.
fn rewrite_model_if_aliased(payload: &mut serde_json::Value, raw: &str, mapped: &str) -> bool {
    if mapped == raw {
        return false;
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".into(), serde_json::Value::String(mapped.to_owned()));
        return true;
    }
    false
}

/// What a dispatcher routes an account-less dispatch through.
#[derive(Debug, PartialEq, Eq)]
enum DefaultBinding {
    /// One account identity, by the *name* `mint` resolution consumes —
    /// default injection mints ALL of that identity's providers, so no
    /// provider hint travels with it.
    Account(String),
    /// A pool, by id: elected per dispatch, and no account name can shadow it.
    Pool(uuid::Uuid),
}

/// An account binding is the more specific instruction, so it wins when a
/// dispatcher carries both. Pure so the precedence is unit-testable.
fn default_binding(account: Option<String>, pool_id: Option<uuid::Uuid>) -> Option<DefaultBinding> {
    account.map(DefaultBinding::Account).or_else(|| pool_id.map(DefaultBinding::Pool))
}

/// The identity a dispatcher is bound to for dispatches that name no account.
///
/// `None` when the row carries neither binding, or when the one it carries
/// points at a deleted row (the `ON DELETE SET NULL` FKs clear it). A DB error
/// degrades to `None` so a lookup hiccup never blocks an otherwise-valid
/// dispatch.
async fn dispatcher_default_binding(
    state: &AppState,
    dispatcher_name: &str,
    user_id: uuid::Uuid,
) -> Option<DefaultBinding> {
    let row: Option<(Option<String>, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT a.name, d.default_pool_id \
         FROM dispatchers d \
         LEFT JOIN accounts a ON a.id = d.default_account_id \
         WHERE d.name = $1 AND d.user_id = $2 \
           AND d.deleted_at IS NULL AND d.revoked_at IS NULL \
         ORDER BY d.created_at LIMIT 1",
    )
    .bind(dispatcher_name)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    row.and_then(|(account, pool_id)| default_binding(account, pool_id))
}

/// Resolve a dispatcher *name* for the caller: an enrolled dispatcher
/// takes precedence, falling back to the global env-configured http registry.
/// Returns `Ok(None)` to mean "no such name anywhere" so the caller can 404
/// distinctly from a permission denial. An enrolled dispatcher resolves to an
/// [`EnrolledDispatcher`] that sends Dispatch commands over the WS hub.
///
/// Ownership scoping mirrors machines & connectors: a user token sees
/// only its own enrolled dispatchers (`user_id = caller`); the admin token
/// (`user_id` is `None` — the only authenticated role without a user) gets the
/// same god-view it has elsewhere and resolves by name across ALL owners. Names
/// are unique per `(user_id, name)` but not globally, so the admin path takes a
/// deterministic `ORDER BY created_at LIMIT 1` — acceptable until dispatchers
/// are addressable by id; if two users enroll the same name, admin hits the
/// oldest.
pub async fn resolve_dispatcher(
    state: &AppState,
    user_id: Option<uuid::Uuid>,
    name: &str,
) -> Result<Option<std::sync::Arc<dyn Dispatcher>>, DispatchError> {
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT id FROM dispatchers \
         WHERE ($1::uuid IS NULL OR user_id = $1) AND name = $2 \
         AND deleted_at IS NULL AND revoked_at IS NULL \
         ORDER BY created_at LIMIT 1",
    )
    .bind(user_id)
    .bind(name)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| DispatchError::Backend(format!("dispatcher lookup: {e}")))?;
    if let Some((dispatcher_id,)) = row {
        return Ok(Some(std::sync::Arc::new(EnrolledDispatcher::new(
            name,
            dispatcher_id,
            state.clone(),
        ))));
    }
    // Fall back to a global env-configured http dispatcher (escape hatch).
    match state.dispatchers.get(name) {
        Ok(d) => Ok(Some(d)),
        Err(DispatchError::UnknownDispatcher(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `GET /api/v1/sessions/dispatchers` — the names of every dispatcher the
/// caller can target: their enrolled dispatchers merged with the
/// global env-configured registry. The web UI uses this to populate the
/// dispatch picker. Any authenticated caller may read it (no role gate,
/// matching dispatch itself — for per-user gating).
///
/// Ownership scoping matches [`resolve_dispatcher`]: a user sees its
/// own enrolled dispatchers; the admin token (`user_id` is `None`) sees ALL of
/// them, so the picker offers everything an admin can actually dispatch to.
pub async fn list_dispatchers(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Json<Vec<String>> {
    let mut ids = state.dispatchers.ids();
    if let Ok(names) = sqlx::query_scalar::<_, String>(
        "SELECT name FROM dispatchers \
         WHERE ($1::uuid IS NULL OR user_id = $1) AND deleted_at IS NULL",
    )
    .bind(ctx.owner_filter())
    .fetch_all(&state.pool)
    .await
    {
        ids.extend(names);
    }
    ids.sort();
    ids.dedup();
    Json(ids)
}

// Linear dispatch pipeline (auth → resolve account/dispatcher → mint key → resolve
// session id → forward); complexity is the breadth of validation/error branches,
// not nesting. Splitting risks the 522 session-id/dedup invariants.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub async fn dispatch(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<DispatchRequest>,
) -> Result<(StatusCode, Json<DispatchResponse>), (StatusCode, Json<ApiError>)> {
    let dispatcher = match resolve_dispatcher(&state, ctx.owner_filter(), &req.dispatcher).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            let known = state.dispatchers.ids().join(", ");
            tracing::warn!(
                "dispatch rejected: unknown dispatcher {} (known: {known})",
                req.dispatcher
            );
            return Err((
                StatusCode::NOT_FOUND,
                Json(ApiError {
                    error: format!("unknown dispatcher: {}. known: [{known}]", req.dispatcher),
                }),
            ));
        }
        Err(e) => {
            tracing::error!("dispatcher resolution failed: {e}");
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(ApiError { error: format!("dispatcher unavailable: {e}") }),
            ));
        }
    };

    // Cross-replica delivery: if a live PEER replica holds this
    // dispatcher's WS, the bus's peer transport forwards the Dispatch frame to
    // it inside `EnrolledDispatcher::dispatch` — side effects (ntfy, ephemeral
    // key mint, webhook registration) run here exactly once either way.

    // Claude's daemon derives `short = session_id[..8]` and rejects a dispatch
    // unless `short` matches /^[a-f0-9]{8}$/ — so the worker's session id must be
    // UUID-shaped. Callers may pass a human-readable logical id (e.g.
    // an automation dedup key like `triage-PROJ-2026…`); we now mint a FRESH UUID
    // session for it and carry the original as both the display name and the
    // `dedup_key`. The dispatcher hashes `dedup_key` into the Job name,
    // so a duplicate webhook still coalesces while each round keeps an isolated
    // session — the server no longer chains every round's logs onto one id.
    let (session_id, display_name, dedup_key) =
        resolve_dispatch_session_id(req.session_id.as_deref());
    let origin = dispatcher.id();

    // Alert that a dispatch arrived. Built from the *original* payload
    // (before the machine key is injected) and no-ops unless ntfy is configured.
    let caller = caller_label(&state, ctx.owner_filter()).await;
    let summary = summarize(&req.payload);
    ntfy::notify(
        &state.config,
        Notification {
            title: format!("Dispatch received → {}", req.dispatcher),
            message: format!(
                "user: {caller}\nsession: {session_id}\ndispatcher: {}\n\n{summary}\n\npayload:\n{}",
                req.dispatcher,
                payload_for_notify(&req.payload),
            ),
            tags: "inbox_tray".into(),
            priority: 3,
        },
    );

    // We do NOT pre-create a session row: the worker's cctui-daemon
    // self-dispatches the real session on boot and registers it
    // directly under the shared `dispatch` machine — forcing this pre-minted
    // (UUID-shaped) `session_id` so the registered id matches the id
    // the gateway token is bound to. A pre-minted row would just linger as an
    // empty `dispatch:<origin>` placeholder alongside it. Double dispatch is
    // still idempotent: the dispatcher derives the k8s Job name from
    // `sha(dedup_key)` (the caller's logical key), so a repeat of the
    // same key maps to the same Job (409 → same handle) even though each
    // dispatch now mints a fresh `session_id`.

    // Resolve the caller's stable dispatch machine and forward its key to the
    // pod via a reserved payload key. The dispatcher lifts it into
    // `CCTUI_MACHINE_KEY` and keeps it OUT of TASK_PAYLOAD_JSON, so the worker's
    // daemon runs AS this one machine without a per-pod enroll. The web UI and
    // automation dispatch with a user token (user_id present); the admin token (no
    // owning user) dispatches without the shared identity.
    // Dispatch permission is now the `dispatch` scope, enforced
    // uniformly for every caller. The migration backfilled `dispatch` into
    // user_acls only where the legacy `can_dispatch` flag was set, so this is
    // transparent: a user previously toggled off has no `dispatch` scope and is
    // still denied. Admin holds the scope by ceiling.
    ctx.requires(crate::auth::Scope::Dispatch).map_err(|s| {
        tracing::warn!(uid = %ctx.user_id, "dispatch denied: caller lacks dispatch scope");
        (s, Json(ApiError { error: "dispatch is not permitted for this token".into() }))
    })?;

    // Gated on `owner_filter()` to match registration below: an admin-token
    // dispatch has no owning user and never registers a webhook.
    if let (Some(_), Some(notify_url)) =
        (ctx.owner_filter(), req.notify_url.as_deref().filter(|u| !u.trim().is_empty()))
    {
        crate::webhook::validate_notify_url(notify_url).await.map_err(|e| {
            tracing::warn!(uid = %ctx.user_id, "dispatch rejected: unsafe notify_url ({e})");
            (StatusCode::BAD_REQUEST, Json(ApiError { error: format!("invalid notify_url: {e}") }))
        })?;
    }

    let mut forwarded_payload = req.payload.clone();
    // Carry the caller's logical id as the session display name (the session id
    // itself is now a derived UUID) so the UI still shows e.g.
    // `triage-PROJ-2026…`. Only when the caller didn't already name the session.
    if let Some(name) = &display_name
        && let Some(obj) = forwarded_payload.as_object_mut()
    {
        obj.entry("name").or_insert_with(|| serde_json::Value::String(name.clone()));
    }
    // The shared dispatch-machine identity + account routing key on the
    // AUTHENTICATED USER — `ctx.user_id`, NOT `owner_filter()`.
    // `owner_filter()` is a query-result scoping switch (it returns `None` for
    // admins so list views see every row) and has nothing to do with who owns a
    // dispatched session. Keying on it meant an RBAC-admin user (a real user
    // with the Admin scope — e.g. the web UI operator) dispatched with NO shared
    // identity, so `CCTUI_MACHINE_KEY` was never injected and the worker
    // hard-exited. Only the env admin token has no real user (`user_id` is nil);
    // it still dispatches without the shared identity.
    if let Some(uid) = (ctx.user_id != uuid::Uuid::nil()).then_some(ctx.user_id) {
        // The shared `dispatch` machine still groups every dispatched session
        // under one logical machine (UI grouping unchanged) — but the
        // credential handed to the pod is now a PER-SESSION ephemeral key,
        // so a leaked worker key only impersonates its own session
        // and expires with it. `ensure_dispatch_machine` is kept for the row
        // (and `dispatch_key` rotation) it owns.
        let (machine_id, _shared_key) =
            ensure_dispatch_machine(&state, uid).await.map_err(|e| {
                tracing::error!("ensure_dispatch_machine failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError { error: "could not resolve dispatch machine".into() }),
                )
            })?;
        let key = mint_ephemeral_dispatch_key(&state, uid, machine_id, &session_id, req.timeout)
            .await
            .map_err(|e| {
                tracing::error!("mint_ephemeral_dispatch_key failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiError { error: "could not mint dispatch credential".into() }),
                )
            })?;
        if let Some(obj) = forwarded_payload.as_object_mut() {
            obj.insert("cctui_machine_key".into(), serde_json::Value::String(key));
        }

        // Account-scoped routing on the dispatch path: mint session-scoped
        // gateway tokens and merge the gateway base-url + token env into
        // `payload.env` so the worker pod routes through the passthrough
        // gateway. An explicit `req.accounts` list wins — it's the
        // cross-account mix form, each entry optionally family-constrained by
        // its provider hint. Otherwise the singular `req.account` (or the
        // dispatcher's bound default identity) is used. A bare account name mints EVERY provider the
        // identity carries — one worker gets claude + codex creds
        // from `account: "acme"` alone, no accounts[] boilerplate. With no
        // account either way, no gateway env is injected (unchanged).
        let requested_model = req
            .payload
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let payload_adapter = req
            .payload
            .get("adapter_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("claude-code")
            .to_owned();
        let mut bound_pool: Option<uuid::Uuid> = None;
        let accounts: Vec<(String, Option<String>)> = if !req.accounts.is_empty() {
            req.accounts.iter().map(|a| (a.account.clone(), a.provider.clone())).collect()
        } else if let Some(explicit) =
            resolve_dispatch_account(req.account.as_deref(), req.provider.as_deref(), None)
        {
            vec![explicit]
        } else {
            // Nothing named: the dispatcher's own binding decides. A bound pool
            // elects here, per dispatch — the whole point of binding one.
            match dispatcher_default_binding(&state, &req.dispatcher, uid).await {
                Some(DefaultBinding::Account(name)) => vec![(name, None)],
                Some(DefaultBinding::Pool(pool_id)) => {
                    let (account, pool_id) = crate::account_resolve::resolve_pool_by_id(
                        &state,
                        uid,
                        binding_family(None, &payload_adapter),
                        requested_model.as_deref(),
                        pool_id,
                    )
                    .await
                    .map_err(dispatch_resolve_err)?;
                    bound_pool = Some(pool_id);
                    vec![(account, None)]
                }
                None => Vec::new(),
            }
        };

        // Every name accepted here may be a pool: elect a member now, by the
        // pool's own strategy, and keep the pool so the minted token can be
        // stamped with it. An account of the caller's answering to the same
        // name always wins.
        let mut resolved: Vec<(String, Option<String>)> = Vec::with_capacity(accounts.len());
        for (name, hint) in accounts {
            let family = binding_family(hint.as_deref(), &payload_adapter);
            let bound = crate::account_resolve::resolve_account_or_pool(
                &state,
                uid,
                family,
                requested_model.as_deref(),
                &name,
            )
            .await
            .map_err(dispatch_resolve_err)?;
            if bound.pool_id.is_some() {
                // One column, one pool: the first pool named owns the session's
                // failover boundary. A second pool in the same cross-family list
                // still elects, it just cannot also claim the stamp.
                bound_pool = bound_pool.or(bound.pool_id);
            }
            resolved.push((bound.account, hint));
        }
        let accounts = resolved;

        // Map `payload.model` through the resolved account(s) `model_aliases`,
        // mirroring the spawn path. Try Anthropic then Openai per
        // account; first rewrite wins. `resolve_account_model` fails soft
        // (returns input unchanged on any miss), so a non-alias model or an
        // unresolved account passes through untouched; `effort` is never touched.
        if let Some(raw_model) = forwarded_payload
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
        {
            'resolve: for (account_name, _hint) in &accounts {
                for family in [
                    crate::routes::gateway::Family::Anthropic,
                    crate::routes::gateway::Family::Openai,
                    crate::routes::gateway::Family::Fireworks,
                ] {
                    let mapped = crate::routes::gateway::resolve_account_model(
                        &state,
                        uid,
                        account_name,
                        family,
                        &raw_model,
                    )
                    .await;
                    if rewrite_model_if_aliased(&mut forwarded_payload, &raw_model, &mapped) {
                        break 'resolve;
                    }
                }
            }
        }

        // Expand each named account into the provider rows to mint: the hinted
        // family's row only, or every row for a bare name.
        let mut mints: Vec<crate::routes::gateway::ProviderRow> = Vec::new();
        for (account_name, provider_hint) in accounts {
            let rows =
                match crate::routes::gateway::account_provider_rows(&state, uid, &account_name)
                    .await
                {
                    Ok(Some(rows)) if !rows.is_empty() => rows,
                    Ok(_) => {
                        return Err((
                            StatusCode::NOT_FOUND,
                            Json(ApiError {
                                error: format!(
                                    "no account named {account_name:?} with a connected provider"
                                ),
                            }),
                        ));
                    }
                    Err(e) => {
                        tracing::error!("resolving dispatch account {account_name:?}: {e}");
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ApiError { error: "could not provision account session".into() }),
                        ));
                    }
                };
            match provider_hint.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                Some(p) => {
                    let family = crate::routes::gateway::Family::from_provider(p);
                    let before = mints.len();
                    mints.extend(rows.into_iter().filter(|r| r.family() == family));
                    if mints.len() == before {
                        return Err((
                            StatusCode::NOT_FOUND,
                            Json(ApiError {
                                error: format!(
                                    "account {account_name:?} has no {} provider",
                                    family.label()
                                ),
                            }),
                        ));
                    }
                }
                None => mints.extend(rows),
            }
        }

        // Two same-family provider rows would mint the same env keys (e.g.
        // ANTHROPIC_AUTH_TOKEN) and silently repoint the session's family
        // token; reject rather than clobber.
        if let Some(family) =
            colliding_family(mints.iter().map(crate::routes::gateway::ProviderRow::family))
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!(
                        "multiple dispatch accounts resolve to the {} provider family; \
                         specify at most one account per family",
                        family.label()
                    ),
                }),
            ));
        }

        for row in mints {
            match crate::routes::gateway::mint_session_env_for_account(&state, row.id, &session_id)
                .await
            {
                Ok(Some(gateway_env)) => {
                    if let Some(obj) = forwarded_payload.as_object_mut() {
                        let env = obj
                            .entry("env")
                            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                        if let Some(env_obj) = env.as_object_mut() {
                            for (k, v) in gateway_env {
                                env_obj.insert(k, serde_json::Value::String(v));
                            }
                        }
                    }
                }
                Ok(None) => {
                    tracing::error!(
                        provider_id = %row.id,
                        "mint_session_env_for_account (dispatch): provider row vanished mid-dispatch"
                    );
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiError { error: "could not provision account session".into() }),
                    ));
                }
                Err(e) => {
                    tracing::error!(provider_id = %row.id, "mint_session_env_for_account (dispatch) failed: {e}");
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(ApiError { error: "could not provision account session".into() }),
                    ));
                }
            }
        }

        if let Some(pool_id) = bound_pool {
            crate::account_resolve::stamp_pool(&state, &session_id, pool_id).await;
        }
    }

    // Register a server-emitted completion webhook when the caller
    // supplied `notify_url`. The server fires it once the dispatched session
    // reaches a terminal state — covering crash cases the worker's REPLY_URL
    // exit trap can miss. Scoped to a real owning user (admin-token dispatches
    // carry no owner, so they keep the REPLY_URL trap only). Best-effort: a
    // registration failure never blocks the dispatch. The `task_id` echoed back
    // is the dispatch payload's `task_id` if present, else the session id.
    if let (Some(uid), Some(notify_url)) =
        (ctx.owner_filter(), req.notify_url.as_deref().filter(|u| !u.trim().is_empty()))
    {
        let task_id =
            req.payload.get("task_id").and_then(serde_json::Value::as_str).unwrap_or(&session_id);
        crate::webhook::register(
            &state,
            &session_id,
            uid,
            notify_url,
            req.notify_secret.as_deref().filter(|s| !s.trim().is_empty()),
            task_id,
        )
        .await;
    }

    // `payload.spawn_capability` declares what the dispatched worker may spawn
    // through `CctuiAgent`. It is read here, server-side, and never forwarded —
    // the worker must not be able to read or restate its own capability.
    {
        let declared = forwarded_payload
            .as_object_mut()
            .and_then(|obj| obj.remove("spawn_capability"))
            .and_then(|raw| serde_json::from_value::<cctui_proto::api::SpawnCapability>(raw).ok())
            .filter(|cap| !cap.is_empty());
        let cap = declared.unwrap_or_else(cctui_proto::api::SpawnCapability::machine_default);
        if let Err(e) =
            crate::store::spawn_capabilities::upsert(&state.pool, &session_id, &cap).await
        {
            tracing::error!(
                session = %session_id,
                error = %e,
                "spawn-capability persist failed — CctuiAgent will be lost on server restart"
            );
        }
        state.spawn_capabilities.insert(session_id.clone(), cap);
    }

    let spec = DispatchSpec {
        session_id: &session_id,
        timeout_minutes: req.timeout,
        reply_url: req.reply_url.as_deref(),
        dedup_key: dedup_key.as_deref(),
        payload: &forwarded_payload,
    };

    let handle = match dispatcher.dispatch(&spec).await {
        Ok(h) => {
            ntfy::notify(
                &state.config,
                Notification {
                    title: format!("Dispatch forwarded → {}", req.dispatcher),
                    message: format!(
                        "user: {caller}\nsession: {session_id}\nhandle: {}\nnamespace: {}\nstatus: {}",
                        h.handle,
                        h.namespace.as_deref().unwrap_or("-"),
                        h.status.as_deref().unwrap_or("dispatched"),
                    ),
                    tags: "white_check_mark".into(),
                    priority: 3,
                },
            );
            // Persist the opaque handle so the completion-webhook sweep can ask
            // this dispatcher whether the workload later died without a
            // conclusion. Owner-scoped re-resolution in the sweep uses
            // the dispatcher *name* + the session's owning user, so only the
            // name/handle/namespace are stored. Best-effort: a failure here must
            // never block an otherwise-valid dispatch.
            let store = sqlx::query(
                "INSERT INTO dispatch_handles (session_id, dispatcher_name, handle, namespace) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                   dispatcher_name = EXCLUDED.dispatcher_name, \
                   handle = EXCLUDED.handle, \
                   namespace = EXCLUDED.namespace, \
                   created_at = now()",
            )
            .bind(&session_id)
            .bind(origin)
            .bind(&h.handle)
            .bind(h.namespace.as_deref())
            .execute(&state.pool)
            .await;
            if let Err(e) = store {
                tracing::warn!(%session_id, "failed to persist dispatch handle: {e}");
            }
            h
        }
        Err(e) => {
            tracing::error!(
                dispatcher = %req.dispatcher,
                session = %session_id,
                error = %e,
                "dispatch not placed — no worker exists for this session; any claim the \
                 caller took (tag/assignee) is now stale"
            );
            ntfy::notify(
                &state.config,
                Notification {
                    title: format!("Dispatch FAILED → {}", req.dispatcher),
                    message: format!("user: {caller}\nsession: {session_id}\nerror: {e}"),
                    tags: "rotating_light".into(),
                    priority: 5,
                },
            );
            let (code, msg) = match &e {
                DispatchError::InvalidIntent(_) => (StatusCode::BAD_REQUEST, e.to_string()),
                DispatchError::UnknownDispatcher(_) => (StatusCode::NOT_FOUND, e.to_string()),
                DispatchError::Backend(_) => (StatusCode::BAD_GATEWAY, e.to_string()),
                DispatchError::Unsupported(_) => (StatusCode::NOT_IMPLEMENTED, e.to_string()),
            };
            return Err((code, Json(ApiError { error: msg })));
        }
    };

    Ok((
        StatusCode::ACCEPTED,
        Json(DispatchResponse {
            session_id,
            dispatcher: origin.to_string(),
            handle: handle.handle,
            namespace: handle.namespace,
            // Surface the dispatcher's outcome verbatim so a re-dispatch onto a
            // terminal Job reads as `redispatched` (a fresh run) rather than a
            // misleading `dispatched`. Older dispatchers omit it →
            // preserve the historical `dispatched`.
            status: handle.status.unwrap_or_else(|| "dispatched".into()),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        DefaultBinding, binding_family, colliding_family, default_binding,
        resolve_dispatch_account, resolve_dispatch_session_id, rewrite_model_if_aliased,
    };
    use crate::routes::gateway::Family;

    #[test]
    fn session_id_human_logical_mints_fresh_uuid_keeps_key_as_dedup_and_name() {
        // each dispatch of the same logical key gets a DISTINCT session
        // (no log chaining), while the logical key is preserved as both the
        // display name and the dedup key (idempotency now lives there).
        let (a, na, ka) = resolve_dispatch_session_id(Some("triage-PROJ-202606231511"));
        let (b, nb, kb) = resolve_dispatch_session_id(Some("triage-PROJ-202606231511"));
        assert_ne!(a, b, "fresh, distinct session per dispatch");
        assert!(uuid::Uuid::parse_str(&a).is_ok(), "UUID-shaped: {a}");
        assert!(a[..8].chars().all(|c| c.is_ascii_hexdigit()), "hex short");
        assert_eq!(na.as_deref(), Some("triage-PROJ-202606231511"), "logical id kept as name");
        assert_eq!(nb, na, "display name is the logical key both times");
        assert_eq!(ka.as_deref(), Some("triage-PROJ-202606231511"), "logical id is the dedup key");
        assert_eq!(kb, ka, "dedup key is stable across dispatches (Job coalesces)");
    }

    #[test]
    fn session_id_real_uuid_passes_through_as_its_own_dedup_key() {
        let u = "a1b2c3d4-0000-4000-8000-000000000000";
        let (id, name, dedup) = resolve_dispatch_session_id(Some(u));
        assert_eq!(id, u);
        assert!(name.is_none());
        assert_eq!(dedup.as_deref(), Some(u), "explicit uuid is its own dedup target");
    }

    #[test]
    fn session_id_none_mints_fresh_uuid_no_dedup() {
        let (id, name, dedup) = resolve_dispatch_session_id(None);
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert!(name.is_none());
        assert!(dedup.is_none(), "no logical key ⇒ no dedup, each dispatch unique");
    }

    #[test]
    fn explicit_account_is_used_verbatim() {
        // An explicit `req.account` routes through that account regardless of any
        // dispatcher binding (the override case).
        let got = resolve_dispatch_account(Some("work"), Some("anthropic"), None);
        assert_eq!(got, Some(("work".into(), Some("anthropic".into()))));
    }

    #[test]
    fn explicit_account_overrides_bound_default() {
        // explicit account wins over the dispatcher's default, and uses
        // the explicit provider hint.
        let got =
            resolve_dispatch_account(Some("work"), Some("anthropic"), Some("automation-account"));
        assert_eq!(got, Some(("work".into(), Some("anthropic".into()))));
    }

    #[test]
    fn empty_account_falls_back_to_bound_default_identity_all_providers() {
        // / an empty / whitespace `req.account` falls back to
        // the dispatcher's bound default account IDENTITY — no provider hint, so
        // every provider the identity carries gets minted.
        assert_eq!(
            resolve_dispatch_account(None, None, Some("automation-account")),
            Some(("automation-account".into(), None))
        );
        assert_eq!(
            resolve_dispatch_account(Some("   "), None, Some("automation-account")),
            Some(("automation-account".into(), None))
        );
    }

    #[test]
    fn bare_explicit_account_carries_no_provider_hint() {
        // `account: "acme"` alone means "all of acme's providers" —
        // the resolution must not invent a family constraint.
        assert_eq!(resolve_dispatch_account(Some("acme"), None, None), Some(("acme".into(), None)));
    }

    #[test]
    fn no_account_and_no_binding_is_none() {
        // Unbound dispatcher + no explicit account: no gateway env injected
        // (behaves as before any account routing existed).
        assert_eq!(resolve_dispatch_account(None, None, None), None);
        assert_eq!(resolve_dispatch_account(Some(""), None, None), None);
    }

    #[test]
    fn disjoint_families_do_not_collide() {
        // one identity's claude + codex rows mint disjoint env keys.
        assert!(colliding_family([Family::Anthropic, Family::Openai]).is_none());
        assert!(colliding_family([]).is_none());
        assert!(colliding_family([Family::Openai]).is_none());
    }

    #[test]
    fn same_family_twice_collides() {
        // two same-family rows would fight over ANTHROPIC_AUTH_TOKEN /
        // OPENAI_API_KEY — the guard names the colliding family.
        assert!(matches!(
            colliding_family([Family::Anthropic, Family::Anthropic]),
            Some(Family::Anthropic)
        ));
        assert!(matches!(
            colliding_family([Family::Anthropic, Family::Openai, Family::Openai]),
            Some(Family::Openai)
        ));
    }

    #[test]
    fn alias_hit_rewrites_model_and_leaves_effort_untouched() {
        // a resolved model differing from the request model rewrites
        // `payload.model` in place; `effort` (and any other key) is untouched.
        let mut payload =
            serde_json::json!({ "model": "opus", "effort": "high", "flow": "triage" });
        assert!(rewrite_model_if_aliased(&mut payload, "opus", "claude-opus-4-8[1m]"));
        assert_eq!(payload["model"], "claude-opus-4-8[1m]");
        assert_eq!(payload["effort"], "high", "effort never rewritten");
        assert_eq!(payload["flow"], "triage", "unrelated keys preserved");
    }

    #[test]
    fn alias_miss_passes_model_through_verbatim() {
        // when resolution fails soft (mapped == raw), nothing changes —
        // no account / no alias leaves the forwarded model verbatim.
        let mut payload = serde_json::json!({ "model": "opus", "effort": "high" });
        assert!(!rewrite_model_if_aliased(&mut payload, "opus", "opus"));
        assert_eq!(payload["model"], "opus", "unchanged on alias miss");
        assert_eq!(payload["effort"], "high");
    }

    #[test]
    fn a_provider_hint_scopes_the_binding_family() {
        assert_eq!(binding_family(Some("openai"), "claude-code"), Family::Openai);
        assert_eq!(binding_family(Some("fireworks"), "claude-code"), Family::Fireworks);
    }

    #[test]
    fn without_a_hint_the_payload_adapter_scopes_the_binding_family() {
        assert_eq!(binding_family(None, "codex"), Family::Openai);
        assert_eq!(binding_family(Some("  "), "codex"), Family::Openai);
        assert_eq!(binding_family(None, "claude-code"), Family::Anthropic);
    }

    #[test]
    fn a_bound_account_beats_a_bound_pool() {
        // Both columns set: the account is the more specific instruction, the
        // same rule a name that denotes both an account and a pool follows.
        let pool = uuid::Uuid::new_v4();
        assert_eq!(
            default_binding(Some("alpha".into()), Some(pool)),
            Some(DefaultBinding::Account("alpha".into()))
        );
    }

    #[test]
    fn a_bound_pool_alone_elects_per_dispatch() {
        let pool = uuid::Uuid::new_v4();
        assert_eq!(default_binding(None, Some(pool)), Some(DefaultBinding::Pool(pool)));
    }

    #[test]
    fn an_unbound_dispatcher_injects_nothing() {
        assert_eq!(default_binding(None, None), None);
    }
}
