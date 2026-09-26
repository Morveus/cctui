//! `/api/v1/accounts` — account identities + provider credentials.
//!
//! An **account** is an identity (e.g. `personal`, `enterprise`): a name, an
//! owner, optional extra environment, and sharing grants. Each account holds
//! zero or more **providers** (`account_providers`, née `oauth_accounts`):
//! one credential per provider family (anthropic | openai | fireworks) — a
//! native OAuth subscription, a compatible endpoint, or a static provider key.
//! OAuth refresh tokens
//! are encrypted at rest with the vault key (`crate::crypto`, same as
//! `api_keys`/`dispatchers`) and are **never** returned over the API —
//! list/get only ever surface provider/expiry/last-used + lightweight stats.
//! Accounts belong to the registering user and are visible/usable only by that
//! user (`require_human` + `owner_filter`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::{AuthContext, Scope};
use crate::routes::gateway;
use crate::state::AppState;

/// How long a pending "Sign in with Claude" login stays valid.
const PENDING_OAUTH_TTL: Duration = Duration::minutes(10);

/// In-memory store of pending OAuth logins, keyed by nonce.
pub type PendingOAuthLogins = Arc<DashMap<String, PendingOAuthLogin>>;

/// A pending "Sign in with Claude" login: the PKCE verifier we generated and the
/// user it belongs to, with a creation timestamp for TTL expiry. Held only in
/// memory and deleted on finish (single-use). `account_id` carries an optional
/// attach target: when set, the finished credential lands as a
/// provider under that existing account instead of creating a new identity.
#[derive(Clone)]
pub struct PendingOAuthLogin {
    pub user_id: Uuid,
    pub provider: String,
    pub code_verifier: String,
    pub created_at: DateTime<Utc>,
    pub account_id: Option<Uuid>,
}

/// Resolve which user an account operation targets. A user token
/// always acts as itself; the env admin token has no user identity, so it must
/// name the owner explicitly (`user_id` in the request). This is what lets an
/// admin-authed webui run the "Sign in with Claude/ChatGPT" flows instead of
/// bouncing off "user token required".
pub fn resolve_owner(
    ctx: &AuthContext,
    explicit: Option<Uuid>,
) -> Result<Uuid, (StatusCode, Json<serde_json::Value>)> {
    // A machine key has no business creating accounts: require a
    // human identity (read scope, no machine id). An admin acts cross-user by
    // naming the owner explicitly; a user acts as itself.
    if ctx.machine_id.is_some() || !ctx.has(Scope::Read) {
        return Err(err(StatusCode::FORBIDDEN, "user or admin token required"));
    }
    if ctx.is_admin() {
        explicit.ok_or_else(|| {
            err(StatusCode::BAD_REQUEST, "user_id required when using the admin token")
        })
    } else {
        Ok(ctx.user_id)
    }
}

/// Gate the account read/mutation routes to a human identity (a user or admin
/// token, never a machine key). Admin then sees/acts across all owners via
/// `owner_filter`.
pub fn require_human(ctx: &AuthContext) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if ctx.machine_id.is_some() || !ctx.has(Scope::Read) {
        return Err(err(StatusCode::FORBIDDEN, "user or admin token required"));
    }
    Ok(())
}

/// Back-compat shim: if `CCTUI_CLAUDE_LITELLM_*` is set,
/// synthesize a server-owned **managed** anthropic-compatible provider per user
/// (under a dedicated `litellm (legacy)` account identity) so existing
/// deployments keep working after the env-var path is retired. Managed
/// providers are read-only over the API (edit/delete excluded). Idempotent:
/// re-upserted on every restart against the partial unique index
/// `(user_id, provider) WHERE managed`. A no-op unless both the endpoint and the
/// model list are configured. Retained for env-var deployments.
// Linear per-user upsert loop; the parent+child pair pushes it over the limit.
#[allow(clippy::cognitive_complexity)]
pub async fn sync_litellm_shim(pool: &sqlx::PgPool, config: &crate::config::Config) {
    let Some(endpoint) = config.claude_litellm_endpoint.as_deref() else { return };
    let models = config.claude_litellm_visible_models();
    if models.is_empty() {
        return;
    }
    let key = crate::crypto::vault_key();
    let cred = config.claude_litellm_token.as_deref().unwrap_or("sk-dummy");
    let enc_access = crate::crypto::encrypt(cred, &key);
    let models_json = serde_json::to_value(
        models
            .iter()
            .map(|m| AccountModel {
                model: m.model.clone(),
                label: m.label.clone(),
                ..AccountModel::default()
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or(serde_json::Value::Null);

    // One managed provider per user, keyed by (user_id, provider) WHERE managed,
    // parented under an idempotently upserted `litellm (legacy)` identity.
    let users: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE revoked_at IS NULL")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for uid in users {
        // Resolve the parent identity via the user's existing MANAGED provider
        // first — never adopt a same-named account the user made themselves:
        // the `ON CONFLICT (user_id, name) DO UPDATE … RETURNING id`
        // find-or-create hijacked any user account literally named
        // `litellm (legacy)`, making it read-only/undeletable (managed guard)
        // or failing the (account_id, family) unique index outright.
        let managed_parent: Option<Uuid> = sqlx::query_scalar(
            "SELECT account_id FROM account_providers \
             WHERE user_id = $1 AND managed AND provider = 'anthropic-compatible'",
        )
        .bind(uid)
        .fetch_optional(pool)
        .await
        .unwrap_or_default();
        let parent: Result<Option<Uuid>, sqlx::Error> = match managed_parent {
            Some(id) => Ok(Some(id)),
            None => {
                sqlx::query_scalar(
                    "INSERT INTO accounts (user_id, name) VALUES ($1, 'litellm (legacy)') \
                     ON CONFLICT (user_id, name) DO NOTHING \
                     RETURNING id",
                )
                .bind(uid)
                .fetch_optional(pool)
                .await
            }
        };
        let account_id = match parent {
            Ok(Some(id)) => id,
            Ok(None) => {
                tracing::warn!(
                    %uid,
                    "litellm shim skipped: user already has an unmanaged account named \
                     'litellm (legacy)'"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(%uid, "litellm shim parent upsert failed: {e}");
                continue;
            }
        };
        let res = sqlx::query(
            "INSERT INTO account_providers \
                (user_id, account_id, provider, encrypted_access_token, base_url, models, \
                 auth_scheme, managed) \
             VALUES ($1, $2, 'anthropic-compatible', $3, $4, $5, 'bearer', TRUE) \
             ON CONFLICT (user_id, provider) WHERE managed DO UPDATE \
               SET encrypted_access_token = EXCLUDED.encrypted_access_token, \
                   base_url = EXCLUDED.base_url, models = EXCLUDED.models, \
                   account_id = EXCLUDED.account_id",
        )
        .bind(uid)
        .bind(account_id)
        .bind(&enc_access)
        .bind(endpoint)
        .bind(&models_json)
        .execute(pool)
        .await;
        if let Err(e) = res {
            tracing::warn!(%uid, "litellm shim upsert failed: {e}");
        }
    }
    tracing::info!("CCTUI_CLAUDE_LITELLM_* shim: synced managed compatible providers");
}

/// One selectable model on a compatible-endpoint or `fireworks` provider:
/// `model` is the `--model` code, `label` the display name. Safe to return over
/// the API — model names are not secret (unlike the credential).
///
/// Pricing is per *million* tokens in USD and is account-owned data: it is what
/// a pay-per-token provider is metered against, so it lives on the row rather
/// than in a table someone has to redeploy to correct.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AccountModel {
    pub model: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cached_input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<i64>,
}

/// Seed catalog for a new `fireworks` provider. DATA, not policy: it is written
/// to the row at create and fully editable afterwards — no spawn, dispatch,
/// entrypoint, or worker path may hardcode a model id.
fn fireworks_default_models() -> serde_json::Value {
    serde_json::json!([{
        "model": "accounts/fireworks/models/kimi-k3",
        "label": "Kimi K3",
        "price_input_per_mtok": 3.0,
        "price_cached_input_per_mtok": 0.3,
        "price_output_per_mtok": 15.0,
        "context_length": 1_048_576,
    }])
}

/// API view of one provider credential under an account. Secrets (the
/// OAuth/static tokens) are deliberately absent; `base_url`/`auth_scheme` are
/// surfaced so the accounts UI can render/edit a compatible endpoint in place.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct ProviderInfo {
    pub id: Uuid,
    pub account_id: Uuid,
    /// `anthropic` | `openai` | `anthropic-compatible` | `openai-compatible`.
    pub provider: String,
    /// Provider family (generated column): `anthropic` | `openai`. At most one
    /// provider per family per account, guaranteed by construction.
    pub family: String,
    /// Models this credential offers, declared by the operator, with optional
    /// pricing. `None`/empty falls back to the harness catalog / native
    /// families. Honoured for every provider kind.
    pub models: Option<serde_json::Value>,
    /// Per-provider logical→concrete model alias map, e.g.
    /// `{"opus": "claude-opus-4-8[1m]"}`. Resolved server-side at spawn.
    pub model_aliases: Option<serde_json::Value>,
    /// `true` for a server-synthesized (managed) provider — read-only over the
    /// API (the back-compat shim for `CCTUI_CLAUDE_LITELLM_*`).
    pub managed: bool,
    /// Compatible-endpoint base URL; NULL for native providers.
    pub base_url: Option<String>,
    /// `oauth` (native) | `bearer` | `api_key` (compatible).
    pub auth_scheme: String,
    /// Upstream account id (Codex `chatgpt_account_id`).
    pub provider_account_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub request_count: i64,
    pub bytes_transferred: i64,
    /// Total tokens (input + output + cache) attributed to this provider across
    /// all its sessions. Joined from `session_tokens` →
    /// `session_token_usage` at read time.
    pub total_tokens: i64,
    /// USD cost of this provider's recorded usage. For a pay-per-token
    /// (`fireworks`) row this is priced per model from the account's own
    /// catalog and is the real spend; for subscription rows it is a blended-rate
    /// estimate — a usage-weight signal, not a bill.
    pub est_cost_usd: f64,
    /// Per-provider soft limits: a validated JSONB map keyed by
    /// canonical window identity (`session` | `weekly_all` | `weekly_model:<id>`),
    /// each value `{cap_pct?, bypass_minutes?, pace_cap?}`. NULL ⇒ no soft limits configured.
    pub soft_limits: Option<serde_json::Value>,
    /// Usage ticker `{ enabled, step_pct }`; NULL ⇒ off.
    pub usage_notices: Option<serde_json::Value>,
    /// Whether this credential's gauge shows in the header strip.
    pub header_pin: bool,
    /// Credential health: `true` once the gateway saw the upstream
    /// provider reject this credential, cleared on the next successful upstream
    /// call. The accounts UI shows a "reauthenticate" badge.
    pub needs_reauth: bool,
    pub last_auth_error: Option<String>,
    pub last_auth_error_at: Option<DateTime<Utc>>,
    /// Validated, allowlisted subset of harness settings applied to sessions run
    /// under this provider. Config, not secret → returned normally.
    pub settings_json: Option<serde_json::Value>,
    /// Gateway request-shaping settings for this credential (fireworks:
    /// `context_length_exceeded_behavior`, session affinity, extra body keys).
    /// Distinct from `settings_json`, which is harness settings.
    pub provider_settings: Option<serde_json::Value>,
    /// Per-(account, provider) gateway rate limits `{ rpm?, tpm? }`, enforced in
    /// the proxy path. NULL ⇒ no throttling.
    pub rate_limits: Option<serde_json::Value>,
}

/// API view of an account identity: name, owner, timestamps, and its
/// provider credentials.
#[derive(Debug, serde::Serialize)]
pub struct AccountInfo {
    pub id: Uuid,
    pub name: String,
    /// Owner-chosen identity glyph: one emoji grapheme, or NULL for the
    /// generated letter square.
    pub emoji: Option<String>,
    /// Owning user — admins see all accounts, so the owner matters.
    pub user_id: Uuid,
    /// Owner's name for display, joined from `users`.
    pub user_name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub providers: Vec<ProviderInfo>,
    /// The owner's veto on pool membership: with this false, only the owner
    /// may enrol this account in an account pool. Grantees can still launch on
    /// it by name — they just cannot make it a silent overflow target.
    pub pool_eligible: bool,
    /// Relative plan size inside a pool aggregate (upstream reports percent,
    /// never the plan behind it). `1` = same as the other members.
    pub pool_weight: f32,
    /// Names (only) of the account's free-form extra env vars, sorted.
    /// Values stay WRITE-ONLY (encrypted, never returned) — the names let the UI
    /// show what is currently set with a replace-on-save affordance.
    pub env_names: Vec<String>,
    // NOTE: env VALUES are deliberately NOT a field here — the `env_json` blob
    // holds encrypted extra environment (possibly secrets) and is WRITE-ONLY,
    // never returned over the API, exactly like the OAuth tokens.
}

/// Identity-row projection (`accounts` + owner name); providers are attached
/// separately from [`PROVIDER_SELECT`].
#[derive(Debug, sqlx::FromRow)]
struct AccountRow {
    id: Uuid,
    name: String,
    emoji: Option<String>,
    user_id: Uuid,
    user_name: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    /// Encrypted extra-env blob; decrypted to NAMES only for the API.
    env_json: Option<String>,
    pool_eligible: bool,
    pool_weight: f32,
}

impl AccountRow {
    fn into_info(self, providers: Vec<ProviderInfo>, key: &[u8]) -> AccountInfo {
        let env_names = env_names_from_enc(self.env_json.as_deref(), key);
        AccountInfo {
            id: self.id,
            name: self.name,
            emoji: self.emoji,
            user_id: self.user_id,
            user_name: self.user_name,
            pool_eligible: self.pool_eligible,
            pool_weight: self.pool_weight,
            created_at: self.created_at,
            updated_at: self.updated_at,
            providers,
            env_names,
        }
    }
}

/// Shared SELECT for [`ProviderInfo`]: per-provider token totals + a rough USD
/// cost estimate. Tokens are recorded per session
/// (`session_token_usage`); `session_tokens` bridges a session to the provider
/// row it ran under. `SUM()` over bigint returns NUMERIC, so cast back to bigint
/// for the i64 columns. Cost uses a per-provider blended per-million rate
/// (input/output/cache weighted) — an estimate, not a meter. Append a
/// `WHERE`/`ORDER BY` clause before use. The token totals are a correlated
/// LATERAL rather than a grouped subquery so that selecting one provider
/// aggregates one provider's rows instead of the whole table.
const PROVIDER_SELECT: &str = "SELECT p.id, p.account_id, p.provider, p.family, p.models, p.model_aliases, p.managed, \
            p.base_url, p.auth_scheme, p.provider_account_id, \
            p.expires_at, p.created_at, p.last_used_at, \
            p.request_count, p.bytes_transferred, \
            p.soft_limits_json AS soft_limits, p.usage_notices, p.header_pin, \
            p.needs_reauth, p.last_auth_error, p.last_auth_error_at, p.settings_json, \
            p.provider_settings, p.rate_limits_json AS rate_limits, \
            (COALESCE(t.input_tokens,0) + COALESCE(t.output_tokens,0) \
             + COALESCE(t.cache_read_tokens,0) + COALESCE(t.cache_creation_tokens,0))::bigint \
              AS total_tokens, \
            (CASE \
               WHEN p.family = 'fireworks' THEN COALESCE(fw.usd, 0) * 1000000.0 \
               WHEN p.provider = 'openai' THEN \
                 COALESCE(t.input_tokens,0)*1.25 + COALESCE(t.output_tokens,0)*10 \
                 + COALESCE(t.cache_read_tokens,0)*0.125 + COALESCE(t.cache_creation_tokens,0)*1.25 \
               ELSE \
                 COALESCE(t.input_tokens,0)*3 + COALESCE(t.output_tokens,0)*15 \
                 + COALESCE(t.cache_read_tokens,0)*0.3 + COALESCE(t.cache_creation_tokens,0)*3.75 \
             END / 1000000.0)::double precision AS est_cost_usd \
     FROM account_providers p \
     LEFT JOIN LATERAL ( \
         SELECT SUM(stu.input_tokens)          AS input_tokens, \
                SUM(stu.output_tokens)         AS output_tokens, \
                SUM(stu.cache_read_tokens)     AS cache_read_tokens, \
                SUM(stu.cache_creation_tokens) AS cache_creation_tokens \
         FROM session_tokens st \
         JOIN session_token_usage stu ON stu.session_id = st.session_id \
         WHERE st.account_id = p.id \
     ) t ON true \
     LEFT JOIN LATERAL ( \
         SELECT SUM(( \
                  (stu.input_tokens + stu.cache_creation_tokens) \
                    * COALESCE((m.value->>'price_input_per_mtok')::double precision, 0) \
                  + stu.cache_read_tokens \
                    * COALESCE((m.value->>'price_cached_input_per_mtok')::double precision, 0) \
                  + stu.output_tokens \
                    * COALESCE((m.value->>'price_output_per_mtok')::double precision, 0) \
                ) / 1000000.0) AS usd \
         FROM session_tokens st \
         JOIN session_token_usage stu ON stu.session_id = st.session_id \
         JOIN jsonb_array_elements( \
                CASE WHEN jsonb_typeof(p.models) = 'array' THEN p.models ELSE '[]'::jsonb END \
              ) m ON m.value->>'model' = stu.model \
         WHERE st.account_id = p.id \
     ) fw ON p.family = 'fireworks'";

/// Identity SELECT for [`AccountRow`]. Append a `WHERE`/`ORDER BY` before use.
const ACCOUNT_SELECT: &str = "SELECT a.id, a.name, a.emoji, a.user_id, u.name AS user_name, a.created_at, a.updated_at, \
     a.env_json, a.pool_eligible, a.pool_weight \
     FROM accounts a JOIN users u ON u.id = a.user_id";

/// Fetch one account (owner-scoped: `owner` NULL = admin sees all) with its
/// providers. `Ok(None)` = no such account for that scope.
async fn fetch_account_info(
    pool: &sqlx::PgPool,
    id: Uuid,
    owner: Option<Uuid>,
) -> Result<Option<AccountInfo>, sqlx::Error> {
    let row: Option<AccountRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ACCOUNT_SELECT} WHERE a.id = $1 AND ($2::uuid IS NULL OR a.user_id = $2)"
    )))
    .bind(id)
    .bind(owner)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let providers: Vec<ProviderInfo> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{PROVIDER_SELECT} WHERE p.account_id = $1 ORDER BY p.family"
    )))
    .bind(id)
    .fetch_all(pool)
    .await?;
    let key = crate::crypto::vault_key();
    Ok(Some(row.into_info(providers, &key)))
}

