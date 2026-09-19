use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use sqlx::PgPool;
use uuid::Uuid;

/// Cache TTL for positive auth lookups. Bounds revocation latency.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// The deliberately small scope set. Split later if needed; can't
/// easily un-split. `admin` implies the cross-user god-view; the rest are
/// capability flags intersected per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Read,
    Dispatch,
    Enroll,
    Admin,
}

impl Scope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Dispatch => "dispatch",
            Self::Enroll => "enroll",
            Self::Admin => "admin",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "dispatch" => Some(Self::Dispatch),
            "enroll" => Some(Self::Enroll),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    /// Every scope, for the seeded admin ceiling/grant and for building UI lists.
    #[must_use]
    pub const fn all() -> [Self; 4] {
        [Self::Read, Self::Dispatch, Self::Enroll, Self::Admin]
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resolved identity for one authenticated request. Everyone is a real user:
/// `user_id` is always present, admin is just a user holding the
/// `admin` scope. `scopes` is the effective set = `key_acls` ∩ `user_acls`.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub user_id: Uuid,
    /// The resolved key's id. Carried for auditing / future per-key routing;
    /// not every consumer reads it.
    #[allow(dead_code)]
    pub key_id: Uuid,
    /// Set when the resolved key is a machine key, so the archive/skills
    /// machine paths keep working. `None` for user/dispatcher/admin keys.
    pub machine_id: Option<Uuid>,
    pub scopes: BTreeSet<Scope>,
}

impl AuthContext {
    #[must_use]
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    #[must_use]
    pub fn is_admin(&self) -> bool {
        self.has(Scope::Admin)
    }

    /// Require a scope; 403 otherwise. The single guard all routes collapse to.
    pub fn requires(&self, scope: Scope) -> Result<(), StatusCode> {
        if self.has(scope) { Ok(()) } else { Err(StatusCode::FORBIDDEN) }
    }

    /// The uniform owner FILTER for self-scoped list/search/stats queries — NOT
    /// a magic value. It returns:
    ///   * `None` for an admin — the filter is **disabled**, so the query sees
    ///     every row (the god-view);
    ///   * `Some(user_id)` for everyone else — the filter scopes rows to the
    ///     caller.
    ///
    /// Routes bind it into the existing `WHERE $1::uuid IS NULL OR user_id = $1`
    /// predicate: a `NULL` bind (admin) makes the predicate always true; a
    /// `Some` bind narrows to the caller's rows. This expresses row filtering,
    /// which the per-object `Resource` guard (a yes/no gate) cannot — so
    /// list/batch endpoints keep using this rather than the guard.
    #[must_use]
    pub fn owner_filter(&self) -> Option<Uuid> {
        if self.is_admin() { None } else { Some(self.user_id) }
    }
}

#[derive(Clone)]
struct CacheEntry {
    ctx: AuthContext,
    expires: Instant,
}

#[derive(Clone)]
pub struct AuthConfig {
    pub admin_tokens: Vec<String>,
    pub pool: PgPool,
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
}

