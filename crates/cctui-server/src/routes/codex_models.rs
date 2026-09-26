//! Codex model catalogs.
//!
//! Model availability is an **account** entitlement, so the source of truth is
//! the `ChatGPT` backend's models endpoint, read server-side with the account's
//! own OAuth credential (same treatment as `gateway::usage`). Catalogs reported
//! by daemons over `model/list` are kept as a per-machine fallback for accounts
//! we hold no OAuth for; on a gateway-only machine codex answers from its
//! compiled-in list, so a machine catalog never outranks an account one and
//! never replaces a stored catalog with a strict subset of itself.
//!
//! `GET /machines/{machine_id}/codex-models` returns one machine's catalog
//! (empty `models` when none is known, and the webui falls back to its static
//! offline list). `GET /models/codex` merges every catalog for pickers with no
//! machine in hand (dispatch, fork): a union by model id, account catalogs
//! first, then the newest machine report.
//! `POST /machines/{machine_id}/codex-models/refresh` re-reads every `OpenAI`
//! account's catalog from upstream — no daemon, no local `codex` binary.
//! Catalogs persist in `codex_model_catalogs` / `codex_account_model_catalogs`,
//! warmed into `AppState` on boot. Machine ownership is enforced by the
//! `authz_layer` guard (same as `fs::list_dirs`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use cctui_proto::codex_catalog::{CodexModel, CodexModelCatalog};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::error::AppError;
use crate::routes::gateway::{current_access_token, reload_account};
use crate::state::AppState;

/// How long an account catalog is served before the next read refreshes it.
pub const ACCOUNT_CATALOG_TTL: chrono::Duration = chrono::Duration::hours(6);

/// The `ChatGPT` backend's codex model catalog. Cookieless: the account's OAuth
/// Bearer plus its `chatgpt-account-id`, exactly like `wham/usage`.
/// Overridable via env to track upstream moves.
pub fn openai_models_url() -> String {
    std::env::var("CCTUI_OPENAI_MODELS_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex/models".into())
}

/// Last resort: used only when npm is unreachable and nothing was ever
/// resolved. Goes stale at the next codex release.
pub const BUNDLED_CODEX_CLIENT_VERSION: &str = "0.156.1";

pub const LATEST_VERSION_TTL: chrono::Duration = chrono::Duration::hours(1);

pub fn codex_latest_url() -> String {
    std::env::var("CCTUI_CODEX_LATEST_URL")
        .unwrap_or_else(|_| "https://registry.npmjs.org/@openai/codex/latest".into())
}

/// An operator's explicit freeze of the catalog view.
pub fn codex_version_pin() -> Option<String> {
    std::env::var("CCTUI_CODEX_CLIENT_VERSION").ok().filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Clone)]
pub struct CachedVersion {
    pub version: String,
    pub fetched_at: DateTime<Utc>,
}

/// Pin > resolved latest > bundled constant.
pub fn resolve_client_version(pin: Option<&str>, latest: Option<&str>) -> String {
    pin.or(latest).unwrap_or(BUNDLED_CODEX_CLIENT_VERSION).to_owned()
}

/// The `client_version` the models endpoint is tagged with — it gates which
/// models a caller is offered, so it tracks the codex release we speak.
pub fn codex_client_version(state: &AppState) -> String {
    let latest = state.codex_latest_version.lock().ok().and_then(|c| c.clone()).map(|c| c.version);
    resolve_client_version(codex_version_pin().as_deref(), latest.as_deref())
}

/// `.version` of the npm dist-tag document. `None` on any transport, status or
/// shape failure — the caller keeps the last good value.
pub async fn fetch_latest_codex_version(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| tracing::warn!("codex latest-version transport error: {e}"))
        .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "codex latest-version rejected");
        return None;
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| tracing::warn!("codex latest-version decode error: {e}"))
        .ok()?;
    body.get("version").and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// Re-resolve the latest codex release and, when the resolved `client_version`