/// Fetch one provider row by id. `Ok(None)` = no such provider.
async fn fetch_provider_info(
    pool: &sqlx::PgPool,
    id: Uuid,
) -> Result<Option<ProviderInfo>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("{PROVIDER_SELECT} WHERE p.id = $1")))
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Provider-credential payload: the create/attach fields for one
/// provider row. Used standalone by `POST /accounts/{id}/providers` and
/// flattened into [`CreateAccount`] so the legacy one-shot account+credential
/// create keeps working.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ProviderSpec {
    /// `anthropic` | `openai` (native subscription) | `anthropic-compatible` |
    /// `openai-compatible`. Optional only when flattened into
    /// [`CreateAccount`] (identity-only create); required on the provider route.
    #[serde(default)]
    pub provider: Option<String>,
    /// OAuth refresh token (subscription providers). Optional for compatible
    /// endpoints, which store only a static credential (in `access_token`).
    /// Stored encrypted; the gateway exchanges it for access tokens on demand.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Initial access token (subscription) OR the static credential (a compatible
    /// endpoint's bearer/api key). Stored encrypted; never read back.
    #[serde(default)]
    pub access_token: Option<String>,
    /// Optional access-token expiry (unix seconds). When absent the gateway
    /// refreshes on first use (subscription) / never refreshes (compatible).
    #[serde(default)]
    pub expires_at: Option<i64>,
    /// Compatible-endpoint base URL, e.g. a LiteLLM/vLLM/Ollama-proxy.
    /// Required for `*-compatible` providers; ignored for native ones.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Models this credential offers; empty falls back to the harness catalog.
    #[serde(default)]
    pub models: Option<Vec<AccountModel>>,
    /// Logical→concrete model alias map, e.g.
    /// `{"opus": "claude-opus-4-8[1m]"}`. Honoured for every provider.
    #[serde(default)]
    pub model_aliases: Option<std::collections::HashMap<String, String>>,
    /// Credential scheme for a compatible endpoint: `bearer` | `api_key`.
    /// Defaults to `bearer`. Native providers are always `oauth`.
    #[serde(default)]
    pub auth_scheme: Option<String>,
    /// Per-provider soft limits: a canonical-key map
    /// `{ "session": {cap_pct?, bypass_minutes?, pace_cap?}, "weekly_all": {…}, … }`.
    /// Absent ⇒ NULL (no caps). Validated before persist.
    #[serde(default)]
    pub soft_limits: Option<serde_json::Value>,
    /// Legacy scalar soft-limit fields, still accepted on create and
    /// folded into the `session` / `weekly_all` keys when `soft_limits` is absent.
    #[serde(default)]
    pub soft_limit_5h_pct: Option<i32>,
    #[serde(default)]
    pub soft_limit_7d_pct: Option<i32>,
    #[serde(default)]
    pub soft_limit_bypass_5h_minutes: Option<i32>,
    #[serde(default)]
    pub soft_limit_bypass_7d_minutes: Option<i32>,
    #[serde(default)]
    pub soft_limit_bypass_minutes: Option<i32>,
    /// Validated, allowlisted harness settings for this provider.
    /// Server rejects MANAGED/SYSTEM keys before persist. Returned normally.
    #[serde(default)]
    pub settings_json: Option<serde_json::Value>,
    /// Gateway request-shaping settings. Absent on a `fireworks` create seeds
    /// the defaults so every knob is visible and editable from the start.
    #[serde(default)]
    pub provider_settings: Option<serde_json::Value>,
    /// Per-(account, provider) gateway rate limits `{ rpm?, tpm? }`. Absent ⇒
    /// NULL (no throttling). Validated before persist.
    #[serde(default)]
    pub rate_limits: Option<serde_json::Value>,
}

/// `POST /api/v1/accounts` payload: the identity fields, plus an
/// optionally flattened [`ProviderSpec`] — supplying `provider` creates the
/// account and its first credential in one call (the legacy shape).
#[derive(Debug, serde::Deserialize)]
pub struct CreateAccount {
    pub name: String,
    /// Identity glyph; one emoji grapheme. Blank/absent ⇒ the letter square.
    #[serde(default)]
    pub emoji: Option<String>,
    /// Owning user — required (and only honoured) when authenticated with the
    /// admin token, which has no user identity of its own.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Extra environment variables for sessions run under this account.
    /// Stored ENCRYPTED at rest and never returned over the API (write-only,
    /// like the OAuth tokens). An empty map ⇒ no override.
    #[serde(default)]
    pub env_json: Option<std::collections::HashMap<String, String>>,
    #[serde(flatten)]
    pub provider: ProviderSpec,
}

/// `PATCH /api/v1/accounts/{id}` payload: identity-level fields only.
/// `name` renames; `env_json` provided → re-encrypts and replaces (an empty map
/// clears it); absent → unchanged (write-only, never returned). The legacy
/// provider-ish fields are accepted syntactically but rejected with a pointer
/// to the provider route, so an un-migrated client gets a clear 400 instead of
/// a silent no-op.
#[derive(Debug, serde::Deserialize)]
pub struct UpdateAccount {
    #[serde(default)]
    pub name: Option<String>,
    /// Identity glyph. A blank string clears it back to the letter square;
    /// absent leaves it unchanged.
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub env_json: Option<std::collections::HashMap<String, String>>,
    /// Names to remove from the stored env without re-sending the other
    /// values: decrypt → drop names → re-encrypt, all server-side. Ignored
    /// when `env_json` is provided (replace-all wins).
    #[serde(default)]
    pub env_remove: Option<Vec<String>>,
    /// Owner-only: whether grantees may enrol this account in their pools.
    #[serde(default)]
    pub pool_eligible: Option<bool>,
    /// Owner-only: the account's relative plan size in pool aggregates. Must
    /// be a finite positive number.
    #[serde(default)]
    pub pool_weight: Option<f32>,
    // Legacy provider fields (any shape): presence ⇒ 400 pointing at
    // PATCH /accounts/{id}/providers/{provider_id}.
    #[serde(default)]
    pub base_url: Option<serde_json::Value>,
    #[serde(default)]
    pub auth_scheme: Option<serde_json::Value>,
    #[serde(default)]
    pub models: Option<serde_json::Value>,
    #[serde(default)]
    pub model_aliases: Option<serde_json::Value>,
    #[serde(default)]
    pub access_token: Option<serde_json::Value>,
    #[serde(default)]
    pub soft_limits: Option<serde_json::Value>,
    #[serde(default)]
    pub settings_json: Option<serde_json::Value>,
    #[serde(default)]
    pub defaults: Option<serde_json::Value>,
}

/// `PATCH /api/v1/accounts/{id}/providers/{provider_id}` payload. A partial update:
/// for a non-managed compatible endpoint the operator may edit `base_url`,
/// `auth_scheme`, and rotate the static credential (`access_token`).
/// `models` / `model_aliases` / `soft_limits` / `settings_json` are editable
/// for every provider. All optional; an absent field leaves that column unchanged.
/// `base_url`/credential are never returned, so the editor re-supplies
/// `base_url` when changing it and leaves the credential blank to keep the
/// stored one.
#[derive(Debug, serde::Deserialize)]
pub struct UpdateProvider {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth_scheme: Option<String>,
    #[serde(default)]
    pub models: Option<Vec<AccountModel>>,
    /// Replacement model alias map. Provided → replaces the stored map
    /// wholesale (an empty object clears it); absent → unchanged.
    #[serde(default)]
    pub model_aliases: Option<std::collections::HashMap<String, String>>,
    /// New static credential for a compatible endpoint; blank/absent keeps the
    /// stored one.
    #[serde(default)]
    pub access_token: Option<String>,
    /// Replacement soft-limit config: a canonical-key map
    /// `{ key: {cap_pct?, bypass_minutes?, pace_cap?} }`. Provided → replaces the whole
    /// stored map (an empty object clears it, an omitted key drops that window);
    /// absent → unchanged. Validated before persist.
    #[serde(default)]
    pub soft_limits: Option<serde_json::Value>,
    /// Replacement usage ticker `{ enabled?, step_pct? }`. Provided → replaces
    /// (an empty object / `enabled: false` turns it off); absent → unchanged.
    #[serde(default)]
    pub usage_notices: Option<serde_json::Value>,
    /// Show this credential's gauge in the header strip; absent → unchanged.
    #[serde(default)]
    pub header_pin: Option<bool>,
    /// Replacement validated settings blob. Provided → replaces the
    /// stored settings wholesale (an empty object clears it); absent → unchanged.
    /// Validated against the allowlist before persist.
    #[serde(default)]
    pub settings_json: Option<serde_json::Value>,
    /// Replacement gateway settings object. Provided → replaces wholesale (an
    /// empty object drops back to the family defaults); absent → unchanged.
    #[serde(default)]
    pub provider_settings: Option<serde_json::Value>,
    /// Replacement rate-limit object `{ rpm?, tpm? }`. Provided → replaces the
    /// stored value (an empty object / zeros clear it); absent → unchanged.
    #[serde(default)]
    pub rate_limits: Option<serde_json::Value>,
}

/// `POST /api/v1/accounts/{id}/providers/{provider_id}/move` payload:
/// re-parent a provider credential onto another account owned by the same user
/// (the manual merge path for the migration's one-account-per-old-row backfill).
#[derive(Debug, serde::Deserialize)]
pub struct MoveProvider {
    pub target_account_id: Uuid,
}

/// Validate a canonical-key soft-limit map into the JSONB blob to
/// store, folding in any legacy scalar fields. Returns `Ok(None)` when the
/// result is empty (clears the column). Rejects out-of-range caps/bypasses and
/// non-canonical keys (which could otherwise inject markup or collide).
fn build_soft_limits_json(
    map: Option<&serde_json::Value>,
    legacy_session_cap: Option<i32>,
    legacy_weekly_cap: Option<i32>,
    legacy_session_bypass: Option<i32>,
    legacy_weekly_bypass: Option<i32>,
) -> Result<Option<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use std::collections::BTreeMap;
    let mut out: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    let mut insert = |key: &str,
                      cap: Option<i32>,
                      cap_usd: Option<f64>,
                      bypass: Option<i32>,
                      pace_cap: Option<f64>| {
        if let Some(c) = cap
            && !(0..=100).contains(&c)
        {
            return Err(err(StatusCode::BAD_REQUEST, "soft-limit cap must be 0-100"));
        }
        // Below 1x is a cap tighter than an even spend, which would refuse an
        // idle-then-work account forever; above 100x can never trigger.
        if let Some(p) = pace_cap
            && (!p.is_finite() || !(1.0..=100.0).contains(&p))
        {
            return Err(err(StatusCode::BAD_REQUEST, "soft-limit pace_cap must be 1-100"));
        }
        if let Some(c) = cap_usd
            && (!c.is_finite() || c < 0.0)
        {
            return Err(err(StatusCode::BAD_REQUEST, "soft-limit cap_usd must be >= 0"));
        }
        if let Some(b) = bypass
            && b < 0
        {
            return Err(err(StatusCode::BAD_REQUEST, "soft-limit bypass must be >= 0"));
        }
        if cap.is_none() && cap_usd.is_none() && bypass.is_none() && pace_cap.is_none() {
            return Ok(());
        }
        let Some(canon) = crate::soft_limit::canonicalize_key(key) else {
            return Err(err(StatusCode::BAD_REQUEST, "unknown soft-limit window key"));
        };
        // A dollar cap belongs only to a dollar window, and vice versa: storing
        // the wrong one would read as an unenforceable cap in the UI.
        let usd_window = crate::soft_limit::is_usd_key(&canon);
        let mut entry = serde_json::Map::new();
        if let Some(c) = cap.filter(|_| !usd_window) {
            entry.insert("cap_pct".into(), serde_json::json!(c));
        }
        if let Some(c) = cap_usd.filter(|_| usd_window) {
            entry.insert("cap_usd".into(), serde_json::json!(c));
        }
        if let Some(b) = bypass {
            entry.insert("bypass_minutes".into(), serde_json::json!(b));
        }
        if let Some(p) = pace_cap.filter(|_| !usd_window) {
            entry.insert("pace_cap".into(), serde_json::json!(p));
        }
        if entry.is_empty() {
            return Ok(());
        }
        out.insert(canon, serde_json::Value::Object(entry));
        Ok(())
    };

    if let Some(obj) = map.and_then(serde_json::Value::as_object) {
        for (key, v) in obj {
            let cap = v.get("cap_pct").and_then(serde_json::Value::as_i64).map(|n| n as i32);
            let cap_usd = v.get("cap_usd").and_then(serde_json::Value::as_f64);
            let bypass =
                v.get("bypass_minutes").and_then(serde_json::Value::as_i64).map(|n| n as i32);
            let pace_cap = v.get("pace_cap").and_then(serde_json::Value::as_f64);
            insert(key, cap, cap_usd, bypass, pace_cap)?;
        }
    } else if map.is_some_and(|v| !v.is_null()) {
        return Err(err(StatusCode::BAD_REQUEST, "soft_limits must be an object"));
    } else {
        // No map supplied — fold the legacy scalar fields.
        insert(
            crate::soft_limit::KEY_SESSION,
            legacy_session_cap,
            None,
            legacy_session_bypass,
            None,
        )?;
        insert(
            crate::soft_limit::KEY_WEEKLY_ALL,
            legacy_weekly_cap,
            None,
            legacy_weekly_bypass,
            None,
        )?;
    }

    Ok((!out.is_empty()).then(|| serde_json::to_value(out).unwrap_or(serde_json::Value::Null)))
}