impl AuthConfig {
    pub fn new(admin_tokens: Vec<String>, pool: PgPool) -> Self {
        Self { admin_tokens, pool, cache: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Resolve `CCTUI_ADMIN_TOKENS` to a seeded admin **user** with `{admin}`
    /// ceiling, and an `auth_keys` row (with `{admin}` grant) per env token, so
    /// the break-glass token is a real identity — never a `user_id=None` ghost.
    /// Idempotent: re-run safe on every startup. Best-effort — a
    /// transient DB error is logged, never fatal, because env-token validation
    /// also still works through the cheap env short-circuit in `validate()`.
    pub async fn seed_admin(&self) {
        if self.admin_tokens.is_empty() {
            return;
        }
        let admin_user_id = Uuid::nil(); // stable, reserved id for the seeded admin
        if let Err(e) = sqlx::query(
            "INSERT INTO users (id, name, key_hash) VALUES ($1, 'admin', $2) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(admin_user_id)
        // A sentinel key_hash that can never match a real sha256 (different
        // length) so the legacy users.key_hash path never resolves to it.
        .bind("seeded-admin-no-legacy-hash")
        .execute(&self.pool)
        .await
        {
            tracing::warn!("seed_admin: create user failed: {e}");
            return;
        }
        for scope in Scope::all() {
            let _ = sqlx::query(
                "INSERT INTO user_acls (user_id, scope) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(admin_user_id)
            .bind(scope.as_str())
            .execute(&self.pool)
            .await;
        }
        for token in &self.admin_tokens {
            let hash = sha256_hex(token);
            let key_id: Option<(Uuid,)> = sqlx::query_as(
                "INSERT INTO auth_keys (user_id, key_hash, label, kind) \
                 VALUES ($1, $2, 'admin (env)', 'admin') \
                 ON CONFLICT (key_hash) DO UPDATE SET label = EXCLUDED.label \
                 RETURNING id",
            )
            .bind(admin_user_id)
            .bind(&hash)
            .fetch_optional(&self.pool)
            .await
            .unwrap_or(None);
            if let Some((kid,)) = key_id {
                for scope in Scope::all() {
                    let _ = sqlx::query(
                        "INSERT INTO key_acls (key_id, scope) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                    )
                    .bind(kid)
                    .bind(scope.as_str())
                    .execute(&self.pool)
                    .await;
                }
            }
        }
        tracing::info!(count = self.admin_tokens.len(), "seeded admin user + env keys");
    }

    /// Purge a cached entry by its key hash. Call after revoke/rotate/scope-edit
    /// so the change takes effect immediately rather than after TTL.
    pub fn purge(&self, key_hash: &str) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(key_hash);
        }
    }

    /// Drop the entire cache. Used after an ACL edit (user or key scope change)
    /// since we don't track which token hashes a given key/user owns in cache;
    /// the cache repopulates within one request and TTL is short anyway.
    pub fn purge_all(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    fn cache_get(&self, hash: &str) -> Option<AuthContext> {
        let mut cache = self.cache.lock().ok()?;
        if let Some(entry) = cache.get(hash).cloned() {
            if entry.expires > Instant::now() {
                return Some(entry.ctx);
            }
            cache.remove(hash);
        }
        None
    }

    fn cache_put(&self, hash: String, ctx: AuthContext) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(hash, CacheEntry { ctx, expires: Instant::now() + CACHE_TTL });
        }
    }

    /// Load the effective scopes for a (`key_id`, `user_id`) pair = `key_acls` ∩
    /// `user_acls`. Re-intersected on every (cache-miss) auth so a demotion of
    /// the user's ceiling immediately limits the key (the drift-killer).
    async fn effective_scopes(&self, key_id: Uuid, user_id: Uuid) -> BTreeSet<Scope> {
        let grant: Vec<(String,)> = sqlx::query_as("SELECT scope FROM key_acls WHERE key_id = $1")
            .bind(key_id)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        let ceiling: Vec<(String,)> =
            sqlx::query_as("SELECT scope FROM user_acls WHERE user_id = $1")
                .bind(user_id)
                .fetch_all(&self.pool)
                .await
                .unwrap_or_default();
        let ceiling: BTreeSet<Scope> = ceiling.iter().filter_map(|(s,)| Scope::parse(s)).collect();
        grant.iter().filter_map(|(s,)| Scope::parse(s)).filter(|s| ceiling.contains(s)).collect()
    }

    pub async fn validate(&self, token: &str) -> Option<AuthContext> {
        let hash = sha256_hex(token);
        if let Some(ctx) = self.cache_get(&hash) {
            return Some(ctx);
        }

        // --- New unified path: auth_keys + ACLs. Tried first. ---
        if let Some(ctx) = self.validate_api_key(&hash).await {
            self.cache_put(hash, ctx.clone());
            return Some(ctx);
        }

        // --- Dual-read fallback to the legacy tables (transparency window). ---
        // No token is invalidated mid-cutover: a credential that hasn't been
        // backfilled (or a freshly-rotated legacy hash) still resolves here,
        // with scopes synthesized from its owner's ceiling.
        if let Some(ctx) = self.validate_legacy(&hash).await {
            self.cache_put(hash, ctx.clone());
            return Some(ctx);
        }

        // Last resort: an env admin token whose seeded auth_keys row hasn't
        // landed yet (DB hiccup at startup). Resolve to the seeded admin user.
        if self.admin_tokens.iter().any(|t| t == token) {
            return Some(AuthContext {
                user_id: Uuid::nil(),
                key_id: Uuid::nil(),
                machine_id: None,
                scopes: Scope::all().into_iter().collect(),
            });
        }

        None
    }

    /// Re-derive, from the database as it is now, the context a request
    /// authenticated earlier as (`user_id`, `key_id`) would get today. For work
    /// accepted then run later (a queued spawn): `None` once the key or its
    /// user has been revoked, disabled or has expired, and scopes re-intersected
    /// from the current ACLs, so a demotion takes effect. Same rules as
    /// [`Self::validate`], by key identity instead of token hash.
    ///
    /// The env admin fallback (`nil`/`nil`, no seeded row) stays valid only
    /// while an env admin token is configured.
    pub async fn revalidate(
        &self,
        user_id: Uuid,
        key_id: Uuid,
    ) -> Result<Option<AuthContext>, sqlx::Error> {
        if user_id.is_nil() && key_id.is_nil() {
            return Ok((!self.admin_tokens.is_empty()).then(|| AuthContext {
                user_id,
                key_id,
                machine_id: None,
                scopes: Scope::all().into_iter().collect(),
            }));
        }
        // Unified keys. A key that exists there but is no longer live is final:
        // no legacy fallback for it.
        let unified: Option<(Option<Uuid>, bool)> = sqlx::query_as(
            "SELECT k.machine_id, (k.revoked_at IS NULL \
                 AND (k.expires_at IS NULL OR k.expires_at > now()) \
                 AND u.revoked_at IS NULL AND u.disabled_at IS NULL) \
             FROM auth_keys k JOIN users u ON u.id = k.user_id \
             WHERE k.id = $1 AND k.user_id = $2",
        )
        .bind(key_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some((machine_id, live)) = unified {
            if !live {
                return Ok(None);
            }
            let scopes = self.effective_scopes(key_id, user_id).await;
            return Ok(Some(AuthContext { user_id, key_id, machine_id, scopes }));
        }
        // Legacy identities, as `validate_legacy` synthesizes them: a machine
        // key (key id = machine id), a user key (key id = user id), a user token.
        let legacy: Option<(Option<Uuid>,)> = sqlx::query_as(
            "SELECT m.id FROM machines m JOIN users u ON u.id = m.user_id \
             WHERE m.id = $1 AND m.user_id = $2 AND m.revoked_at IS NULL \
               AND u.revoked_at IS NULL AND u.disabled_at IS NULL \
             UNION ALL \
             SELECT NULL::uuid FROM users u WHERE u.id = $2 AND $1 = $2 \
               AND u.revoked_at IS NULL AND u.disabled_at IS NULL \
             UNION ALL \
             SELECT NULL::uuid FROM user_tokens t JOIN users u ON u.id = t.user_id \
             WHERE t.id = $1 AND t.user_id = $2 AND t.revoked_at IS NULL \
               AND (t.expires_at IS NULL OR t.expires_at > now()) \
               AND u.revoked_at IS NULL AND u.disabled_at IS NULL \
             LIMIT 1",
        )
        .bind(key_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((machine_id,)) = legacy else { return Ok(None) };
        let scopes = self.ceiling_scopes(user_id).await;
        Ok(Some(AuthContext { user_id, key_id, machine_id, scopes }))
    }

    /// Resolve a token hash against the unified `auth_keys` table, gating on the
    /// owning user being live (revoked/disabled cascades) and the key itself
    /// being live (not revoked/expired). Scopes = `key_acls` ∩ `user_acls`.
    async fn validate_api_key(&self, hash: &str) -> Option<AuthContext> {
        #[derive(sqlx::FromRow)]
        struct KeyRow {
            id: Uuid,
            user_id: Uuid,
            machine_id: Option<Uuid>,
        }
        let row = sqlx::query_as::<_, KeyRow>(
            "SELECT k.id, k.user_id, k.machine_id FROM auth_keys k \
             JOIN users u ON u.id = k.user_id \
             WHERE k.key_hash = $1 \
             AND k.revoked_at IS NULL \
             AND (k.expires_at IS NULL OR k.expires_at > now()) \
             AND u.revoked_at IS NULL AND u.disabled_at IS NULL",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None)?;
        let KeyRow { id: key_id, user_id, machine_id } = row;
        let scopes = self.effective_scopes(key_id, user_id).await;
        self.touch_key(key_id);
        if let Some(mid) = machine_id {
            self.touch_machine(mid);
        }
        Some(AuthContext { user_id, key_id, machine_id, scopes })
    }

    /// Legacy resolution (`machines.key_hash`, `users.key_hash`, `user_tokens`).
    /// Synthesizes a key identity from the legacy row and scopes from the
    /// owner's ceiling so a not-yet-backfilled token behaves identically.
    async fn validate_legacy(&self, hash: &str) -> Option<AuthContext> {
        // Machine key.
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT m.id, m.user_id FROM machines m \
             JOIN users u ON u.id = m.user_id \
             WHERE m.key_hash = $1 AND m.revoked_at IS NULL \
             AND u.revoked_at IS NULL AND u.disabled_at IS NULL",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);
        if let Some((machine_id, user_id)) = row {
            let scopes = self.ceiling_scopes(user_id).await;
            self.touch_machine(machine_id);
            return Some(AuthContext {
                user_id,
                key_id: machine_id,
                machine_id: Some(machine_id),
                scopes,
            });
        }

        // Legacy users.key_hash.
        let row: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM users \
             WHERE key_hash = $1 AND revoked_at IS NULL AND disabled_at IS NULL",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);
        if let Some((user_id,)) = row {
            let scopes = self.ceiling_scopes(user_id).await;
            return Some(AuthContext { user_id, key_id: user_id, machine_id: None, scopes });
        }

        // user_tokens.
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT t.id, t.user_id FROM user_tokens t \
             JOIN users u ON u.id = t.user_id \
             WHERE t.token_hash = $1 \
             AND t.revoked_at IS NULL \
             AND (t.expires_at IS NULL OR t.expires_at > now()) \
             AND u.revoked_at IS NULL AND u.disabled_at IS NULL",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None);
        if let Some((token_id, user_id)) = row {
            let scopes = self.ceiling_scopes(user_id).await;
            return Some(AuthContext { user_id, key_id: token_id, machine_id: None, scopes });
        }

        None
    }

    /// A legacy token's effective scopes = the owner's full ceiling (a legacy
    /// key carries no narrowing grant of its own, matching behavior
    /// where any of a user's tokens did everything the user could do).
    async fn ceiling_scopes(&self, user_id: Uuid) -> BTreeSet<Scope> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT scope FROM user_acls WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .unwrap_or_default();
        rows.iter().filter_map(|(s,)| Scope::parse(s)).collect()
    }

    /// Coarse to the minute so a chatty client is one write, not one per request.
    fn touch_key(&self, key_id: Uuid) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let _ = sqlx::query(
                "UPDATE auth_keys SET last_used_at = now() \
                 WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
            )
            .bind(key_id)
            .execute(&pool)
            .await;
        });
    }