/// moves, refetch every account catalog rather than waiting out
/// [`ACCOUNT_CATALOG_TTL`].
pub async fn refresh_client_version(state: &AppState) -> String {
    let (version, moved) = resolve_latest(state).await;
    if moved {
        refresh_account_catalogs(state).await;
    }
    version
}

async fn resolve_latest(state: &AppState) -> (String, bool) {
    resolve_latest_into(&state.http_client, &codex_latest_url(), &state.codex_latest_version).await
}

/// Re-resolve and cache; the flag says whether the resolved version moved, which
/// is what makes a catalog refetch due.
async fn resolve_latest_into(
    client: &reqwest::Client,
    url: &str,
    cache: &std::sync::Mutex<Option<CachedVersion>>,
) -> (String, bool) {
    let read = |cache: &std::sync::Mutex<Option<CachedVersion>>| {
        let latest = cache.lock().ok().and_then(|c| c.clone()).map(|c| c.version);
        resolve_client_version(codex_version_pin().as_deref(), latest.as_deref())
    };
    let before = read(cache);
    if codex_version_pin().is_some() {
        return (before, false);
    }
    if let Some(version) = fetch_latest_codex_version(client, url).await {
        if let Ok(mut slot) = cache.lock() {
            *slot = Some(CachedVersion { version, fetched_at: Utc::now() });
        }
    } else if let Ok(mut slot) = cache.lock() {
        // Stamp even on failure, so an unreachable npm is retried once per TTL
        // rather than on every request.
        let version = slot
            .as_ref()
            .map_or_else(|| BUNDLED_CODEX_CLIENT_VERSION.to_owned(), |c| c.version.clone());
        *slot = Some(CachedVersion { version, fetched_at: Utc::now() });
    }
    let after = read(cache);
    let moved = after != before;
    if moved {
        tracing::info!(%before, %after, "codex client_version moved, refetching account catalogs");
    }
    (after, moved)
}

fn refresh_client_version_if_stale(state: &AppState) {
    if codex_version_pin().is_some() {
        return;
    }
    let fresh = state
        .codex_latest_version
        .lock()
        .ok()
        .and_then(|c| c.as_ref().map(|c| c.fetched_at))
        .is_some_and(|at| Utc::now() - at < LATEST_VERSION_TTL);
    if fresh {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        refresh_client_version(&state).await;
    });
}

#[derive(Debug, Clone)]
pub struct CachedCatalog {
    pub catalog: CodexModelCatalog,
    pub fetched_at: DateTime<Utc>,
    /// Read from upstream with the account's own credential. Outranks a
    /// machine catalog of any age in the merge.
    pub authoritative: bool,
}

/// The merged cross-machine view: model ids from every machine, each taken
/// from the most recently fetched catalog that lists it.
#[derive(Debug, Default, Serialize)]
pub struct MergedCodexCatalog {
    pub models: Vec<CodexModel>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub machines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

pub fn merge_catalogs<'a>(
    catalogs: impl IntoIterator<Item = &'a CachedCatalog>,
) -> MergedCodexCatalog {
    let mut sorted: Vec<&CachedCatalog> = catalogs.into_iter().collect();
    sorted.sort_by_key(|c| (std::cmp::Reverse(c.authoritative), std::cmp::Reverse(c.fetched_at)));
    let mut merged = MergedCodexCatalog { machines: sorted.len(), ..Default::default() };
    let mut seen = std::collections::HashSet::new();
    for cached in sorted {
        merged.fetched_at.get_or_insert(cached.fetched_at);
        for model in &cached.catalog.models {
            if seen.insert(model.id.clone()) {
                merged.models.push(model.clone());
            }
        }
    }
    merged
}

/// Whether `incoming` would degrade `existing`: it lists nothing the stored
/// catalog does not already have, and fewer models. A machine whose codex fell
/// back to its bundled list reports exactly that, and a persisted downgrade is
/// unfixable from the UI — so it is dropped instead.
pub fn is_downgrade(existing: &CodexModelCatalog, incoming: &CodexModelCatalog) -> bool {
    if incoming.models.len() >= existing.models.len() {
        return false;
    }
    let have: std::collections::HashSet<&str> =
        existing.models.iter().map(|m| m.id.as_str()).collect();
    incoming.models.iter().all(|m| have.contains(m.id.as_str()))
}