/// Validate a `{ rpm?, tpm? }` rate-limit object into the JSONB blob to store.
/// A zero or omitted dimension clears that limit; `Ok(None)` when neither is set
/// (clears the column). Rejects negative / non-integer values and a non-object.
fn build_rate_limits_json(
    map: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let Some(map) = map.filter(|v| !v.is_null()) else { return Ok(None) };
    let Some(obj) = map.as_object() else {
        return Err(err(StatusCode::BAD_REQUEST, "rate_limits must be an object"));
    };
    let mut out = serde_json::Map::new();
    for key in ["rpm", "tpm"] {
        match obj.get(key) {
            None | Some(serde_json::Value::Null) => {}
            Some(v) => {
                let Some(n) = v.as_u64() else {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "rate limit must be a whole number >= 0",
                    ));
                };
                if n > 0 {
                    out.insert(key.to_owned(), serde_json::json!(n));
                }
            }
        }
    }
    Ok((!out.is_empty()).then_some(serde_json::Value::Object(out)))
}

pub fn err(code: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(serde_json::json!({ "error": msg })))
}

fn db_err(e: &sqlx::Error) -> (StatusCode, Json<serde_json::Value>) {
    tracing::error!("db error: {e}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
}

/// The settings catalog as served to the webui account-settings editor.
/// Everything here comes from the embedded catalog — the webui
/// carries NO mirror of the key list, so it cannot drift from the server that
/// validates the writes. `managed`/`system` keys are omitted entirely.
#[derive(serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SettingsCatalogResponse {
    /// Exposable (safe/care) `settings.json` keys, catalog order. Keys with a
    /// `group`/`label` are the curated boolean toggles; the rest are settable
    /// via the raw-JSON box only.
    pub keys: Vec<crate::settings_catalog::SettingKey>,
    /// The curated env-var allowlist (all exposable by construction).
    pub env: Vec<crate::settings_catalog::EnvVar>,
    /// The "Quiet defaults" preset, with its `settings` filtered to exposable
    /// keys (the server-applied MANAGED keys are not offerable per-account).
    pub preset: crate::settings_catalog::Preset,
}

/// Which family's catalog to serve. Absent = anthropic (Claude Code).
#[derive(serde::Deserialize)]
pub struct SettingsCatalogQuery {
    pub family: Option<String>,
}

/// `GET /accounts/settings-catalog` — serve the per-account settings catalog
/// for `?family=` (anthropic | openai).
/// Read-only, embedded data; no tenant scoping needed (the catalog
/// is the same for everyone and contains no secrets).
pub async fn settings_catalog(
    Query(q): Query<SettingsCatalogQuery>,
) -> Json<SettingsCatalogResponse> {
    let family = q
        .family
        .as_deref()
        .and_then(crate::routes::gateway::Family::from_label)
        .unwrap_or(crate::routes::gateway::Family::Anthropic);
    let c = crate::settings_catalog::for_family(family);
    let mut preset = c.quiet_defaults().clone();
    preset.settings.retain(|name, _| {
        c.key(name).is_some_and(crate::settings_catalog::SettingKey::account_exposable)
    });
    Json(SettingsCatalogResponse {
        keys: c.exposable_keys().cloned().collect(),
        env: c.env_allowlist().to_vec(),
        preset,
    })
}

/// Longest accepted glyph in scalar values: a family ZWJ sequence with skin
/// tones sits well inside this, a pasted sentence does not.
const EMOJI_MAX_SCALARS: usize = 8;

const fn is_emoji_base(c: char) -> bool {
    matches!(c as u32,
        0x203C | 0x2049 | 0x2122 | 0x2139
        | 0x2190..=0x21FF
        | 0x2300..=0x23FF
        | 0x25AA..=0x25FF
        | 0x2600..=0x27BF
        | 0x2934 | 0x2935
        | 0x2B00..=0x2BFF
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x1F000..=0x1FAFF)
}

/// Modifiers that decorate a base without starting a new glyph: variation
/// selectors, skin tones, the keycap enclosure and tag characters.
const fn is_emoji_modifier(c: char) -> bool {
    matches!(c as u32, 0xFE0E | 0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F)
}

const fn is_regional_indicator(c: char) -> bool {
    matches!(c as u32, 0x1F1E6..=0x1F1FF)
}

/// Normalize an account's identity glyph: trimmed, and either a single emoji
/// grapheme or nothing. Accepts ZWJ sequences (👨‍👩‍👧), skin-tone modifiers and
/// two-scalar regional-indicator flags; rejects plain text, two glyphs stuck
/// together, and anything longer than [`EMOJI_MAX_SCALARS`] scalar values.
/// A blank input is [`None`] — "clear it".
fn normalize_emoji(raw: &str) -> Result<Option<String>, &'static str> {
    const BAD: &str = "emoji must be a single emoji character";
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() > EMOJI_MAX_SCALARS {
        return Err(BAD);
    }
    if is_regional_indicator(chars[0]) {
        return if chars.len() == 2 && is_regional_indicator(chars[1]) {
            Ok(Some(trimmed.to_owned()))
        } else {
            Err(BAD)
        };
    }
    let mut want_base = true;
    for &c in &chars {
        if want_base {
            if !is_emoji_base(c) {
                return Err(BAD);
            }
            want_base = false;
        } else if c == '\u{200D}' {
            want_base = true;
        } else if !is_emoji_modifier(c) {
            return Err(BAD);
        }
    }
    if want_base {
        return Err(BAD);
    }
    Ok(Some(trimmed.to_owned()))
}

/// [`normalize_emoji`] as an API-level check.
fn emoji_field(raw: Option<&str>) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    raw.map_or(Ok(None), |v| normalize_emoji(v).map_err(|msg| err(StatusCode::BAD_REQUEST, msg)))
}

/// Validate a pasted `settings_json` blob before persisting it via the
/// settings catalog: only keys tagged `safe`/`care` may be set
/// per-provider; unknown, MANAGED, and SYSTEM keys are rejected. Fail-closed —
/// any violation aborts the whole write.
fn validate_settings_json(
    provider: &str,
    value: &serde_json::Value,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let report = crate::settings_catalog::for_provider(provider).validate_settings(value);
    if report.ok() {
        return Ok(());
    }
    let detail = report.violations.iter().map(|v| v.key.as_str()).collect::<Vec<_>>().join(", ");
    Err(err(
        StatusCode::BAD_REQUEST,
        &format!("settings_json rejected — not settable per-account: {detail}"),
    ))
}

/// Validate the account-level free-form extra-env map before it is
/// encrypted and stored: any well-formed env var name is accepted EXCEPT a
/// denylist of session-critical / gateway-managed vars (values are arbitrary and
/// may be secrets). Fail-closed with a per-name reason.
fn validate_env_json(
    env: &std::collections::HashMap<String, String>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let map: std::collections::BTreeMap<String, String> =
        env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let report = crate::settings_catalog::catalog().validate_free_env(&map);
    if report.ok() {
        return Ok(());
    }
    let detail = report
        .violations
        .iter()
        .map(|v| format!("{}: {}", v.key, v.reason))
        .collect::<Vec<_>>()
        .join("; ");
    Err(err(StatusCode::BAD_REQUEST, &format!("env_json rejected — {detail}")))
}

/// Decrypt a stored `env_json` blob to its var NAMES only (sorted), never the
/// values. A missing/undecryptable/malformed blob yields an empty list
/// — the names are a display convenience, never a hard dependency.
fn env_names_from_enc(enc: Option<&str>, key: &[u8]) -> Vec<String> {
    env_map_from_enc(enc, key).into_keys().collect()
}

/// Decrypt a stored `env_json` blob to its full map, server-side only.
/// Missing/undecryptable/malformed ⇒ empty map.
fn env_map_from_enc(enc: Option<&str>, key: &[u8]) -> std::collections::BTreeMap<String, String> {
    let Some(enc) = enc else { return std::collections::BTreeMap::new() };
    let Some(json) = crate::crypto::decrypt(enc, key) else {
        return std::collections::BTreeMap::new();
    };
    serde_json::from_str(&json).unwrap_or_default()
}

/// Validate + encrypt an extra-env map. Empty ⇒ `None` (clears).
fn encrypt_env(
    env: Option<&std::collections::HashMap<String, String>>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    let Some(map) = env.filter(|m| !m.is_empty()) else { return Ok(None) };
    validate_env_json(map)?;
    let key = crate::crypto::vault_key();
    Ok(Some(crate::crypto::encrypt(&serde_json::to_string(map).unwrap_or_default(), &key)))
}

/// A validated, encrypted provider payload ready to INSERT.
struct ProviderWrite {
    provider: String,
    enc_refresh: Option<String>,
    enc_access: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    base_url: Option<String>,
    auth_scheme: String,
    models: Option<serde_json::Value>,
    model_aliases: Option<serde_json::Value>,
    soft_limits_json: Option<serde_json::Value>,
    settings_json: Option<serde_json::Value>,
    provider_settings: Option<serde_json::Value>,
    rate_limits_json: Option<serde_json::Value>,
}

/// Validate a [`ProviderSpec`] into a [`ProviderWrite`] (shared by the one-shot
/// account create and `POST /accounts/{id}/providers`). Native subscription
/// providers require an OAuth refresh token (`auth_scheme` = oauth); compatible
/// endpoints a base URL + a static credential stored in
/// `encrypted_access_token`, no refresh token, `auth_scheme` = `bearer|api_key`.
// Linear validator: one branch per optional field, no nesting.
#[allow(clippy::too_many_lines)]
fn prepare_provider_write(
    spec: &ProviderSpec,
) -> Result<ProviderWrite, (StatusCode, Json<serde_json::Value>)> {
    let Some(provider) = spec.provider.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
        return Err(err(StatusCode::BAD_REQUEST, "provider required"));
    };
    if !matches!(
        provider,
        "anthropic" | "openai" | "anthropic-compatible" | "openai-compatible" | "fireworks"
    ) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "provider must be anthropic|openai|anthropic-compatible|openai-compatible|fireworks",
        ));
    }
    let fireworks = provider == "fireworks";
    // `fireworks` carries a static `fw_...` bearer like a compatible endpoint,
    // but its upstream is built in — a base URL is an override, not a
    // requirement.
    let compatible = fireworks || matches!(provider, "anthropic-compatible" | "openai-compatible");

    let (enc_refresh, enc_access, base_url, auth_scheme): (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    );
    if compatible {
        let base = spec.base_url.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let base = match base {
            Some(b) => Some(b),
            None if fireworks => None,
            None => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "base_url required for a compatible endpoint",
                ));
            }
        };
        // SSRF is explicitly out of scope for this single-operator, self-hosted
        // deployment; a light scheme check only. Prefer https.
        if let Some(b) = base
            && !(b.starts_with("http://") || b.starts_with("https://"))
        {
            return Err(err(StatusCode::BAD_REQUEST, "base_url must be an http(s) URL"));
        }
        let scheme = spec.auth_scheme.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let scheme = scheme.unwrap_or("bearer");
        if !matches!(scheme, "bearer" | "api_key") {
            return Err(err(StatusCode::BAD_REQUEST, "auth_scheme must be bearer|api_key"));
        }
        // A static credential is optional (an open proxy accepts any value); when
        // absent we still store a dummy so the gateway has a bearer to forward.
        let cred = spec
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("sk-dummy");
        let key = crate::crypto::vault_key();
        enc_refresh = None;
        enc_access = Some(crate::crypto::encrypt(cred, &key));
        base_url = base.map(str::to_owned);
        auth_scheme = scheme.to_owned();
    } else {
        let refresh = spec.refresh_token.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let Some(refresh) = refresh else {
            return Err(err(StatusCode::BAD_REQUEST, "refresh_token required"));
        };
        let key = crate::crypto::vault_key();
        enc_refresh = Some(crate::crypto::encrypt(refresh, &key));
        enc_access = spec.access_token.as_deref().map(|t| crate::crypto::encrypt(t, &key));
        base_url = None;
        auth_scheme = "oauth".to_owned();
    }
    let expires_at = spec.expires_at.and_then(|s| DateTime::<Utc>::from_timestamp(s, 0));
    let models = spec
        .models
        .as_ref()
        .filter(|m| !m.is_empty())
        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
        .or_else(|| fireworks.then(fireworks_default_models));
    // Alias map: an empty map stores NULL (no remapping).
    let model_aliases = spec
        .model_aliases
        .as_ref()
        .filter(|m| !m.is_empty())
        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
    if let Some(s) = spec.settings_json.as_ref().filter(|v| !v.is_null()) {
        validate_settings_json(provider, s)?;
    }
    let settings_json = spec.settings_json.as_ref().filter(|v| !v.is_null()).cloned();
    let provider_settings = match spec.provider_settings.as_ref().filter(|v| !v.is_null()) {
        Some(v) => Some(validate_provider_settings(v)?),
        None => fireworks.then(crate::routes::gateway::fireworks_default_settings),
    };

    Ok(ProviderWrite {
        provider: provider.to_owned(),
        enc_refresh,
        enc_access,
        expires_at,
        base_url,
        auth_scheme,
        models,
        model_aliases,
        soft_limits_json: build_soft_limits_json(
            spec.soft_limits.as_ref(),
            spec.soft_limit_5h_pct,
            spec.soft_limit_7d_pct,
            spec.soft_limit_bypass_5h_minutes.or(spec.soft_limit_bypass_minutes),
            spec.soft_limit_bypass_7d_minutes.or(spec.soft_limit_bypass_minutes),
        )?,
        settings_json,
        provider_settings,
        rate_limits_json: build_rate_limits_json(spec.rate_limits.as_ref())?,
    })
}