    fn touch_machine(&self, machine_id: Uuid) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let _ = sqlx::query("UPDATE machines SET last_seen_at = now() WHERE id = $1")
                .bind(machine_id)
                .execute(&pool)
                .await;
        });
    }
}

/// Register a freshly-minted key into the unified `auth_keys` table with a
/// `key_acls` grant. Returns the new key id. Used by the enroll /
/// mint flows so new credentials live in the new model from day one (the
/// legacy tables are still written by those flows during the cutover window).
/// `key ⊆ user` is the caller's responsibility — pass scopes already
/// intersected with the owner's ceiling.
/// A key to insert into `auth_keys`, minus the scope grant.
pub struct NewKey<'a> {
    pub user_id: Uuid,
    pub key_hash: &'a str,
    pub key_preview: Option<&'a str>,
    pub label: Option<&'a str>,
    pub kind: &'a str,
    pub machine_id: Option<Uuid>,
    pub dispatcher_id: Option<Uuid>,
}

pub async fn register_key(
    pool: &PgPool,
    key: NewKey<'_>,
    scopes: impl IntoIterator<Item = Scope>,
) -> Result<Uuid, sqlx::Error> {
    let key_id: (Uuid,) = sqlx::query_as(
        "INSERT INTO auth_keys (user_id, key_hash, key_preview, label, kind, machine_id, dispatcher_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (key_hash) DO UPDATE SET label = EXCLUDED.label \
         RETURNING id",
    )
    .bind(key.user_id)
    .bind(key.key_hash)
    .bind(key.key_preview)
    .bind(key.label)
    .bind(key.kind)
    .bind(key.machine_id)
    .bind(key.dispatcher_id)
    .fetch_one(pool)
    .await?;
    for scope in scopes {
        sqlx::query("INSERT INTO key_acls (key_id, scope) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(key_id.0)
            .bind(scope.as_str())
            .execute(pool)
            .await?;
    }
    Ok(key_id.0)
}

/// The owner's current ceiling scopes — the default grant for a new machine /
/// dispatcher / primary key so it behaves like the owner (transparency).
pub async fn ceiling_of(pool: &PgPool, user_id: Uuid) -> BTreeSet<Scope> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT scope FROM user_acls WHERE user_id = $1")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    rows.iter().filter_map(|(s,)| Scope::parse(s)).collect()
}