pub async fn store_catalog(state: &AppState, machine_id: Uuid, catalog: CodexModelCatalog) {
    if let Some(existing) = state.codex_catalogs.get(&machine_id)
        && is_downgrade(&existing.catalog, &catalog)
    {
        tracing::info!(%machine_id, "ignoring codex catalog report: strict subset of the stored one");
        return;
    }
    let fetched_at = Utc::now();
    let json = serde_json::to_value(&catalog).unwrap_or(serde_json::Value::Null);
    state
        .codex_catalogs
        .insert(machine_id, CachedCatalog { catalog, fetched_at, authoritative: false });
    if let Err(err) = sqlx::query(
        "INSERT INTO codex_model_catalogs (machine_id, catalog, fetched_at) VALUES ($1, $2, $3) \
         ON CONFLICT (machine_id) DO UPDATE SET catalog = EXCLUDED.catalog, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(machine_id)
    .bind(json)
    .bind(fetched_at)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(%machine_id, %err, "failed to persist codex model catalog");
    }
}

/// Parse the models endpoint body into a catalog. The REST shape is `snake_case`
/// where the app-server protocol is camelCase, and either `models` or `data`
/// may carry the array, so both are accepted.
pub fn parse_remote_catalog(body: &Value) -> CodexModelCatalog {
    let array = body
        .get("models")
        .or_else(|| body.get("data"))
        .and_then(Value::as_array)
        .or_else(|| body.as_array());
    let models =
        array.map(|arr| arr.iter().filter_map(parse_remote_model).collect()).unwrap_or_default();
    CodexModelCatalog { models, client_version: None }
}

fn field<'a>(v: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    v.get(snake).or_else(|| v.get(camel))
}

fn parse_remote_model(v: &Value) -> Option<CodexModel> {
    let id = field(v, "id", "slug").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_owned();
    let model = field(v, "model", "modelSlug").and_then(Value::as_str).unwrap_or(&id).to_owned();
    let strings = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        e.as_str()
                            .or_else(|| e.get("reasoning_effort").and_then(Value::as_str))
                            .or_else(|| e.get("reasoningEffort").and_then(Value::as_str))
                    })
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(CodexModel {
        display_name: field(v, "display_name", "displayName")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_owned(),
        description: v.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(),
        hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false)
            || v.get("visibility").and_then(Value::as_str) == Some("hide"),
        minimal_client_version: field(v, "minimal_client_version", "minimalClientVersion")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        is_default: field(v, "is_default", "isDefault").and_then(Value::as_bool).unwrap_or(false),
        default_effort: field(v, "default_reasoning_effort", "defaultReasoningEffort")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        supported_efforts: strings(field(
            v,
            "supported_reasoning_efforts",
            "supportedReasoningEfforts",
        )),
        input_modalities: strings(field(v, "input_modalities", "inputModalities")),
        upgrade: v.get("upgrade").and_then(Value::as_str).map(str::to_owned),
        id,
        model,
    })
}

/// The upstream catalog read: cookieless Bearer + `chatgpt-account-id`, and the
/// `client_version` that gates which models come back.
pub fn catalog_request(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
    account_id: &str,
    client_version: &str,
) -> reqwest::RequestBuilder {
    client
        .get(url)
        .query(&[("client_version", client_version)])
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header(reqwest::header::ACCEPT, "*/*")
}