/// Gateway settings must be a JSON object — the gateway deep-merges them over
/// the family defaults, and a scalar/array would silently replace the whole
/// blob instead of overriding one knob. `thinking_display` is dropped: the
/// gateway forwards bytes verbatim and cannot apply it.
fn validate_provider_settings(
    v: &serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    v.as_object().map_or_else(
        || Err(err(StatusCode::BAD_REQUEST, "provider_settings must be a JSON object")),
        |obj| {
            let mut obj = obj.clone();
            obj.remove("thinking_display");
            Ok(serde_json::Value::Object(obj))
        },
    )
}

/// INSERT one provider row under an account. Bubbles the raw `sqlx::Error` so
/// callers can map a unique violation on `(account_id, family)` to a 409.
async fn insert_provider(
    conn: &mut sqlx::PgConnection,
    user_id: Uuid,
    account_id: Uuid,
    w: &ProviderWrite,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO account_providers \
            (user_id, account_id, provider, encrypted_refresh_token, encrypted_access_token, \
             expires_at, base_url, models, auth_scheme, model_aliases, \
             soft_limits_json, settings_json, provider_settings, rate_limits_json) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
         RETURNING id",
    )
    .bind(user_id)
    .bind(account_id)
    .bind(&w.provider)
    .bind(&w.enc_refresh)
    .bind(&w.enc_access)
    .bind(w.expires_at)
    .bind(&w.base_url)
    .bind(&w.models)
    .bind(&w.auth_scheme)
    .bind(&w.model_aliases)
    .bind(&w.soft_limits_json)
    .bind(&w.settings_json)
    .bind(&w.provider_settings)
    .bind(&w.rate_limits_json)
    .fetch_one(conn)
    .await
}

/// `GET /api/v1/accounts` — the caller's own accounts with their providers
/// (tokens never returned). Admin sees every account, with the owner's name
/// joined in.
pub async fn list_accounts(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<AccountInfo>>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    let accounts: Vec<AccountRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ACCOUNT_SELECT} WHERE $1::uuid IS NULL OR a.user_id = $1 \
           OR EXISTS (SELECT 1 FROM resource_shares s \
                      WHERE s.resource_type = 'account' AND s.resource_id = a.id \
                        AND s.grantee_id = $1 AND s.revoked_at IS NULL) \
         ORDER BY a.name"
    )))
    .bind(ctx.owner_filter())
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    let providers: Vec<ProviderInfo> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{PROVIDER_SELECT} WHERE $1::uuid IS NULL OR p.user_id = $1 \
           OR EXISTS (SELECT 1 FROM resource_shares s \
                      WHERE s.resource_type = 'account' AND s.resource_id = p.account_id \
                        AND s.grantee_id = $1 AND s.revoked_at IS NULL) \
         ORDER BY p.family"
    )))
    .bind(ctx.owner_filter())
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;

    let mut by_account: HashMap<Uuid, Vec<ProviderInfo>> = HashMap::new();
    for p in providers {
        by_account.entry(p.account_id).or_default().push(p);
    }
    let key = crate::crypto::vault_key();
    let rows = accounts
        .into_iter()
        .map(|a| {
            let providers = by_account.remove(&a.id).unwrap_or_default();
            a.into_info(providers, &key)
        })
        .collect();
    Ok(Json(rows))
}

/// `GET /api/v1/accounts/{id}` — one account with its providers (owner-scoped;
/// admin sees any).
pub async fn get_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountInfo>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    fetch_account_info(&state.pool, id, ctx.owner_filter())
        .await
        .map_err(|e| db_err(&e))?
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such account"))
}

/// `POST /api/v1/accounts` — register an account identity, optionally with its
/// first provider credential in the same call (the legacy one-shot shape: a
/// body carrying `provider` + credential fields).
pub async fn create_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateAccount>,
) -> Result<(StatusCode, Json<AccountInfo>), (StatusCode, Json<serde_json::Value>)> {
    let uid = resolve_owner(&ctx, req.user_id)?;
    if req.name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name required"));
    }
    let emoji = emoji_field(req.emoji.as_deref())?;
    let enc_env = encrypt_env(req.env_json.as_ref())?;
    // Validate the optional provider payload BEFORE creating the identity so a
    // bad credential body doesn't leave an empty account behind.
    let provider_write = if req.provider.provider.is_some() {
        Some(prepare_provider_write(&req.provider)?)
    } else {
        None
    };

    let mut tx = state.pool.begin().await.map_err(|e| db_err(&e))?;
    let account_id: Uuid = match sqlx::query_scalar(
        "INSERT INTO accounts (user_id, name, env_json, emoji) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(uid)
    .bind(req.name.trim())
    .bind(&enc_env)
    .bind(&emoji)
    .fetch_one(&mut *tx)
    .await
    {
        Ok(id) => id,
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            return Err(err(StatusCode::CONFLICT, "an account with that name already exists"));
        }
        Err(e) => return Err(db_err(&e)),
    };
    if let Some(w) = &provider_write
        && let Err(e) = insert_provider(&mut tx, uid, account_id, w).await
    {
        // A fresh account can't collide on (account_id, family); any error here
        // is a genuine DB failure. The tx rollback drops the parent too.
        return Err(db_err(&e));
    }
    tx.commit().await.map_err(|e| db_err(&e))?;

    let info = fetch_account_info(&state.pool, account_id, None)
        .await
        .map_err(|e| db_err(&e))?
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "account vanished after create"))?;
    Ok((StatusCode::CREATED, Json(info)))
}

/// `PATCH /api/v1/accounts/{id}` — rename the identity and/or replace its extra
/// env. Provider fields moved to the provider routes; sending them
/// here 400s with a pointer. Accounts holding a managed provider (the litellm
/// shim) are read-only.
pub async fn update_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateAccount>,
) -> Result<Json<AccountInfo>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    if req.base_url.is_some()
        || req.auth_scheme.is_some()
        || req.models.is_some()
        || req.model_aliases.is_some()
        || req.access_token.is_some()
        || req.soft_limits.is_some()
        || req.settings_json.is_some()
        || req.defaults.is_some()
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "provider fields moved: use PATCH /api/v1/accounts/:id/providers/:provider_id",
        ));
    }
    if let Some(name) = req.name.as_deref()
        && name.trim().is_empty()
    {
        return Err(err(StatusCode::BAD_REQUEST, "name required"));
    }
    let name = req.name.as_deref().map(str::trim).map(str::to_owned);
    if let Some(w) = req.pool_weight
        && !(w.is_finite() && w > 0.0)
    {
        return Err(err(StatusCode::BAD_REQUEST, "pool_weight must be a positive number"));
    }
    // emoji: provided → set (a blank string clears it); absent → unchanged.
    let emoji_provided = req.emoji.is_some();
    let emoji = emoji_field(req.emoji.as_deref())?;
    // env_json: provided → re-encrypt + replace (empty map clears); absent →
    // unchanged. COALESCE can't distinguish those, so carry a provided-flag.
    let (env_provided, enc_env) = if req.env_json.is_none()
        && let Some(remove) = req.env_remove.as_ref().filter(|r| !r.is_empty())
    {
        let stored: Option<Option<String>> = sqlx::query_scalar(
            "SELECT env_json FROM accounts              WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
        )
        .bind(id)
        .bind(ctx.owner_filter())
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| db_err(&e))?;
        let key = crate::crypto::vault_key();
        let mut map = env_map_from_enc(stored.flatten().as_deref(), &key);
        map.retain(|name, _| !remove.contains(name));
        let enc = (!map.is_empty()).then(|| {
            crate::crypto::encrypt(&serde_json::to_string(&map).unwrap_or_default(), &key)
        });
        (true, enc)
    } else {
        (req.env_json.is_some(), encrypt_env(req.env_json.as_ref())?)
    };

    // The owner's pool veto is set on its own: it is a property of lending the
    // account out, not of its credentials, so it stays editable on a managed
    // account (which the identity update below refuses to touch).
    if let Some(eligible) = req.pool_eligible {
        set_pool_eligible(&state, id, ctx.owner_filter(), eligible).await?;
    }
    // Same for the pool weight: a statement about the plan, not the credential.
    if let Some(weight) = req.pool_weight {
        set_pool_weight(&state, id, ctx.owner_filter(), weight).await?;
    }
    if req.pool_eligible.is_some() || req.pool_weight.is_some() {
        // Nothing else to change: don't run the identity update, whose managed
        // guard would 404 an account the pool fields just applied to cleanly.
        if name.is_none() && !env_provided && !emoji_provided {
            let info = fetch_account_info(&state.pool, id, None)
                .await
                .map_err(|e| db_err(&e))?
                .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such account"))?;
            return Ok(Json(info));
        }
    }

    let updated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE accounts SET \
            name = COALESCE($3, name), \
            env_json = CASE WHEN $4 THEN $5 ELSE env_json END, \
            emoji = CASE WHEN $6 THEN $7 ELSE emoji END, \
            updated_at = now() \
         WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2) \
           AND NOT EXISTS (SELECT 1 FROM account_providers p \
                           WHERE p.account_id = accounts.id AND p.managed) \
         RETURNING id",
    )
    .bind(id)
    .bind(ctx.owner_filter())
    .bind(&name)
    .bind(env_provided)
    .bind(&enc_env)
    .bind(emoji_provided)
    .bind(&emoji)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            err(StatusCode::CONFLICT, "an account with that name already exists")
        }
        _ => db_err(&e),
    })?;
    if updated.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    let info = fetch_account_info(&state.pool, id, None)
        .await
        .map_err(|e| db_err(&e))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such account"))?;
    Ok(Json(info))
}

/// Set an account's relative plan size for pool aggregates (owner-scoped, like
/// the veto). Validated by the caller: finite and positive.
async fn set_pool_weight(
    state: &AppState,
    id: Uuid,
    owner: Option<Uuid>,
    weight: f32,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let touched = sqlx::query(
        "UPDATE accounts SET pool_weight = $3, updated_at = now() \
          WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
    )
    .bind(id)
    .bind(owner)
    .bind(weight)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if touched.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    Ok(())
}

/// Set an account's pool veto, and prune the memberships it withdraws.
///
/// Clearing the flag retires the account from every pool it does not belong to
/// by ownership: leaving those rows would let it come back the moment the flag
/// flipped, which is not what "stop lending this out" means. The prune is
/// best-effort — [`crate::store::account_pools::usable_members`] re-checks the
/// flag at every election, so a failed prune costs tidiness, not safety.
async fn set_pool_eligible(
    state: &AppState,
    id: Uuid,
    owner: Option<Uuid>,
    eligible: bool,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let touched = sqlx::query(
        "UPDATE accounts SET pool_eligible = $3, updated_at = now() \
          WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
    )
    .bind(id)
    .bind(owner)
    .bind(eligible)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if touched.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    if !eligible
        && let Err(e) = sqlx::query(
            "DELETE FROM account_pool_members m \
              USING account_pools p, accounts a \
              WHERE m.pool_id = p.id AND a.id = m.account_id \
                AND m.account_id = $1 AND p.user_id <> a.user_id",
        )
        .bind(id)
        .execute(&state.pool)
        .await
    {
        tracing::warn!(account_id = %id, error = %e, "pruning pool memberships failed");
    }
    Ok(())
}

/// `DELETE /api/v1/accounts/{id}` — delete the identity (cascades its providers
/// and their `session_tokens`).
pub async fn delete_account(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    // Admin (`ctx.user_id` = NULL) may delete any account; a user only its own.
    // Accounts holding a managed provider (litellm shim) are read-only.
    let res = sqlx::query(
        "DELETE FROM accounts WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2) \
           AND NOT EXISTS (SELECT 1 FROM account_providers p \
                           WHERE p.account_id = accounts.id AND p.managed)",
    )
    .bind(id)
    .bind(ctx.owner_filter())
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if res.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ----------------------------------------------------------------------------
// Provider routes: add / edit / remove / move one credential under an
// account identity.
// ----------------------------------------------------------------------------

/// `POST /api/v1/accounts/{id}/providers` — attach a provider credential to an
/// existing account (pasted-token / compatible-endpoint path; the OAuth flows
/// attach via `oauth/start`'s `account_id`). 409 if the account already has a
/// provider of that family.
pub async fn add_provider(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<ProviderSpec>,
) -> Result<(StatusCode, Json<ProviderInfo>), (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    let owner = require_account_owner(&state, &ctx, id).await?;
    let w = prepare_provider_write(&req)?;

    let mut conn = state.pool.acquire().await.map_err(|e| db_err(&e))?;
    let pid = match insert_provider(&mut conn, owner, id, &w).await {
        Ok(pid) => pid,
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
            return Err(err(
                StatusCode::CONFLICT,
                "the account already has a provider of that family",
            ));
        }
        Err(e) => return Err(db_err(&e)),
    };
    drop(conn);
    let info = fetch_provider_info(&state.pool, pid)
        .await
        .map_err(|e| db_err(&e))?
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "provider vanished after create"))?;
    Ok((StatusCode::CREATED, Json(info)))
}

/// Whether the payload touches a field only a compatible endpoint has.
/// `models` is deliberately not one: a declared model list is honoured for
/// every provider kind, native subscriptions included.
const fn endpoint_fields_present(req: &UpdateProvider) -> bool {
    req.base_url.is_some() || req.auth_scheme.is_some() || req.access_token.is_some()
}

/// The provider PATCH's single UPDATE. COALESCE keeps each column when its bind
/// is NULL, so an absent field is a no-op; the `CASE WHEN $n` pairs carry an
/// explicit provided-flag for the columns whose "clear" is also NULL. Admin
/// (`ctx.user_id` = NULL) may edit any provider, a user only its own; managed
/// rows are excluded.
const UPDATE_PROVIDER_SQL: &str = "UPDATE account_providers SET \
            base_url = COALESCE($4, base_url), \
            auth_scheme = COALESCE($5, auth_scheme), \
            models = COALESCE($6, models), \
            encrypted_access_token = COALESCE($7, encrypted_access_token), \
            model_aliases = CASE WHEN $8 THEN $9 ELSE model_aliases END, \
            soft_limits_json = CASE WHEN $10 THEN $11 ELSE soft_limits_json END, \
            settings_json = CASE WHEN $12 THEN $13 ELSE settings_json END, \
            provider_settings = CASE WHEN $14 THEN $15 ELSE provider_settings END, \
            rate_limits_json = CASE WHEN $16 THEN $17 ELSE rate_limits_json END, \
            usage_notices = CASE WHEN $18 THEN $19 ELSE usage_notices END, \
            header_pin = COALESCE($20, header_pin) \
         WHERE id = $1 AND account_id = $2 \
           AND ($3::uuid IS NULL OR user_id = $3) AND NOT managed \
         RETURNING id";