pub async fn auth_middleware(request: Request, next: Next) -> Result<Response, StatusCode> {
    let auth_config = request
        .extensions()
        .get::<AuthConfig>()
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    // Resolve the credential from the `Authorization: Bearer` header (API /
    // daemon / TUI clients) or, failing that, the `HttpOnly` auth cookie set by
    // `/api/v1/auth/login` for browser clients. Both prove the same
    // token; the cookie keeps it out of URLs and JS-readable storage.
    let token = bearer_or_cookie(request.headers()).ok_or(StatusCode::UNAUTHORIZED)?;

    let ctx = auth_config.validate(&token).await.ok_or(StatusCode::UNAUTHORIZED)?;

    let mut request = request;
    request.extensions_mut().insert(ctx);
    Ok(next.run(request).await)
}

/// Name of the `HttpOnly` cookie that carries the user/admin token for browser
/// clients. Browsers send it automatically on same-origin requests
/// and WS upgrades, so the token never appears in a URL or in `localStorage`.
pub const AUTH_COOKIE: &str = "cctui_auth";

/// Pull the auth token from the `Cookie` header, if the `cctui_auth` cookie is
/// present. Tolerates the usual `a=1; b=2` cookie-pair formatting.
#[must_use]
pub fn token_from_cookies(headers: &http::HeaderMap) -> Option<String> {
    let raw = headers.get(http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k.trim() == AUTH_COOKIE).then(|| v.trim().to_string())
    })
}