/// Read one `OpenAI` account's catalog from upstream with its stored OAuth.
/// `None` when the credential can't produce a catalog (not an openai provider,
/// no `chatgpt-account-id`, refresh failure, upstream error, empty body) — the
/// caller keeps whatever it had.
pub async fn fetch_account_catalog(
    state: &AppState,
    provider_id: Uuid,
) -> Option<CodexModelCatalog> {
    let acct = reload_account(state, provider_id).await?;
    if acct.provider != "openai" {
        return None;
    }
    let account_id = acct.provider_account_id.as_deref()?;
    let access_token = current_access_token(state, &acct).await.ok()?;
    let resp = catalog_request(
        &state.http_client,
        &openai_models_url(),
        &access_token,
        account_id,
        &codex_client_version(state),
    )
    .send()
    .await
    .map_err(|e| tracing::warn!(account = %provider_id, "codex models transport error: {e}"))
    .ok()?;
    if !resp.status().is_success() {
        tracing::warn!(account = %provider_id, status = %resp.status(), "codex models rejected");
        return None;
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| tracing::warn!(account = %provider_id, "codex models decode error: {e}"))
        .ok()?;
    let catalog = parse_remote_catalog(&body);
    if catalog.models.is_empty() { None } else { Some(catalog) }
}

pub async fn store_account_catalog(
    state: &AppState,
    provider_id: Uuid,
    catalog: CodexModelCatalog,
) {
    let fetched_at = Utc::now();
    let json = serde_json::to_value(&catalog).unwrap_or(Value::Null);
    state
        .codex_account_catalogs
        .insert(provider_id, CachedCatalog { catalog, fetched_at, authoritative: true });
    if let Err(err) = sqlx::query(
        "INSERT INTO codex_account_model_catalogs (provider_id, catalog, fetched_at) VALUES ($1, $2, $3) \
         ON CONFLICT (provider_id) DO UPDATE SET catalog = EXCLUDED.catalog, fetched_at = EXCLUDED.fetched_at",
    )
    .bind(provider_id)
    .bind(json)
    .bind(fetched_at)
    .execute(&state.pool)
    .await
    {
        tracing::warn!(account = %provider_id, %err, "failed to persist codex account catalog");
    }
}

/// Re-read every `OpenAI` account's catalog from upstream. Best-effort per
/// account: one failure never clears a stored catalog.
pub async fn refresh_account_catalogs(state: &AppState) -> usize {
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM account_providers WHERE provider = 'openai' AND encrypted_refresh_token IS NOT NULL",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(%err, "failed to list openai providers for a catalog refresh");
        Vec::new()
    });
    let mut refreshed = 0;
    for id in ids {
        if let Some(catalog) = fetch_account_catalog(state, id).await {
            store_account_catalog(state, id, catalog).await;
            refreshed += 1;
        }
    }
    refreshed
}

/// Refresh account catalogs in the background when the freshest one is older
/// than [`ACCOUNT_CATALOG_TTL`], so a new upstream model appears without any
/// per-machine action or webui release.
fn refresh_account_catalogs_if_stale(state: &AppState) {
    let freshest = state.codex_account_catalogs.iter().map(|c| c.fetched_at).max();
    if freshest.is_some_and(|at| Utc::now() - at < ACCOUNT_CATALOG_TTL) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        refresh_account_catalogs(&state).await;
    });
}

pub async fn warm_cache(state: &AppState) {
    let rows: Vec<(Uuid, serde_json::Value, DateTime<Utc>)> =
        sqlx::query_as("SELECT provider_id, catalog, fetched_at FROM codex_account_model_catalogs")
            .fetch_all(&state.pool)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "failed to load codex account catalogs");
                Vec::new()
            });
    for (provider_id, json, fetched_at) in rows {
        match serde_json::from_value::<CodexModelCatalog>(json) {
            Ok(catalog) => {
                state.codex_account_catalogs.insert(
                    provider_id,
                    CachedCatalog { catalog, fetched_at, authoritative: true },
                );
            }
            Err(err) => {
                tracing::warn!(account = %provider_id, %err, "malformed persisted codex catalog");
            }
        }
    }
    warm_machine_cache(state).await;
}