/// `PATCH /api/v1/accounts/{id}/providers/{provider_id}` — edit a provider
/// — compatible endpoints may change
/// base URL / auth scheme / credential; the declared model list, aliases, soft
/// limits, and settings are editable for every provider. Managed providers are read-only.
// Linear handler: per-field optional updates built into one dynamic UPDATE.
#[allow(clippy::too_many_lines)]
pub async fn update_provider(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((id, provider_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateProvider>,
) -> Result<Json<ProviderInfo>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    // Resolve the target (scoped to the caller; admin sees all) so we can tell a
    // compatible endpoint from a native one and reject editing managed rows.
    let provider = crate::store::account_providers::provider_owner_scoped(
        &state.pool,
        provider_id,
        id,
        ctx.owner_filter(),
    )
    .await
    .map_err(|e| db_err(&e))?;
    let Some(provider) = provider else {
        return Err(err(StatusCode::NOT_FOUND, "no such provider"));
    };
    let compatible =
        matches!(provider.as_str(), "anthropic-compatible" | "openai-compatible" | "fireworks");

    // Endpoint fields are rejected for native providers so the edit form can't
    // silently no-op against a subscription credential.
    if !compatible && endpoint_fields_present(&req) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "endpoint fields are only editable for a compatible provider",
        ));
    }

    let key = crate::crypto::vault_key();
    // Normalise the optional compatible-endpoint fields. base_url/auth_scheme are
    // validated like create; a blank credential keeps the stored one (NULL bind →
    // COALESCE no-op in SQL). models replaces the list wholesale when provided.
    let base_url = match req.base_url.as_deref().map(str::trim) {
        Some(b) if !b.is_empty() => {
            if !(b.starts_with("http://") || b.starts_with("https://")) {
                return Err(err(StatusCode::BAD_REQUEST, "base_url must be an http(s) URL"));
            }
            Some(b.to_owned())
        }
        _ => None,
    };
    let auth_scheme = match req.auth_scheme.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => {
            if !matches!(s, "bearer" | "api_key") {
                return Err(err(StatusCode::BAD_REQUEST, "auth_scheme must be bearer|api_key"));
            }
            Some(s.to_owned())
        }
        _ => None,
    };
    let models =
        req.models.as_ref().map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
    // Aliases: COALESCE can't distinguish "clear" from "unchanged"
    // (both would bind NULL), so carry an explicit provided-flag — provided +
    // empty clears the column, provided + non-empty replaces it.
    let aliases_provided = req.model_aliases.is_some();
    let model_aliases = req
        .model_aliases
        .as_ref()
        .filter(|m| !m.is_empty())
        .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null));
    let enc_access = req
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|c| crate::crypto::encrypt(c, &key));
    // Soft limits: a provided map replaces the whole JSONB column (an
    // empty object clears it); absent leaves it untouched. Validated before
    // persist. Carry a provided-flag so CASE-WHEN distinguishes clear/unchanged.
    let soft_provided = req.soft_limits.is_some();
    let soft_limits_json =
        build_soft_limits_json(req.soft_limits.as_ref(), None, None, None, None)?;
    // Settings: provided replaces (empty clears), absent untouched;
    // validated against the catalog allowlist before persist.
    let settings_provided = req.settings_json.is_some();
    if let Some(s) = req.settings_json.as_ref().filter(|v| !v.is_null()) {
        validate_settings_json(&provider, s)?;
    }
    let settings_json = req.settings_json.as_ref().filter(|v| !v.is_null()).cloned();
    let gateway_settings_provided = req.provider_settings.is_some();
    let provider_settings = match req.provider_settings.as_ref().filter(|v| !v.is_null()) {
        Some(v) => Some(validate_provider_settings(v)?),
        None => None,
    };
    let rate_limits_provided = req.rate_limits.is_some();
    let rate_limits_json = build_rate_limits_json(req.rate_limits.as_ref())?;
    let usage_notices_provided = req.usage_notices.is_some();
    let usage_notices =
        crate::routes::gateway::usage_notices::UsageNotices::build_json(req.usage_notices.as_ref())
            .map_err(|m| err(StatusCode::BAD_REQUEST, &m))?;

    let updated: Option<Uuid> = sqlx::query_scalar(UPDATE_PROVIDER_SQL)
        .bind(provider_id)
        .bind(id)
        .bind(ctx.owner_filter())
        .bind(&base_url)
        .bind(&auth_scheme)
        .bind(&models)
        .bind(&enc_access)
        .bind(aliases_provided)
        .bind(&model_aliases)
        .bind(soft_provided)
        .bind(&soft_limits_json)
        .bind(settings_provided)
        .bind(&settings_json)
        .bind(gateway_settings_provided)
        .bind(&provider_settings)
        .bind(rate_limits_provided)
        .bind(&rate_limits_json)
        .bind(usage_notices_provided)
        .bind(&usage_notices)
        .bind(req.header_pin)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| db_err(&e))?;
    if updated.is_none() {
        return Err(err(StatusCode::NOT_FOUND, "no such provider"));
    }
    if soft_provided {
        let caps = crate::soft_limit::SoftLimits::from_json(soft_limits_json.as_ref());
        reevaluate_soft_limit_block(&state, provider_id, &caps).await;
    }
    let info = fetch_provider_info(&state.pool, provider_id)
        .await
        .map_err(|e| db_err(&e))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such provider"))?;
    Ok(Json(info))
}

/// Sessions bound to `provider_id` that currently carry a soft-limit block. The
/// session row owns the block, so a block another replica set is visible here.
async fn soft_limit_blocked_sessions(
    pool: &sqlx::PgPool,
    provider_id: Uuid,
) -> Result<Vec<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT DISTINCT s.id, s.soft_limit_key FROM sessions s \
         JOIN session_tokens t ON t.session_id = s.id AND t.revoked_at IS NULL \
         WHERE t.account_id = $1 AND s.soft_limit_reason IS NOT NULL",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await
}

/// Blocked sessions among `candidates` whose own block `caps` now lifts.
///
/// Each candidate is judged against the limit that blocked it, so a session over
/// its own `session_usd` budget stays blocked however high the account cap goes.
/// A row from before the key column carries `None` and keeps the old
/// whole-config reading. Clear-only (no re-block); pure so it is unit-testable.
fn soft_limit_blocks_to_clear(
    candidates: &[(String, Option<String>)],
    windows: &[crate::soft_limit::UsageWindow],
    caps: &crate::soft_limit::SoftLimits,
    now: DateTime<Utc>,
) -> Vec<String> {
    let all_allow = matches!(
        crate::soft_limit::evaluate_soft_limit(windows, caps, None, now),
        crate::soft_limit::Decision::Allow
    );
    candidates
        .iter()
        .filter(|(_, key)| {
            key.as_ref()
                .map_or(all_allow, |k| crate::soft_limit::block_lifted_by(k, windows, caps, now))
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// After a provider's soft-limit config is raised, lift the blocks it holds that
/// are now under cap. Best-effort: any DB/usage error is swallowed so
/// the surrounding PATCH still succeeds.
async fn reevaluate_soft_limit_block(
    state: &AppState,
    provider_id: Uuid,
    caps: &crate::soft_limit::SoftLimits,
) {
    let candidates = match soft_limit_blocked_sessions(&state.pool, provider_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(%provider_id, error = %e, "soft-limit re-eval: session lookup failed");
            return;
        }
    };
    if candidates.is_empty() {
        return;
    }
    let usage = gateway::usage_for_soft_limit(state, provider_id).await;
    let windows =
        usage.as_ref().map(crate::soft_limit::normalize_usage_windows).unwrap_or_default();
    let to_clear = soft_limit_blocks_to_clear(&candidates, &windows, caps, Utc::now());
    for session_id in to_clear {
        gateway::clear_soft_limit_block(state, &session_id).await;
    }
}

/// `DELETE /api/v1/accounts/{id}/providers/{provider_id}` — remove one provider
/// credential (cascades its `session_tokens`). The identity and its other
/// providers stay.
pub async fn delete_provider(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((id, provider_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    let removed = crate::store::account_providers::delete_owner_scoped(
        &state.pool,
        provider_id,
        id,
        ctx.owner_filter(),
    )
    .await
    .map_err(|e| db_err(&e))?;
    if removed == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such provider"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/v1/accounts/{id}/providers/{provider_id}/move` — re-parent a
/// provider onto another account of the SAME owner — manual merge for e.g.
/// "alice (anthropic)" + "alice (openai)" → one "alice". 409 if the target
/// already has a provider of that family.
pub async fn move_provider(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((id, provider_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<MoveProvider>,
) -> Result<Json<ProviderInfo>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    let src_owner = require_account_owner(&state, &ctx, id).await?;
    let tgt_owner = require_account_owner(&state, &ctx, req.target_account_id)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "no such target account"))?;
    // Same-owner only: shares confer `use`, never re-homing a credential onto
    // another user's identity (which would also flip whose sessions bill it).
    if src_owner != tgt_owner {
        return Err(err(
            StatusCode::FORBIDDEN,
            "provider can only move between accounts of the same owner",
        ));
    }
    let moved = sqlx::query(
        "UPDATE account_providers SET account_id = $1 \
         WHERE id = $2 AND account_id = $3 AND NOT managed",
    )
    .bind(req.target_account_id)
    .bind(provider_id)
    .bind(id)
    .execute(&state.pool)
    .await;
    match moved {
        Ok(done) if done.rows_affected() == 0 => {
            Err(err(StatusCode::NOT_FOUND, "no such provider"))
        }
        Ok(_) => {
            let info = fetch_provider_info(&state.pool, provider_id)
                .await
                .map_err(|e| db_err(&e))?
                .ok_or_else(|| err(StatusCode::NOT_FOUND, "no such provider"))?;
            Ok(Json(info))
        }
        Err(sqlx::Error::Database(db)) if db.is_unique_violation() => Err(err(
            StatusCode::CONFLICT,
            "the target account already has a provider of that family",
        )),
        Err(e) => Err(db_err(&e)),
    }
}

// ----------------------------------------------------------------------------
// "Sign in with Claude" / "Sign in with ChatGPT" OAuth authorize flow
//
// OAuth 2.1 authorization-code + PKCE in manual paste mode: we generate a PKCE
// verifier/challenge and a nonce, the user authorizes upstream, and pastes the
// result back. Anthropic (claude.ai, `claude /login` / better-ccflare) displays
// a `code#state` pair the user copies. Codex's public client has a FIXED
// localhost:1455 redirect we can't change, so the browser redirect fails to
// load and the user copies the full callback URL from the address bar; we parse
// the `code` out of it. Token exchange differs per provider (anthropic JSON,
// openai form-encoded) and Codex's id_token carries the chatgpt_account_id we
// persist for the gateway's upstream header. Pending logins live in memory
// only, keyed by nonce + scoped to the authenticated user, single-use,
// TTL-bounded. `start` may carry an `account_id` attach target so the
// finished credential lands as a provider under an existing account.
// ----------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct OAuthStart {
    /// `anthropic` ("Sign in with Claude") or `openai` ("Sign in with `ChatGPT`").
    pub provider: String,
    /// Owning user — required (and only honoured) when authenticated with the
    /// admin token. Ignored when `account_id` names the attach target
    /// (the target's owner wins).
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Optional attach target: finish the flow as a provider under
    /// this existing account instead of creating a new identity.
    #[serde(default)]
    pub account_id: Option<Uuid>,
}

#[derive(Debug, serde::Serialize)]
pub struct OAuthStartResponse {
    pub nonce: String,
    pub authorize_url: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct OAuthFinish {
    pub nonce: String,
    /// New-account name. Required unless the flow was started with an
    /// `account_id` attach target (then ignored).
    #[serde(default)]
    pub name: Option<String>,
    /// anthropic: the `code#state` pair pasted from claude.ai (the `#state`
    /// suffix is optional). Either this or `callback_url` must be present.
    #[serde(default)]
    pub code: Option<String>,
    /// openai/Codex: the full `http://localhost:1455/auth/callback?code=…&state=…`
    /// URL the user copies from the browser address bar after the redirect fails
    /// to load (the fixed redirect can't reach cctui).
    #[serde(default)]
    pub callback_url: Option<String>,
}

#[derive(serde::Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    /// OpenAI/Codex returns an OIDC `id_token` whose claims carry the
    /// `chatgpt_account_id` we need for the upstream header.
    #[serde(default)]
    id_token: Option<String>,
}

/// Extract `chatgpt_account_id` from an `OpenAI` `id_token` JWT without verifying
/// the signature (the token came straight from the trusted token endpoint over
/// TLS). The claim is nested under `https://api.openai.com/auth`.
fn chatgpt_account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// Parse the `code` out of an `OpenAI` callback URL (or a bare `code`/`code#state`
/// string). Accepts the full `http://localhost:1455/auth/callback?code=…&state=…`
/// the user pastes, or just the code itself.
fn code_from_callback(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some(qpos) = input.find('?') {
        for pair in input[qpos + 1..].split('&') {
            if let Some(code) = pair.strip_prefix("code=") {
                let code = code.split('#').next().unwrap_or(code);
                return Some(urldecode(code));
            }
        }
        return None;
    }
    // Bare value: strip any `#state` suffix and use it verbatim.
    Some(input.split('#').next().unwrap_or(input).to_string())
}

/// Minimal percent-decoding for the `code` query param.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// PKCE S256 challenge for a verifier: base64url(sha256(verifier)), no padding.
fn account_name_missing(name: Option<&str>) -> bool {
    name.is_none_or(|s| s.trim().is_empty())
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Split a pasted `code#state` into its parts. claude.ai displays the pair
/// joined by `#`; some clients omit the state, so it is optional.
fn split_code_state(pasted: &str) -> (String, Option<String>) {
    let pasted = pasted.trim();
    match pasted.split_once('#') {
        Some((code, state)) => (code.to_string(), Some(state.to_string())),
        None => (pasted.to_string(), None),
    }
}

/// Drop pending logins older than the TTL (lazy sweep on access).
fn sweep_expired(store: &PendingOAuthLogins) {
    let cutoff = Utc::now() - PENDING_OAUTH_TTL;
    store.retain(|_, v| v.created_at > cutoff);
}

/// `POST /api/v1/accounts/oauth/start` — begin a "Sign in with Claude" login.
/// Generates PKCE + a nonce, stashes a pending record, and returns the
/// authorize URL for the webui to open in a new tab. With `account_id`
/// the finish attaches to that existing account.
pub async fn oauth_start(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<OAuthStart>,
) -> Result<Json<OAuthStartResponse>, (StatusCode, Json<serde_json::Value>)> {
    // The attach target names its owner; otherwise the caller does.
    let uid = if let Some(account_id) = req.account_id {
        require_human(&ctx)?;
        let owner: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM accounts WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
        )
        .bind(account_id)
        .bind(ctx.owner_filter())
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| db_err(&e))?;
        owner.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such account"))?
    } else {
        resolve_owner(&ctx, req.user_id)?
    };
    if !matches!(req.provider.as_str(), "anthropic" | "openai") {
        return Err(err(StatusCode::BAD_REQUEST, "provider must be anthropic|openai"));
    }

    sweep_expired(&state.pending_oauth_logins);

    // PKCE verifier (high-entropy, URL-safe) doubles as the OAuth `state`, same
    // as better-ccflare. nonce keys the pending record (distinct from state so
    // the client never has to handle the verifier).
    let code_verifier = crate::auth::mint_secret();
    let nonce = crate::auth::mint_secret();
    let challenge = pkce_challenge(&code_verifier);

    let authorize_url = if req.provider == "openai" {
        // "Sign in with ChatGPT": auth.openai.com authorize with the codex
        // public client. The redirect is fixed to localhost:1455 (can't be
        // changed), so the browser redirect fails to load and the user pastes
        // the full callback URL back to us.
        format!(
            "{}?response_type=code&client_id={}&redirect_uri={}\
             &scope=openid%20profile%20email%20offline_access\
             &code_challenge={}&code_challenge_method=S256&state={}\
             &id_token_add_organizations=true&codex_cli_simplified_flow=true&prompt=login",
            gateway::openai_authorize_url(),
            urlencoding(&gateway::openai_client_id()),
            urlencoding(&gateway::openai_oauth_redirect_uri()),
            urlencoding(&challenge),
            urlencoding(&code_verifier),
        )
    } else {
        format!(
            "{}?code=true&client_id={}&response_type=code&redirect_uri={}\
             &scope=org:create_api_key%20user:profile%20user:inference\
             &code_challenge={}&code_challenge_method=S256&state={}",
            gateway::anthropic_authorize_url(),
            urlencoding(&gateway::anthropic_client_id()),
            urlencoding(&gateway::anthropic_oauth_redirect_uri()),
            urlencoding(&challenge),
            urlencoding(&code_verifier),
        )
    };

    state.pending_oauth_logins.insert(
        nonce.clone(),
        PendingOAuthLogin {
            user_id: uid,
            provider: req.provider,
            code_verifier,
            created_at: Utc::now(),
            account_id: req.account_id,
        },
    );

    Ok(Json(OAuthStartResponse { nonce, authorize_url }))
}

/// `POST /api/v1/accounts/oauth/finish` — exchange the pasted `code#state` for
/// tokens and store the credential. Single-use: the pending record is consumed
/// regardless of exchange outcome. Lands as a provider under the `start`
/// attach target when one was given; otherwise finds-or-creates an account
/// identity by `name` (re-running the flow for an existing name refreshes the
/// same-family credential in place — the Reauthenticate button).
// Linear handler: consume pending record, exchange code, store per provider.
#[allow(clippy::too_many_lines)]
pub async fn oauth_finish(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<OAuthFinish>,
) -> Result<(StatusCode, Json<AccountInfo>), (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;

    sweep_expired(&state.pending_oauth_logins);

    // Consume the pending record (single-use), but only if it belongs to the
    // caller — never let one user finish another user's login. Admin started
    // the flow on behalf of the owner stored in the record, so it may finish
    // any pending login. The account lands on the stored owner.
    let pending = match state.pending_oauth_logins.get(&req.nonce) {
        Some(p) if ctx.is_admin() || ctx.user_id == p.user_id => p.clone(),
        _ => return Err(err(StatusCode::BAD_REQUEST, "unknown or expired login")),
    };
    let uid = pending.user_id;
    state.pending_oauth_logins.remove(&req.nonce);

    // Resolve the target identity BEFORE the token exchange so a bad target
    // fails fast: an attach target must still exist and belong to the pending
    // owner; otherwise `name` finds-or-creates an identity after the exchange.
    let attach_target = if let Some(account_id) = pending.account_id {
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM accounts WHERE id = $1 AND user_id = $2")
                .bind(account_id)
                .bind(uid)
                .fetch_optional(&state.pool)
                .await
                .map_err(|e| db_err(&e))?;
        if owner.is_none() {
            return Err(err(StatusCode::NOT_FOUND, "no such account"));
        }
        Some(account_id)
    } else {
        if account_name_missing(req.name.as_deref()) {
            return Err(err(StatusCode::BAD_REQUEST, "name required"));
        }
        None
    };

    // The token exchange differs per provider: anthropic posts JSON with the
    // pasted `code#state`; openai/Codex posts a form-encoded body with the code
    // extracted from the pasted callback URL.
    let resp = if pending.provider == "openai" {
        let raw = req
            .callback_url
            .as_deref()
            .or(req.code.as_deref())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "callback_url required"))?;
        let code = code_from_callback(raw)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "could not find code in callback URL"))?;

        let form = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", &gateway::openai_client_id()),
            ("redirect_uri", &gateway::openai_oauth_redirect_uri()),
            ("code_verifier", pending.code_verifier.as_str()),
        ];
        state.http_client.post(gateway::openai_token_url()).form(&form).send().await
    } else {
        let raw =
            req.code.as_deref().ok_or_else(|| err(StatusCode::BAD_REQUEST, "code required"))?;
        let (code, state_part) = split_code_state(raw);
        if code.is_empty() {
            return Err(err(StatusCode::BAD_REQUEST, "code required"));
        }
        // claude.ai sends `code#state`; the state must equal the verifier we issued.
        let oauth_state = state_part.unwrap_or_else(|| pending.code_verifier.clone());
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": oauth_state,
            "client_id": gateway::anthropic_client_id(),
            "redirect_uri": gateway::anthropic_oauth_redirect_uri(),
            "code_verifier": pending.code_verifier,
        });
        state.http_client.post(gateway::anthropic_token_url()).json(&body).send().await
    };

    let resp = resp.map_err(|e| {
        tracing::error!("oauth token exchange transport error: {e}");
        err(StatusCode::BAD_GATEWAY, "token exchange failed")
    })?;
    if !resp.status().is_success() {
        let status = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        tracing::error!(%status, "oauth token exchange rejected: {detail}");
        return Err(err(
            StatusCode::BAD_REQUEST,
            "token exchange rejected — check the pasted code/URL",
        ));
    }
    let tok: OAuthTokenResponse = resp.json().await.map_err(|e| {
        tracing::error!("oauth token exchange decode error: {e}");
        err(StatusCode::BAD_GATEWAY, "token exchange decode failed")
    })?;

    // For Codex, pull the chatgpt account id out of the id_token so the gateway
    // can send the `Chatgpt-Account-Id` header upstream.
    let provider_account_id = tok
        .id_token
        .as_deref()
        .and_then(chatgpt_account_id_from_id_token)
        .filter(|_| pending.provider == "openai");

    let key = crate::crypto::vault_key();
    let enc_refresh = crate::crypto::encrypt(&tok.refresh_token, &key);
    let enc_access = crate::crypto::encrypt(&tok.access_token, &key);
    let expires_at = tok.expires_in.map(|s| Utc::now() + Duration::seconds(s));

    // Resolve the identity: the attach target, or find-or-create by name.
    let account_id: Uuid = if let Some(aid) = attach_target {
        aid
    } else {
        // Safe unwrap-ish: validated non-empty above when attach_target is None.
        let name = req.name.as_deref().map(str::trim).unwrap_or_default();
        sqlx::query_scalar(
            "INSERT INTO accounts (user_id, name) VALUES ($1, $2) \
             ON CONFLICT (user_id, name) DO UPDATE SET updated_at = now() \
             RETURNING id",
        )
        .bind(uid)
        .bind(name)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| db_err(&e))?
    };

    // Upsert on (account_id, family): a first login inserts; re-running the flow
    // against the same account (the Reauthenticate button) refreshes the
    // same-family credential in place and clears any `needs_reauth` flag, instead
    // of 409ing on the unique index.
    let pid: Result<Uuid, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO account_providers \
            (user_id, account_id, provider, encrypted_refresh_token, encrypted_access_token, \
             expires_at, provider_account_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (account_id, family) DO UPDATE SET \
             provider                 = EXCLUDED.provider, \
             encrypted_refresh_token  = EXCLUDED.encrypted_refresh_token, \
             encrypted_access_token   = EXCLUDED.encrypted_access_token, \
             expires_at               = EXCLUDED.expires_at, \
             provider_account_id      = EXCLUDED.provider_account_id, \
             needs_reauth = false, last_auth_error = NULL, last_auth_error_at = NULL \
         RETURNING id",
    )
    .bind(uid)
    .bind(account_id)
    .bind(&pending.provider)
    .bind(&enc_refresh)
    .bind(&enc_access)
    .bind(expires_at)
    .bind(&provider_account_id)
    .fetch_one(&state.pool)
    .await;

    match pid {
        Ok(pid) => {
            // Fresh credentials → drop the in-memory reauth gate too, so
            // the gateway's success path doesn't think it still needs clearing.
            state.account_reauth.remove(&pid);
            let info = fetch_account_info(&state.pool, account_id, None)
                .await
                .map_err(|e| db_err(&e))?
                .ok_or_else(|| {
                    err(StatusCode::INTERNAL_SERVER_ERROR, "account vanished after login")
                })?;
            Ok((StatusCode::CREATED, Json(info)))
        }
        Err(e) => Err(db_err(&e)),
    }
}