/// Resolve a credential from the `Authorization: Bearer` header, falling back to
/// the auth cookie. The single token-extraction path shared by `auth_middleware`
/// and the WS upgrade so the two transports never diverge.
#[must_use]
pub fn bearer_or_cookie(headers: &http::HeaderMap) -> Option<String> {
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| token_from_cookies(headers))
}

/// Whether the request reached us over TLS, so the `Secure` cookie attribute is
/// safe to set. We sit behind a TLS-terminating proxy in prod, so trust
/// `x-forwarded-proto`; absent (local http dev) we omit `Secure` so the cookie
/// still works.
#[must_use]
pub fn request_is_https(headers: &http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"))
}

/// `Set-Cookie` value that stores `token` in the `HttpOnly` auth cookie.
#[must_use]
pub fn set_auth_cookie(token: &str, https: bool) -> String {
    // 1 year; the token is long-lived and revocation is server-side.
    let v = format!("{AUTH_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000");
    if https { format!("{v}; Secure") } else { v }
}

/// `Set-Cookie` value that immediately expires the auth cookie (logout).
#[must_use]
pub fn clear_auth_cookie(https: bool) -> String {
    let v = format!("{AUTH_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if https { format!("{v}; Secure") } else { v }
}

/// Generate a new secret. 64 hex chars = 256 bits of entropy.
#[must_use]
pub fn mint_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[must_use]
pub fn user_token(secret: &str) -> String {
    format!("cctui_u_{secret}")
}

#[must_use]
pub fn machine_token(secret: &str) -> String {
    format!("cctui_m_{secret}")
}

#[must_use]
pub fn sha256_hex(input: &str) -> String {
    cctui_proto::util::sha256_hex(input.as_bytes())
}

/// A non-secret, recognisable fragment of a token for display in the admin UI.
/// Keeps the typed prefix (`cctui_u_`) plus a few leading and
/// trailing chars so an operator can tell tokens apart, while the bulk of the
/// 256-bit secret stays hidden — `cctui_u_ab1234…ef34`.
#[must_use]
pub fn token_preview(token: &str) -> String {
    if token.len() <= 16 {
        return "•".repeat(token.len().saturating_sub(4))
            + token.get(token.len().saturating_sub(4)..).unwrap_or("");
    }
    format!("{}…{}", &token[..14], &token[token.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_roundtrips() {
        for s in Scope::all() {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
        }
        assert_eq!(Scope::parse("nope"), None);
    }

    #[test]
    fn admin_owner_filter_is_unscoped() {
        let admin = AuthContext {
            user_id: Uuid::nil(),
            key_id: Uuid::nil(),
            machine_id: None,
            scopes: Scope::all().into_iter().collect(),
        };
        assert!(admin.is_admin());
        assert_eq!(admin.owner_filter(), None);
        assert!(admin.requires(Scope::Enroll).is_ok());

        let uid = Uuid::new_v4();
        let user = AuthContext {
            user_id: uid,
            key_id: Uuid::new_v4(),
            machine_id: None,
            scopes: std::iter::once(Scope::Read).collect(),
        };
        assert!(!user.is_admin());
        assert_eq!(user.owner_filter(), Some(uid));
        assert!(user.requires(Scope::Read).is_ok());
        assert_eq!(user.requires(Scope::Dispatch), Err(StatusCode::FORBIDDEN));
    }

    #[test]
    fn sha256_is_stable() {
        assert_eq!(sha256_hex("hello").len(), 64);
        assert_eq!(sha256_hex("hello"), sha256_hex("hello"));
    }

    #[test]
    fn tokens_have_prefix() {
        let s = mint_secret();
        assert_eq!(s.len(), 64);
        assert!(user_token(&s).starts_with("cctui_u_"));
        assert!(machine_token(&s).starts_with("cctui_m_"));
    }

    #[test]
    fn token_preview_keeps_prefix_and_tail() {
        let token = user_token(&mint_secret());
        let preview = token_preview(&token);
        assert!(preview.starts_with("cctui_u_"));
        assert!(preview.contains('…'));
        assert!(token.ends_with(&preview[preview.len() - 4..]));
        assert!(preview.len() < token.len());
        assert!(!token.contains(&preview));
    }

    fn headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn token_from_cookies_picks_the_auth_pair() {
        let h = headers(&[("cookie", "foo=1; cctui_auth=tok123 ; bar=2")]);
        assert_eq!(token_from_cookies(&h).as_deref(), Some("tok123"));

        assert_eq!(token_from_cookies(&headers(&[("cookie", "foo=1; bar=2")])), None);
        assert_eq!(token_from_cookies(&headers(&[])), None);
    }

    #[test]
    fn bearer_or_cookie_prefers_bearer_then_cookie_and_rejects_other_schemes() {
        assert_eq!(
            bearer_or_cookie(&headers(&[("authorization", "Bearer abc")])).as_deref(),
            Some("abc")
        );
        let h = headers(&[("authorization", "Basic Zm9v"), ("cookie", "cctui_auth=ck")]);
        assert_eq!(bearer_or_cookie(&h).as_deref(), Some("ck"));
        assert_eq!(bearer_or_cookie(&headers(&[("authorization", "Basic Zm9v")])), None);
        assert_eq!(bearer_or_cookie(&headers(&[])), None);
    }

    #[test]
    fn request_is_https_trusts_forwarded_proto_case_insensitively() {
        assert!(request_is_https(&headers(&[("x-forwarded-proto", "https")])));
        assert!(request_is_https(&headers(&[("x-forwarded-proto", "HTTPS")])));
        assert!(!request_is_https(&headers(&[("x-forwarded-proto", "http")])));
        assert!(!request_is_https(&headers(&[])));
    }

    #[test]
    fn auth_cookies_set_hardening_attrs_and_secure_only_over_https() {
        let secure = set_auth_cookie("tok", true);
        assert!(secure.starts_with("cctui_auth=tok;"));
        assert!(secure.contains("HttpOnly"));
        assert!(secure.contains("SameSite=Lax"));
        assert!(secure.contains("Max-Age=31536000"));
        assert!(secure.ends_with("; Secure"));

        let plain = set_auth_cookie("tok", false);
        assert!(!plain.contains("Secure"), "no Secure attr over plain http");

        let cleared = clear_auth_cookie(true);
        assert!(cleared.contains("cctui_auth=;"));
        assert!(cleared.contains("Max-Age=0"));
        assert!(cleared.ends_with("; Secure"));
        assert!(!clear_auth_cookie(false).contains("Secure"));
    }

    #[test]
    fn token_preview_masks_short_tokens_without_leaking_the_secret() {
        let p = token_preview("secret_1234");
        assert!(p.ends_with("1234"));
        assert!(p.contains('•'));
        assert!(!p.contains("secret"));
    }

    #[tokio::test]
    async fn cache_roundtrip() {
        let pool = PgPool::connect_lazy("postgres://invalid").unwrap();
        let cfg = AuthConfig::new(vec!["admin-secret".into()], pool);
        let ctx = AuthContext {
            user_id: Uuid::nil(),
            key_id: Uuid::nil(),
            machine_id: None,
            scopes: std::iter::once(Scope::Read).collect(),
        };
        cfg.cache_put("h".into(), ctx);
        assert!(cfg.cache_get("h").is_some());
        cfg.purge("h");
        assert!(cfg.cache_get("h").is_none());
    }
}