async fn warm_machine_cache(state: &AppState) {
    let rows: Vec<(Uuid, serde_json::Value, DateTime<Utc>)> =
        match sqlx::query_as("SELECT machine_id, catalog, fetched_at FROM codex_model_catalogs")
            .fetch_all(&state.pool)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "failed to load codex model catalogs");
                return;
            }
        };
    for (machine_id, json, fetched_at) in rows {
        match serde_json::from_value::<CodexModelCatalog>(json) {
            Ok(catalog) => {
                state.codex_catalogs.insert(
                    machine_id,
                    CachedCatalog { catalog, fetched_at, authoritative: false },
                );
            }
            Err(err) => tracing::warn!(%machine_id, %err, "malformed persisted codex catalog"),
        }
    }
}

fn parse_machine(machine_id: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(machine_id)
        .map_err(|_| AppError::new(StatusCode::BAD_REQUEST, "machine_id must be a uuid"))
}

pub async fn get_codex_models(
    State(state): State<AppState>,
    Path(machine_id): Path<String>,
) -> Result<Json<CodexModelCatalog>, AppError> {
    let machine_uuid = parse_machine(&machine_id)?;
    refresh_client_version_if_stale(&state);
    refresh_account_catalogs_if_stale(&state);
    let client_version = codex_client_version(&state);
    let accounts: Vec<CachedCatalog> =
        state.codex_account_catalogs.iter().map(|c| c.value().clone()).collect();
    if !accounts.is_empty() {
        return Ok(Json(CodexModelCatalog {
            models: merge_catalogs(&accounts).models,
            client_version: Some(client_version),
        }));
    }
    let mut catalog =
        state.codex_catalogs.get(&machine_uuid).map(|c| c.catalog.clone()).unwrap_or_default();
    catalog.client_version = Some(client_version);
    Ok(Json(catalog))
}

pub async fn get_merged_codex_models(
    State(state): State<AppState>,
) -> Result<Json<MergedCodexCatalog>, AppError> {
    refresh_client_version_if_stale(&state);
    refresh_account_catalogs_if_stale(&state);
    let cached: Vec<CachedCatalog> = state
        .codex_account_catalogs
        .iter()
        .chain(state.codex_catalogs.iter())
        .map(|c| c.value().clone())
        .collect();
    let mut merged = merge_catalogs(&cached);
    merged.client_version = Some(codex_client_version(&state));
    Ok(Json(merged))
}