/// Minimal percent-encoding for query-string components (no extra deps): encode
/// everything outside the RFC 3986 unreserved set.
fn urlencoding(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// How long a cached usage fetch is served before we re-hit upstream.
/// Anthropic's usage endpoint rate-limits per access token (safe at ~180s); we
/// cache for a few minutes so a viewed accounts page + slow background poll never
/// spams it, and many clients share one entry per account.
pub const USAGE_CACHE_TTL: Duration = Duration::minutes(3);

/// Usage windows surfaced per provider credential. `usage` mirrors
/// Anthropic's free OAuth usage payload (`five_hour`/`seven_day` utilization +
/// reset timestamps); `None` means the provider has no usage API (Codex) or the
/// credential has no active windows — the webui hides the indicator in that
/// case. `account_id` is the provider-row id (the legacy field name is the
/// API contract).
#[derive(Debug, serde::Serialize)]
pub struct AccountUsage {
    pub account_id: Uuid,
    pub provider: String,
    /// Raw upstream usage JSON (passed through verbatim) or `null`.
    pub usage: Option<serde_json::Value>,
    /// Normalized, provider-agnostic usage windows: the collection the
    /// UI renders and the soft-limit evaluator gates on. Empty ⇒ no supported
    /// windows in the latest response (distinct from a fetch error).
    pub windows: Vec<UsageWindowView>,
    /// Seconds since this usage was fetched upstream (0 = just now). Lets the UI
    /// show staleness; values refresh on the slow cache TTL, not per request.
    pub age_secs: u64,
    /// Whether a usage-limit reset can be claimed right now (Codex reset
    /// credits, Claude `juniper_tide`); `None` when the payload has no such block.
    pub limit_reset: Option<crate::routes::limit_reset::LimitResetStatus>,
}

/// A normalized window plus its pace against the window's linear budget
/// (`None` when the window has no reset time or no known length).
#[derive(Debug, serde::Serialize)]
pub struct UsageWindowView {
    #[serde(flatten)]
    pub window: crate::soft_limit::UsageWindow,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace: Option<crate::pace::Pace>,
}

impl UsageWindowView {
    /// Rate `window` now, on the slope from `previous` when there is one and
    /// on the window average otherwise.
    fn now(window: crate::soft_limit::UsageWindow, previous: Option<crate::pace::Sample>) -> Self {
        let pace = crate::pace::for_window(Utc::now(), &window, previous);
        Self { window, pace }
    }
}

/// The earlier readings a provider's windows are rated against, or none when
/// the lookup fails: a slope refines the pace, it is never a precondition for
/// serving the gauge.
async fn previous_samples(
    state: &AppState,
    provider_id: Uuid,
    usage: Option<&serde_json::Value>,
) -> Vec<crate::store::usage_samples::PreviousSample> {
    let Some(usage) = usage else {
        return Vec::new();
    };
    let windows = crate::soft_limit::normalize_usage_windows(usage);
    match crate::store::usage_samples::previous_for(&state.pool, provider_id, &windows, Utc::now())
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(provider_id = %provider_id, error = %e, "reading usage samples failed");
            Vec::new()
        }
    }
}

impl AccountUsage {
    /// Assemble the view; `previous` carries at most one earlier sample per
    /// window key (see [`previous_samples`]).
    fn build(
        account_id: Uuid,
        provider: String,
        usage: Option<serde_json::Value>,
        age_secs: u64,
        previous: &[crate::store::usage_samples::PreviousSample],
    ) -> Self {
        let windows = usage
            .as_ref()
            .map(crate::soft_limit::normalize_usage_windows)
            .unwrap_or_default()
            .into_iter()
            .map(|w| {
                let prev = previous
                    .iter()
                    .find(|p| p.window_key == w.key)
                    .map(crate::store::usage_samples::PreviousSample::sample);
                UsageWindowView::now(w, prev)
            })
            .collect();
        let limit_reset = usage
            .as_ref()
            .and_then(|u| crate::routes::limit_reset::limit_reset_status(&provider, u));
        Self { account_id, provider, usage, windows, age_secs, limit_reset }
    }
}

/// Cache a fresh usage fetch and push it to every client.
///
/// Only ever reached after an upstream fetch that already happened, so the push
/// adds no upstream calls: a caller served from a fresh cache returns before
/// this and broadcasts nothing, which is what bounds pushes to one per account
/// per [`USAGE_CACHE_TTL`] and keeps the event from becoming a refresh storm.
/// An empty `provider` means the account no longer resolves: still cached, but
/// not published, since there is no row shape to patch a client cache with.
pub async fn store_and_broadcast_usage(
    state: &AppState,
    id: Uuid,
    provider: String,
    usage: Option<serde_json::Value>,
) -> AccountUsage {
    state.account_usage_cache.insert(
        id,
        crate::state::CachedUsage { fetched_at: std::time::Instant::now(), usage: usage.clone() },
    );
    let previous = previous_samples(state, id, usage.as_ref()).await;
    let row = AccountUsage::build(id, provider, usage, 0, &previous);
    if row.provider.is_empty() {
        return row;
    }
    if let Ok(usage) = serde_json::to_value(&row) {
        state
            .bus
            .publish_server(cctui_proto::ws::ServerEvent::AccountUsage { account_id: id, usage });
    }
    row
}

/// `GET /api/v1/accounts/{id}/usage` — current subscription usage for a
/// provider credential. `{id}` is the provider-row id (the pre-
/// account id — migrated rows share the uuid, so old callers keep working).
/// Free + tokenless: for anthropic providers this hits Anthropic's OAuth usage
/// endpoint (5h/7d window utilization), served from a slow-refresh per-provider
/// cache so we never spam the rate-limited upstream. OpenAI/codex providers
/// have no such API, so the 5h/7d windows are metered locally from recorded
/// token usage — same shape, same cache, same UI chip.
/// Ownership: a user may only read their own providers; admin may read any.
pub async fn account_usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountUsage>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    // Authorize + resolve provider in one go. Admin (`ctx.user_id` = NULL) may
    // read any provider; a user only its own.
    let provider: Option<String> = sqlx::query_scalar(
        "SELECT provider FROM account_providers \
         WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
    )
    .bind(id)
    .bind(ctx.owner_filter())
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    let Some(provider) = provider else {
        return Err(err(StatusCode::NOT_FOUND, "no such account"));
    };

    // Serve a fresh-enough cached value without touching upstream.
    if let Some(hit) = state.account_usage_cache.get(&id)
        && hit.fetched_at.elapsed() < USAGE_CACHE_TTL.to_std().unwrap_or_default()
    {
        let age_secs = hit.fetched_at.elapsed().as_secs();
        let usage = hit.usage.clone();
        drop(hit);
        let previous = previous_samples(&state, id, usage.as_ref()).await;
        return Ok(Json(AccountUsage::build(id, provider, usage, age_secs, &previous)));
    }

    // Stale or absent → fetch upstream (anthropic only; Codex returns None).
    let fetched = gateway::fetch_account_usage(&state, id).await;
    let usage = if let Ok(u) = fetched {
        gateway::record_usage_samples(&state, id, u.as_ref());
        u
    } else {
        // Upstream hiccup (e.g. 429/refresh fail): fall back to the last
        // cached value if we have one rather than erroring the whole row.
        let cached = state
            .account_usage_cache
            .get(&id)
            .map(|hit| (hit.usage.clone(), hit.fetched_at.elapsed().as_secs()));
        if let Some((usage, age_secs)) = cached {
            let previous = previous_samples(&state, id, usage.as_ref()).await;
            return Ok(Json(AccountUsage::build(id, provider, usage, age_secs, &previous)));
        }
        // No prior value — surface as "no usage" so the UI just hides the chip.
        None
    };
    Ok(Json(store_and_broadcast_usage(&state, id, provider, usage).await))
}

/// One row of `GET /api/v1/accounts/usage`: a provider credential's usage plus
/// the account it hangs off, so a caller can group without a second request.
#[derive(Debug, serde::Serialize)]
pub struct AccountUsageEntry {
    #[serde(flatten)]
    pub usage: AccountUsage,
    pub account: Uuid,
    pub account_name: String,
    /// The account's identity glyph, so the header can group without a second request.
    pub account_emoji: Option<String>,
    /// Whether this credential's gauge shows in the header strip.
    pub header_pin: bool,
}

#[derive(sqlx::FromRow)]
struct UsageProviderRow {
    id: Uuid,
    provider: String,
    account_id: Uuid,
    account_name: String,
    account_emoji: Option<String>,
    header_pin: bool,
}

/// `GET /api/v1/accounts/usage` — usage windows of every provider credential
/// the caller owns (admin: all), in one round trip. Each row is served by the
/// same per-provider cache as the single-provider route, refreshed upstream at
/// most once per [`USAGE_CACHE_TTL`]; a provider whose fetch fails and has no
/// cached value reports empty windows rather than failing the whole list.
pub async fn all_accounts_usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<AccountUsageEntry>>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    let rows: Vec<UsageProviderRow> = sqlx::query_as(
        "SELECT p.id, p.provider, p.account_id, p.header_pin, a.name AS account_name, a.emoji AS account_emoji          FROM account_providers p JOIN accounts a ON a.id = p.account_id          WHERE ($1::uuid IS NULL OR p.user_id = $1)            AND p.provider IN ('anthropic', 'openai', 'fireworks')          ORDER BY a.name, p.family",
    )
    .bind(ctx.owner_filter())
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;

    let fetches = rows.iter().map(|r| gateway::usage_for_soft_limit(&state, r.id));
    let usages = futures_util::future::join_all(fetches).await;
    let previous = futures_util::future::join_all(
        rows.iter().zip(&usages).map(|(r, usage)| previous_samples(&state, r.id, usage.as_ref())),
    )
    .await;
    let out = rows
        .into_iter()
        .zip(usages)
        .zip(previous)
        .map(|((r, usage), previous)| {
            let age_secs = state
                .account_usage_cache
                .get(&r.id)
                .map_or(0, |hit| hit.fetched_at.elapsed().as_secs());
            AccountUsageEntry {
                usage: AccountUsage::build(r.id, r.provider, usage, age_secs, &previous),
                account: r.account_id,
                account_name: r.account_name,
                account_emoji: r.account_emoji,
                header_pin: r.header_pin,
            }
        })
        .collect();
    Ok(Json(out))
}

// ----------------------------------------------------------------------------
// Account sharing management
//
// `account_shares` is the sharing seam: a live grant row lets
// a NON-owner resolve/use an account on the gateway + dispatch path, without
// transferring ownership. Grants key on the account IDENTITY: sharing
// an account shares all its provider credentials. Owner-scoped: only the
// account's owner (or an admin) may manage its shares — a grant confers `use`,
// never share management.
// ----------------------------------------------------------------------------

/// API view of one live share grant on an account. Safe to return —
/// no secrets; just who the account is shared with and since when.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct ShareInfo {
    pub account_id: Uuid,
    pub user_id: Uuid,
    /// The grantee's login (`users.name`), joined for display.
    pub user_name: String,
    pub action: String,
    pub granted_at: DateTime<Utc>,
}

/// `POST /api/v1/accounts/{id}/shares` payload. `user` is the grantee,
/// accepted as either a UUID or a login (`users.name`) so an operator can grant
/// by whichever they have. `action` defaults to `use` (the only action today).
#[derive(Debug, serde::Deserialize)]
pub struct GrantShare {
    pub user: String,
    #[serde(default)]
    pub action: Option<String>,
}

/// Confirm the caller owns the account identity (admin sees any), scoped like
/// the other account mutations via `owner_filter()`; returns the owner's id.
/// Returns 404 (not 403) when the caller isn't the owner so an account id's
/// existence never leaks. Share management is owner-only — a share grant does
/// NOT confer the right to manage shares.
async fn require_account_owner(
    state: &AppState,
    ctx: &AuthContext,
    id: Uuid,
) -> Result<Uuid, (StatusCode, Json<serde_json::Value>)> {
    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM accounts \
         WHERE id = $1 AND ($2::uuid IS NULL OR user_id = $2)",
    )
    .bind(id)
    .bind(ctx.owner_filter())
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    owner.ok_or_else(|| err(StatusCode::NOT_FOUND, "no such account"))
}

/// `GET /api/v1/accounts/{id}/shares` — who the account is shared with (owner-
/// scoped). Lists only live grants (`revoked_at IS NULL`).
pub async fn list_shares(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ShareInfo>>, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    require_account_owner(&state, &ctx, id).await?;
    let rows: Vec<ShareInfo> = sqlx::query_as(
        "SELECT s.resource_id AS account_id, s.grantee_id AS user_id, u.name AS user_name, \
                s.action, s.granted_at \
         FROM resource_shares s JOIN users u ON u.id = s.grantee_id \
         WHERE s.resource_type = 'account' AND s.resource_id = $1 AND s.revoked_at IS NULL \
         ORDER BY u.name",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    Ok(Json(rows))
}

/// `POST /api/v1/accounts/{id}/shares` — grant `use` to another user (owner-
/// scoped). `user` is a UUID or login. Idempotent: re-granting a previously
/// revoked share un-revokes it in place rather than 409ing on the primary key.
pub async fn grant_share(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<GrantShare>,
) -> Result<(StatusCode, Json<ShareInfo>), (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    require_account_owner(&state, &ctx, id).await?;

    // Only `use` today (schema default); reject anything else so a typo doesn't
    // silently store a dead action that no code path honours.
    let action = req.action.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("use");
    if action != "use" {
        return Err(err(StatusCode::BAD_REQUEST, "action must be 'use'"));
    }

    // Resolve the grantee by UUID or login (`users.name`), active users only.
    let ident = req.user.trim();
    if ident.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "user required"));
    }
    let target: Option<Uuid> = Uuid::parse_str(ident)
        .map_or_else(
            |_| {
                sqlx::query_scalar("SELECT id FROM users WHERE name = $1 AND revoked_at IS NULL")
                    .bind(ident)
            },
            |uuid| {
                sqlx::query_scalar("SELECT id FROM users WHERE id = $1 AND revoked_at IS NULL")
                    .bind(uuid)
            },
        )
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| db_err(&e))?;
    let Some(target) = target else {
        return Err(err(StatusCode::NOT_FOUND, "no such user"));
    };

    sqlx::query(
        "INSERT INTO resource_shares (resource_type, resource_id, grantee_id, action) \
         VALUES ('account', $1, $2, $3) \
         ON CONFLICT (resource_type, resource_id, grantee_id, action) \
         DO UPDATE SET revoked_at = NULL, granted_at = now()",
    )
    .bind(id)
    .bind(target)
    .bind(action)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;

    let info: ShareInfo = sqlx::query_as(
        "SELECT s.resource_id AS account_id, s.grantee_id AS user_id, u.name AS user_name, \
                s.action, s.granted_at \
         FROM resource_shares s JOIN users u ON u.id = s.grantee_id \
         WHERE s.resource_type = 'account' AND s.resource_id = $1 \
           AND s.grantee_id = $2 AND s.action = $3",
    )
    .bind(id)
    .bind(target)
    .bind(action)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    Ok((StatusCode::CREATED, Json(info)))
}