/// The ↻ button. The machine is only the caller's authorization handle: the
/// catalog is read from upstream per account, since a gateway-only machine's
/// codex can only answer from its compiled-in list.
pub async fn refresh_codex_models(
    State(state): State<AppState>,
    Path(machine_id): Path<String>,
) -> Result<StatusCode, AppError> {
    parse_machine(&machine_id)?;
    resolve_latest(&state).await;
    refresh_account_catalogs(&state).await;
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, display_name: &str) -> CodexModel {
        CodexModel {
            id: id.to_owned(),
            model: id.to_owned(),
            display_name: display_name.to_owned(),
            description: String::new(),
            hidden: false,
            is_default: false,
            supported_efforts: vec![],
            default_effort: String::new(),
            input_modalities: vec![],
            upgrade: None,
            minimal_client_version: None,
        }
    }

    fn cached(models: Vec<CodexModel>, secs: i64) -> CachedCatalog {
        CachedCatalog {
            catalog: CodexModelCatalog { models, client_version: None },
            fetched_at: DateTime::from_timestamp(secs, 0).unwrap(),
            authoritative: false,
        }
    }

    #[test]
    fn merge_is_a_union_where_the_newest_report_wins() {
        let old = cached(vec![model("gpt-a", "A old"), model("gpt-old-only", "Old only")], 10);
        let new = cached(vec![model("gpt-a", "A new"), model("gpt-b", "B")], 20);
        let merged = merge_catalogs([&old, &new]);
        let labels: Vec<(&str, &str)> =
            merged.models.iter().map(|m| (m.id.as_str(), m.display_name.as_str())).collect();
        assert_eq!(labels, [("gpt-a", "A new"), ("gpt-b", "B"), ("gpt-old-only", "Old only")]);
        assert_eq!(merged.fetched_at, Some(new.fetched_at));
        assert_eq!(merged.machines, 2);
    }

    #[test]
    fn an_account_catalog_outranks_a_newer_machine_catalog() {
        let machine = cached(vec![model("gpt-5.5", "GPT-5.5 machine")], 99);
        let account = CachedCatalog {
            authoritative: true,
            ..cached(vec![model("gpt-6-astra", "Astra"), model("gpt-5.5", "GPT-5.5")], 1)
        };
        let merged = merge_catalogs([&machine, &account]);
        let ids: Vec<&str> = merged.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-astra", "gpt-5.5"]);
        assert_eq!(merged.models[1].display_name, "GPT-5.5");
    }

    #[test]
    fn a_bundled_fallback_report_never_replaces_a_richer_catalog() {
        let stored = CodexModelCatalog {
            models: vec![model("gpt-6-astra", "Astra"), model("gpt-5.5", "GPT-5.5")],
            client_version: None,
        };
        let fallback =
            CodexModelCatalog { models: vec![model("gpt-5.5", "GPT-5.5")], client_version: None };
        assert!(is_downgrade(&stored, &fallback));
        assert!(!is_downgrade(&fallback, &stored));
        assert!(!is_downgrade(&stored, &stored.clone()));
        let sideways =
            CodexModelCatalog { models: vec![model("gpt-7", "New")], client_version: None };
        assert!(!is_downgrade(&stored, &sideways));
    }

    #[test]
    fn the_models_endpoint_body_parses_in_either_casing() {
        let body = serde_json::json!({"models": [{
            "id": "gpt-6-astra",
            "display_name": "GPT-6-Astra",
            "is_default": true,
            "supported_reasoning_efforts": ["low", "high"],
            "default_reasoning_effort": "high"
        }, {
            "id": "gpt-5.5",
            "displayName": "GPT-5.5",
            "supportedReasoningEfforts": [{"reasoningEffort": "medium"}]
        }, {
            "displayName": "no id, dropped"
        }]});
        let catalog = parse_remote_catalog(&body);
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-astra", "gpt-5.5"]);
        assert_eq!(catalog.models[0].display_name, "GPT-6-Astra");
        assert_eq!(catalog.models[0].supported_efforts, ["low", "high"]);
        assert!(catalog.models[0].is_default);
        assert_eq!(catalog.models[1].display_name, "GPT-5.5");
        assert_eq!(catalog.models[1].supported_efforts, ["medium"]);
        assert_eq!(catalog.models[1].model, "gpt-5.5");
    }

    #[test]
    fn a_body_without_models_parses_to_an_empty_catalog() {
        assert!(parse_remote_catalog(&serde_json::json!({"error": "nope"})).models.is_empty());
        assert_eq!(parse_remote_catalog(&serde_json::json!([{"id": "a"}])).models.len(), 1);
    }

    #[test]
    fn the_client_version_prefers_a_pin_then_the_resolved_latest() {
        assert_eq!(resolve_client_version(Some("0.100.0"), Some("0.156.1")), "0.100.0");
        assert_eq!(resolve_client_version(None, Some("0.156.1")), "0.156.1");
        assert_eq!(resolve_client_version(None, None), BUNDLED_CODEX_CLIENT_VERSION);
        assert_eq!(resolve_client_version(Some("0.100.0"), None), "0.100.0");
    }

    #[test]
    fn a_hidden_visibility_and_a_minimum_version_are_read_off_a_model() {
        let body = serde_json::json!({"models": [{
            "id": "codex-auto-review",
            "visibility": "hide"
        }, {
            "id": "gpt-6-astra",
            "visibility": "list",
            "minimal_client_version": "0.153.0"
        }, {
            "id": "gpt-6-sol",
            "minimalClientVersion": "0.155.0"
        }, {
            "id": "gpt-5.5",
            "minimal_client_version": ""
        }]});
        let models = parse_remote_catalog(&body).models;
        assert!(models[0].hidden);
        assert_eq!(models[0].minimal_client_version, None);
        assert!(!models[1].hidden);
        assert_eq!(models[1].minimal_client_version.as_deref(), Some("0.153.0"));
        assert_eq!(models[2].minimal_client_version.as_deref(), Some("0.155.0"));
        assert_eq!(models[3].minimal_client_version, None);
    }

    /// Answers one request per entry of `bodies`, in order, and hands back the
    /// raw requests it saw.
    async fn mock_http(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut seen = Vec::new();
            for body in bodies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).to_string());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
            seen
        });
        (format!("http://{addr}/"), handle)
    }

    async fn one_shot(body: &'static str) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        mock_http(vec![body.to_owned()]).await
    }

    #[tokio::test]
    async fn the_latest_version_is_read_from_the_npm_dist_tag() {
        let (url, served) = one_shot(r#"{"name":"@openai/codex","version":"0.156.1"}"#).await;
        let client = reqwest::Client::new();
        assert_eq!(fetch_latest_codex_version(&client, &url).await.as_deref(), Some("0.156.1"));
        assert!(served.await.unwrap()[0].starts_with("GET /"));
    }

    #[tokio::test]
    async fn an_unreachable_npm_leaves_the_caller_with_no_version() {
        let client = reqwest::Client::new();
        let dead = "http://127.0.0.1:1/";
        assert_eq!(fetch_latest_codex_version(&client, dead).await, None);
        assert_eq!(resolve_client_version(None, None), BUNDLED_CODEX_CLIENT_VERSION);
    }

    #[tokio::test]
    async fn the_catalog_read_carries_the_resolved_client_version() {
        let (url, served) = one_shot(r#"{"models":[{"id":"gpt-6-astra"}]}"#).await;
        let client = reqwest::Client::new();
        let resp = catalog_request(&client, &url, "tok", "acct-1", "0.156.1")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(parse_remote_catalog(&resp).models[0].id, "gpt-6-astra");
        let req = served.await.unwrap().remove(0);
        assert!(req.contains("client_version=0.156.1"), "{req}");
        assert!(
            req.contains("authorization: Bearer tok") || req.contains("Authorization: Bearer tok")
        );
        assert!(req.contains("chatgpt-account-id: acct-1"));
    }

    #[tokio::test]
    async fn a_moved_version_is_reported_once_and_is_cached_after() {
        if codex_version_pin().is_some() {
            return;
        }
        let (url, served) = mock_http(vec![
            r#"{"version":"0.157.0"}"#.to_owned(),
            r#"{"version":"0.157.0"}"#.to_owned(),
        ])
        .await;
        let client = reqwest::Client::new();
        let cache = std::sync::Mutex::new(None);

        let (version, moved) = resolve_latest_into(&client, &url, &cache).await;
        assert_eq!(version, "0.157.0");
        assert!(moved, "moving off the bundled {BUNDLED_CODEX_CLIENT_VERSION} is a refetch");
        assert_eq!(cache.lock().unwrap().as_ref().unwrap().version, "0.157.0");

        let (version, moved) = resolve_latest_into(&client, &url, &cache).await;
        assert_eq!(version, "0.157.0");
        assert!(!moved, "an unchanged version must not refetch the catalogs");
        assert_eq!(served.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_unreachable_npm_keeps_the_last_good_version_and_never_moves() {
        if codex_version_pin().is_some() {
            return;
        }
        let (url, served) = one_shot(r#"{"version":"0.157.0"}"#).await;
        let client = reqwest::Client::new();
        let cache = std::sync::Mutex::new(None);
        assert!(resolve_latest_into(&client, &url, &cache).await.1);
        served.await.unwrap();

        let (version, moved) = resolve_latest_into(&client, "http://127.0.0.1:1/", &cache).await;
        assert_eq!(version, "0.157.0");
        assert!(!moved);
        let stamped = cache.lock().unwrap().as_ref().unwrap().fetched_at;
        assert!(Utc::now() - stamped < LATEST_VERSION_TTL);
    }

    #[test]
    fn merge_of_nothing_is_empty() {
        let merged = merge_catalogs([]);
        assert!(merged.models.is_empty());
        assert_eq!(merged.fetched_at, None);
        assert_eq!(merged.machines, 0);
    }
}