/// `DELETE /api/v1/accounts/{id}/shares/{user_id}` — revoke a grant (owner-
/// scoped) by setting `revoked_at`. Idempotent-ish: 404 if there was no live
/// share to revoke.
pub async fn revoke_share(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    require_human(&ctx)?;
    require_account_owner(&state, &ctx, id).await?;
    let res = sqlx::query(
        "UPDATE resource_shares SET revoked_at = now() \
         WHERE resource_type = 'account' AND resource_id = $1 \
           AND grantee_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user_id)
    .execute(&state.pool)
    .await
    .map_err(|e| db_err(&e))?;
    if res.rows_affected() == 0 {
        return Err(err(StatusCode::NOT_FOUND, "no such share"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_token_totals_stay_correlated_to_one_provider() {
        assert!(
            PROVIDER_SELECT.contains("WHERE st.account_id = p.id ) t ON true"),
            "the token-total subquery must stay correlated to p.id"
        );
        assert!(
            !PROVIDER_SELECT.contains("GROUP BY st.account_id"),
            "a grouped subquery aggregates every provider's rows on a single-id lookup"
        );
    }

    #[test]
    fn provider_settings_drop_the_inert_thinking_display() {
        let v = serde_json::json!({ "thinking_display": "summarized", "other": 1 });
        assert_eq!(validate_provider_settings(&v).unwrap(), serde_json::json!({ "other": 1 }));
        assert!(validate_provider_settings(&serde_json::json!("x")).is_err());
    }

    fn soft_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-19T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn hot_usage() -> serde_json::Value {
        serde_json::json!({
            "five_hour": { "utilization": 90.0, "resets_at": "2026-06-19T16:00:00Z" },
            "seven_day": { "utilization": 10.0, "resets_at": "2026-06-26T00:00:00Z" },
        })
    }

    fn update_provider(json: serde_json::Value) -> UpdateProvider {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn a_declared_model_list_is_not_an_endpoint_field() {
        let models = update_provider(serde_json::json!({
            "models": [{"model": "claude-opus-5", "label": "Opus 5"}]
        }));
        assert!(!endpoint_fields_present(&models));
        assert!(!endpoint_fields_present(&update_provider(serde_json::json!({"models": []}))));
        assert!(endpoint_fields_present(&update_provider(
            serde_json::json!({"base_url": "https://x"})
        )));
        assert!(endpoint_fields_present(&update_provider(
            serde_json::json!({"auth_scheme": "bearer"})
        )));
        assert!(endpoint_fields_present(&update_provider(
            serde_json::json!({"access_token": "sk-x"})
        )));
    }

    #[test]
    fn usage_window_view_flattens_and_carries_pace() {
        let resets = (Utc::now() + Duration::hours(2)).to_rfc3339();
        let usage = serde_json::json!({
            "five_hour": { "utilization": 60.0, "resets_at": resets },
        });
        let built = AccountUsage::build(Uuid::nil(), "anthropic".into(), Some(usage), 7, &[]);
        let json = serde_json::to_value(&built).unwrap();
        let w = &json["windows"][0];
        assert_eq!(w["key"], "session");
        assert_eq!(w["utilization"], 60.0);
        let pace = &w["pace"];
        assert!(pace["ratio"].as_f64().unwrap() > 0.0);
        assert!(pace["expected_pct"].as_f64().unwrap() > 50.0);
        assert!(pace["projected_wall_at"].is_string());
        assert_eq!(json["age_secs"], 7);
    }

    /// The push has to be interchangeable with a poll response: the client
    /// patches its query cache with it instead of refetching, so any field the
    /// route carries and the event drops would silently blank in the UI.
    #[test]
    fn a_usage_push_carries_the_same_row_shape_as_the_route() {
        let resets = (Utc::now() + Duration::hours(2)).to_rfc3339();
        let usage = serde_json::json!({
            "five_hour": { "utilization": 60.0, "resets_at": resets },
        });
        let row = AccountUsage::build(Uuid::nil(), "anthropic".into(), Some(usage), 0, &[]);
        let payload = serde_json::to_value(&row).expect("row serializes");
        let event =
            cctui_proto::ws::ServerEvent::AccountUsage { account_id: Uuid::nil(), usage: payload };
        let json = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(json["type"], "account_usage");
        assert_eq!(json["account_id"], serde_json::json!(Uuid::nil()));
        for field in ["account_id", "provider", "usage", "windows", "age_secs"] {
            assert!(json["usage"].get(field).is_some(), "push must carry {field}");
        }
        assert_eq!(json["usage"]["windows"][0]["utilization"], 60.0);
        assert_eq!(json["usage"]["age_secs"], 0, "a push is by definition just-fetched");
    }

    #[test]
    fn a_pace_cap_persists_on_percent_windows_and_is_range_checked() {
        let built = build_soft_limits_json(
            Some(&serde_json::json!({
                "session": {"pace_cap": 1.5},
                "usd_5h": {"cap_usd": 2.0, "pace_cap": 1.5},
            })),
            None,
            None,
            None,
            None,
        )
        .expect("valid")
        .expect("non-empty");
        assert_eq!(built["session"]["pace_cap"], 1.5);
        assert!(
            built["usd_5h"].get("pace_cap").is_none(),
            "a dollar window reports no utilization, so a pace cap there is unenforceable"
        );
        for bad in [0.5, 0.0, 1000.0] {
            assert!(
                build_soft_limits_json(
                    Some(&serde_json::json!({ "session": {"pace_cap": bad} })),
                    None,
                    None,
                    None,
                    None,
                )
                .is_err(),
                "pace_cap {bad} must be rejected"
            );
        }
    }

    #[test]
    fn raising_cap_over_usage_clears_every_blocked_session() {
        let caps = crate::soft_limit::SoftLimits::from_json(Some(&serde_json::json!({
            "session": {"cap_pct": 95}
        })));
        // The candidates come from `soft_limit_reason IS NOT NULL`, so a block
        // this process never saw (other replica, or set before a rollout) clears.
        let candidates = vec![
            ("s-other-replica".to_owned(), Some("session".to_owned())),
            ("s-pre-rollout".to_owned(), None),
        ];
        let windows = crate::soft_limit::normalize_usage_windows(&hot_usage());
        let cleared = soft_limit_blocks_to_clear(&candidates, &windows, &caps, soft_now());
        assert_eq!(cleared, ["s-other-replica", "s-pre-rollout"]);
    }

    #[test]
    fn still_over_new_cap_clears_nothing() {
        let caps = crate::soft_limit::SoftLimits::from_json(Some(&serde_json::json!({
            "session": {"cap_pct": 85}
        })));
        let windows = crate::soft_limit::normalize_usage_windows(&hot_usage());
        let cleared = soft_limit_blocks_to_clear(
            &[("s-blocked".to_owned(), Some("session".to_owned()))],
            &windows,
            &caps,
            soft_now(),
        );
        assert!(cleared.is_empty());
    }

    /// CCT-962, both directions: the account cap only lifts the blocks it owns.
    #[test]
    fn raising_an_account_cap_spares_a_session_over_its_own_budget() {
        let caps = crate::soft_limit::SoftLimits::from_json(Some(&serde_json::json!({
            "session": {"cap_pct": 95}
        })));
        let mut windows = crate::soft_limit::normalize_usage_windows(&hot_usage());
        windows.push(crate::soft_limit::usd_window(crate::soft_limit::KEY_SESSION_USD, 5.0, None));
        let over_budget = format!(
            "{}{}",
            crate::soft_limit::SESSION_SCOPE_PREFIX,
            crate::soft_limit::KEY_SESSION_USD
        );
        let candidates = vec![
            ("s-own-budget".to_owned(), Some(over_budget)),
            ("s-account-cap".to_owned(), Some("session".to_owned())),
        ];
        let cleared = soft_limit_blocks_to_clear(&candidates, &windows, &caps, soft_now());
        assert_eq!(
            cleared,
            ["s-account-cap"],
            "the account cap clears its own block and never the session budget's"
        );
    }

    /// DB-gated: the re-evaluation picks its candidates from the session rows,
    /// so raising a cap lifts a block set by the other replica (or before a
    /// rollout) that this process has no memory of.
    #[tokio::test]
    async fn blocked_candidates_come_from_the_session_rows() {
        let Some(url) =
            crate::routes::gateway::test_db_url("blocked_candidates_come_from_the_session_rows")
        else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = Uuid::new_v4();
        let acct = Uuid::new_v4();
        let prov = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("reeval-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
            .bind(acct)
            .bind(uid)
            .bind(format!("reeval-acct-{uid}"))
            .execute(&pool)
            .await
            .expect("seed account");
        sqlx::query(
            "INSERT INTO account_providers \
                 (id, user_id, provider, encrypted_refresh_token, account_id) \
             VALUES ($1, $2, 'anthropic', 'x', $3)",
        )
        .bind(prov)
        .bind(uid)
        .bind(acct)
        .execute(&pool)
        .await
        .expect("seed provider");

        let blocked = format!("reeval-blocked-{uid}");
        let running = format!("reeval-running-{uid}");
        for (id, reason) in
            [(&blocked, Some("switch account: personal rate-limited")), (&running, None)]
        {
            sqlx::query(
                "INSERT INTO sessions (id, machine_id, working_dir, user_id, status, soft_limit_reason) \
                 VALUES ($1, 'm1', '/w', $2, 'active', $3)",
            )
            .bind(id)
            .bind(uid)
            .bind(reason)
            .execute(&pool)
            .await
            .expect("seed session");
            sqlx::query(
                "INSERT INTO session_tokens (token_hash, session_id, account_id) \
                 VALUES ($1, $2, $3)",
            )
            .bind(format!("th-{id}"))
            .bind(id)
            .bind(prov)
            .execute(&pool)
            .await
            .expect("seed token");
        }

        let candidates = soft_limit_blocked_sessions(&pool, prov).await.expect("candidates");
        assert_eq!(candidates, vec![(blocked.clone(), None)]);

        sqlx::query("DELETE FROM session_tokens WHERE account_id = $1")
            .bind(prov)
            .execute(&pool)
            .await
            .expect("cleanup tokens");
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .expect("cleanup sessions");
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(uid)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[test]
    fn accepts_a_single_emoji_grapheme() {
        for one in [
            "\u{1F419}",
            "\u{2764}\u{FE0F}",
            "\u{1F44D}\u{1F3FD}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F1EB}\u{1F1F7}",
            "\u{2600}",
        ] {
            assert_eq!(normalize_emoji(one), Ok(Some(one.to_string())), "{one} should be accepted");
        }
        assert_eq!(normalize_emoji("  \u{1F419}  "), Ok(Some("\u{1F419}".to_string())));
    }

    #[test]
    fn blank_emoji_clears_the_glyph() {
        assert_eq!(normalize_emoji(""), Ok(None));
        assert_eq!(normalize_emoji("   "), Ok(None));
    }

    #[test]
    fn rejects_text_and_multiple_glyphs() {
        for bad in [
            "\u{1F419}\u{1F419}",
            "hi",
            "a",
            "\u{1F419} \u{1F419}",
            "\u{1F1EB}\u{1F1F7}\u{1F1EB}\u{1F1F7}",
            "\u{1F419}\u{200D}",
            "\u{1F419}x",
            "\u{1F419}\u{1F419}\u{1F419}\u{1F419}\u{1F419}\u{1F419}\u{1F419}\u{1F419}\u{1F419}",
        ] {
            assert!(normalize_emoji(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    /// The PATCH's own UPDATE against a migrated database: a provider is pinned
    /// to the header by default, `header_pin: false` unpins it, and an absent
    /// field leaves the column alone.
    #[tokio::test]
    async fn header_pin_round_trips_through_the_provider_patch() {
        let Some(url) = crate::routes::gateway::test_db_url("header_pin_round_trips") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let suffix = Uuid::new_v4();
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (id, name, key_hash) \
             VALUES (gen_random_uuid(), $1, gen_random_uuid()::text) RETURNING id",
        )
        .bind(format!("cct922-{suffix}"))
        .fetch_one(&pool)
        .await
        .expect("insert user");
        let account: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (user_id, name) VALUES ($1, $2) RETURNING id")
                .bind(user)
                .bind(format!("cct922-{suffix}"))
                .fetch_one(&pool)
                .await
                .expect("insert account");
        let provider: Uuid = sqlx::query_scalar(
            "INSERT INTO account_providers (user_id, account_id, provider) \
             VALUES ($1, $2, 'anthropic') RETURNING id",
        )
        .bind(user)
        .bind(account)
        .fetch_one(&pool)
        .await
        .expect("insert provider");

        let patch = |header_pin: Option<bool>| {
            let pool = pool.clone();
            async move {
                let updated: Option<Uuid> = sqlx::query_scalar(UPDATE_PROVIDER_SQL)
                    .bind(provider)
                    .bind(account)
                    .bind(Some(user))
                    .bind(None::<String>)
                    .bind(None::<String>)
                    .bind(None::<serde_json::Value>)
                    .bind(None::<String>)
                    .bind(false)
                    .bind(None::<serde_json::Value>)
                    .bind(false)
                    .bind(None::<serde_json::Value>)
                    .bind(false)
                    .bind(None::<serde_json::Value>)
                    .bind(false)
                    .bind(None::<serde_json::Value>)
                    .bind(false)
                    .bind(None::<serde_json::Value>)
                    .bind(false)
                    .bind(None::<serde_json::Value>)
                    .bind(header_pin)
                    .fetch_optional(&pool)
                    .await
                    .expect("patch provider");
                assert_eq!(updated, Some(provider));
                fetch_provider_info(&pool, provider)
                    .await
                    .expect("fetch provider")
                    .expect("provider exists")
                    .header_pin
            }
        };

        assert!(patch(None).await, "a new credential is pinned to the header");
        assert!(!patch(Some(false)).await, "header_pin: false unpins it");
        assert!(!patch(None).await, "an absent header_pin leaves the column unchanged");
        assert!(patch(Some(true)).await, "header_pin: true pins it again");

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user)
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[test]
    fn missing_or_blank_account_name_is_rejected() {
        assert!(account_name_missing(None));
        assert!(account_name_missing(Some("")));
        assert!(account_name_missing(Some("   \t")));
        assert!(!account_name_missing(Some(" work ")));
    }

    #[test]
    fn splits_code_and_state() {
        let (code, state) = split_code_state("abc123#xyz789");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz789"));
    }

    #[test]
    fn splits_code_without_state() {
        let (code, state) = split_code_state("  abc123  ");
        assert_eq!(code, "abc123");
        assert_eq!(state, None);
    }

    #[test]
    fn pkce_challenge_is_url_safe_b64_of_sha256() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(pkce_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn sweep_drops_only_expired() {
        let store: PendingOAuthLogins = Arc::new(DashMap::new());
        store.insert(
            "fresh".into(),
            PendingOAuthLogin {
                user_id: Uuid::new_v4(),
                provider: "anthropic".into(),
                code_verifier: "v".into(),
                created_at: Utc::now(),
                account_id: None,
            },
        );
        store.insert(
            "stale".into(),
            PendingOAuthLogin {
                user_id: Uuid::new_v4(),
                provider: "anthropic".into(),
                code_verifier: "v".into(),
                created_at: Utc::now() - Duration::minutes(20),
                account_id: None,
            },
        );
        sweep_expired(&store);
        assert!(store.contains_key("fresh"));
        assert!(!store.contains_key("stale"));
    }

    #[test]
    fn extracts_code_from_callback_url() {
        assert_eq!(
            code_from_callback("http://localhost:1455/auth/callback?code=ABC123&state=xyz")
                .as_deref(),
            Some("ABC123")
        );
        // code may be url-encoded and carry a #state suffix.
        assert_eq!(
            code_from_callback("http://localhost:1455/auth/callback?code=a%2Bb%3Dc#st").as_deref(),
            Some("a+b=c")
        );
        // bare code (no URL).
        assert_eq!(code_from_callback("  rawcode#state ").as_deref(), Some("rawcode"));
        assert_eq!(code_from_callback(""), None);
        // query present but no code param.
        assert_eq!(code_from_callback("http://localhost:1455/auth/callback?state=x"), None);
    }

    #[test]
    fn parses_chatgpt_account_id_from_id_token() {
        // header.payload.signature — only the payload (claims) is read.
        let payload = serde_json::json!({
            "sub": "user-1",
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-abc-123" }
        });
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let jwt = format!("header.{payload_b64}.sig");
        assert_eq!(chatgpt_account_id_from_id_token(&jwt).as_deref(), Some("acct-abc-123"));
        // missing claim → None.
        let empty = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        assert_eq!(chatgpt_account_id_from_id_token(&format!("h.{empty}.s")), None);
        assert_eq!(chatgpt_account_id_from_id_token("not-a-jwt"), None);
    }

    #[test]
    fn encodes_query_components() {
        assert_eq!(
            urlencoding("org:create_api_key user:profile"),
            "org%3Acreate_api_key%20user%3Aprofile"
        );
        assert_eq!(
            urlencoding("https://console.anthropic.com/oauth/code/callback"),
            "https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback"
        );
    }

    #[test]
    fn create_account_body_flattens_provider_spec() {
        // Legacy one-shot shape still deserializes: provider fields flat.
        let body: CreateAccount =
            serde_json::from_str(r#"{"name":"work","provider":"anthropic","refresh_token":"rt"}"#)
                .unwrap();
        assert_eq!(body.provider.provider.as_deref(), Some("anthropic"));
        assert_eq!(body.provider.refresh_token.as_deref(), Some("rt"));
        // Identity-only create: no provider block.
        let body: CreateAccount = serde_json::from_str(r#"{"name":"work"}"#).unwrap();
        assert!(body.provider.provider.is_none());
    }

    #[test]
    fn free_form_env_json_denylist_and_acceptance() {
        // Free-form names accepted.
        let mut ok = std::collections::HashMap::new();
        ok.insert("MY_TOKEN".to_string(), "secret".to_string());
        ok.insert("HTTP_PROXY".to_string(), "http://p".to_string());
        assert!(validate_env_json(&ok).is_ok());

        // Denylisted gateway/session vars rejected with a clear per-name reason.
        for denied in ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_BG_CLAIM_AUTH"] {
            let mut bad = std::collections::HashMap::new();
            bad.insert(denied.to_string(), "x".to_string());
            let e = validate_env_json(&bad).expect_err("denylisted name must be rejected");
            assert_eq!(e.0, StatusCode::BAD_REQUEST);
        }

        // Malformed name rejected.
        let mut malformed = std::collections::HashMap::new();
        malformed.insert("1BAD".to_string(), "x".to_string());
        assert!(validate_env_json(&malformed).is_err());
    }

    #[test]
    fn env_names_round_trip_names_only_sorted() {
        // A fixed key exercises the encrypt→decrypt-names path without a vault env.
        let key = b"test-key-32-bytes-test-key-32byt".to_vec();
        let mut map = std::collections::BTreeMap::new();
        map.insert("ZED".to_string(), "z".to_string());
        map.insert("ALPHA".to_string(), "a".to_string());
        let enc = crate::crypto::encrypt(&serde_json::to_string(&map).unwrap(), &key);

        let names = env_names_from_enc(Some(&enc), &key);
        assert_eq!(names, vec!["ALPHA".to_string(), "ZED".to_string()], "sorted names only");
        // No blob ⇒ empty; a garbage blob degrades to empty, never panics.
        assert!(env_names_from_enc(None, &key).is_empty());
        assert!(env_names_from_enc(Some("not-hex-zz"), &key).is_empty());
    }

    #[test]
    fn env_remove_drops_names_and_reencrypts() {
        let key = b"test-key-32-bytes-test-key-32byt".to_vec();
        let mut map = std::collections::BTreeMap::new();
        map.insert("KEEP".to_string(), "k".to_string());
        map.insert("DROP".to_string(), "d".to_string());
        let enc = crate::crypto::encrypt(&serde_json::to_string(&map).unwrap(), &key);

        let mut decrypted = env_map_from_enc(Some(&enc), &key);
        decrypted.retain(|name, _| name != "DROP");
        assert_eq!(decrypted.get("KEEP").map(String::as_str), Some("k"), "values survive");
        let reenc = crate::crypto::encrypt(&serde_json::to_string(&decrypted).unwrap(), &key);
        assert_eq!(env_names_from_enc(Some(&reenc), &key), vec!["KEEP".to_string()]);

        decrypted.retain(|name, _| name != "KEEP");
        assert!(decrypted.is_empty(), "removing every name clears the blob");
    }

    #[test]
    fn provider_write_validates_provider() {
        let spec = ProviderSpec { provider: Some("bogus".into()), ..Default::default() };
        assert!(prepare_provider_write(&spec).is_err());
        let spec = ProviderSpec::default();
        assert!(prepare_provider_write(&spec).is_err());
        // native without refresh token → 400.
        let spec = ProviderSpec { provider: Some("anthropic".into()), ..Default::default() };
        assert!(prepare_provider_write(&spec).is_err());
    }

    #[test]
    fn rate_limits_json_validates_and_clears() {
        let out =
            build_rate_limits_json(Some(&serde_json::json!({ "rpm": 30, "tpm": 90_000 }))).unwrap();
        assert_eq!(out, Some(serde_json::json!({ "rpm": 30, "tpm": 90_000 })));

        // Absent, null, empty object, and zeros all clear the column.
        assert_eq!(build_rate_limits_json(None).unwrap(), None);
        assert_eq!(build_rate_limits_json(Some(&serde_json::Value::Null)).unwrap(), None);
        assert_eq!(build_rate_limits_json(Some(&serde_json::json!({}))).unwrap(), None);
        assert_eq!(
            build_rate_limits_json(Some(&serde_json::json!({ "rpm": 0, "tpm": 0 }))).unwrap(),
            None
        );

        assert_eq!(
            build_rate_limits_json(Some(&serde_json::json!({ "rpm": 5 }))).unwrap(),
            Some(serde_json::json!({ "rpm": 5 }))
        );

        for bad in [
            serde_json::json!({ "rpm": -1 }),
            serde_json::json!({ "tpm": 1.5 }),
            serde_json::json!([1, 2]),
        ] {
            let e = build_rate_limits_json(Some(&bad)).expect_err("invalid rate_limits must 400");
            assert_eq!(e.0, StatusCode::BAD_REQUEST);
        }
    }
}
