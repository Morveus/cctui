/// Refresh proactively once the access token is within this window of expiry.
pub const REFRESH_SKEW_SECS: i64 = 60;

pub const SESSION_TOKEN_TTL_HOURS_DEFAULT: i64 = 12;

pub fn ttl_hours_from(var: Option<String>) -> i64 {
    var.and_then(|v| v.parse::<i64>().ok())
        .filter(|h| *h > 0)
        .unwrap_or(SESSION_TOKEN_TTL_HOURS_DEFAULT)
}

pub fn session_token_ttl() -> chrono::Duration {
    chrono::Duration::hours(ttl_hours_from(std::env::var("CCTUI_SESSION_TOKEN_TTL_HOURS").ok()))
}

/// Anthropic Claude-Code OAuth token endpoint + client id. These are not stable
/// public APIs (caveat accepted in the ticket); overridable via env so we can
/// track upstream changes without a redeploy of code.
pub fn anthropic_token_url() -> String {
    std::env::var("CCTUI_ANTHROPIC_OAUTH_TOKEN_URL")
        .unwrap_or_else(|_| "https://console.anthropic.com/v1/oauth/token".into())
}
pub fn anthropic_client_id() -> String {
    std::env::var("CCTUI_ANTHROPIC_OAUTH_CLIENT_ID")
        .unwrap_or_else(|_| "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into())
}
/// claude.ai authorize endpoint for the manual code-paste OAuth login.
/// Overridable so we can track upstream without a redeploy.
pub fn anthropic_authorize_url() -> String {
    std::env::var("CCTUI_ANTHROPIC_OAUTH_AUTHORIZE_URL")
        .unwrap_or_else(|_| "https://claude.ai/oauth/authorize".into())
}
/// Redirect URI used for the manual code-paste flow — claude.ai displays the
/// `code#state` pair instead of redirecting. Must match what the token exchange
/// sends back.
pub fn anthropic_oauth_redirect_uri() -> String {
    "https://console.anthropic.com/oauth/code/callback".into()
}
pub fn anthropic_upstream() -> String {
    std::env::var("CCTUI_ANTHROPIC_UPSTREAM").unwrap_or_else(|_| "https://api.anthropic.com".into())
}
/// OpenAI/Codex OAuth token endpoint. Codex's public client exchanges +
/// refreshes here with **form-encoded** bodies (unlike Anthropic's JSON).
/// Overridable via env to track upstream changes without a code redeploy.
pub fn openai_token_url() -> String {
    std::env::var("CCTUI_OPENAI_OAUTH_TOKEN_URL")
        .unwrap_or_else(|_| "https://auth.openai.com/oauth/token".into())
}
/// Codex's public OAuth client id. Defaults to the well-known `codex` client
/// (`app_EMoamEEZ73f0CkXaXp7hrann`); overridable via env.
pub fn openai_client_id() -> String {
    std::env::var("CCTUI_OPENAI_OAUTH_CLIENT_ID")
        .unwrap_or_else(|_| "app_EMoamEEZ73f0CkXaXp7hrann".into())
}
/// auth.openai.com authorize endpoint for the "Sign in with `ChatGPT`" login.
/// Overridable so we can track upstream without a redeploy.
pub fn openai_authorize_url() -> String {
    std::env::var("CCTUI_OPENAI_OAUTH_AUTHORIZE_URL")
        .unwrap_or_else(|_| "https://auth.openai.com/oauth/authorize".into())
}
/// Fixed redirect URI baked into Codex's public client — we can't point it at
/// our own host. The browser redirect to localhost:1455 fails to load; the
/// user copies the full URL from the address bar and pastes it back.
pub fn openai_oauth_redirect_uri() -> String {
    std::env::var("CCTUI_OPENAI_OAUTH_REDIRECT_URI")
        .unwrap_or_else(|_| "http://localhost:1455/auth/callback".into())
}
pub fn openai_upstream() -> String {
    // Codex ChatGPT-backed accounts talk to the chatgpt backend, NOT
    // api.openai.com (matches what the codex CLI + CLIProxyAPI do).
    std::env::var("CCTUI_OPENAI_UPSTREAM")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".into())
}

/// Fireworks' OpenAI-compatible inference base. A provider row's `base_url`
/// still wins when set; this is the default upstream for the family.
pub fn fireworks_upstream() -> String {
    std::env::var("CCTUI_FIREWORKS_UPSTREAM")
        .unwrap_or_else(|_| "https://api.fireworks.ai/inference/v1".into())
}

/// Whether the response body must be teed, which forces the upstream call to be
/// made without `accept-encoding`. reqwest is built without decompression
/// features, so a gzip/zstd body reaches the tee as opaque bytes: Langfuse gets a
/// trace with no usage, and Fireworks gets no metered usage at all.
pub const fn tees_response(langfuse: bool, fireworks: bool) -> bool {
    langfuse || fireworks
}

/// Per-provider request-shaping settings for the `fireworks` family, resolved
/// over [`fireworks_default_settings`]. Applied by the gateway on the way
/// upstream so no worker needs to know them (and none can bypass them).
pub struct FireworksSettings {
    /// Injected as the request body's `context_length_exceeded_behavior`
    /// (Fireworks defaults to `truncate`, which silently loses prompt).
    /// `None` (settings key `null`) injects nothing.
    pub context_length_exceeded_behavior: Option<String>,
    /// Pin a conversation's requests to one replica so its prompt prefix stays
    /// cache-warm: the session id goes out as `user` + `x-session-affinity`.
    pub session_affinity: bool,
    /// Extra body keys merged in, none overriding what the client sent.
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// Name of cctui's own API key as it appears in Fireworks' billing console.
    /// A Fireworks account is shared across keys and tenants, so without this
    /// there is no way to tell cctui's spend from anyone else's — unset disables
    /// billing reconciliation rather than importing the whole account's usage.
    pub billing_api_key_name: Option<String>,
}

/// Defaults for a new `fireworks` provider row. Stored as data on the row at
/// create so the accounts UI can edit every knob.
pub fn fireworks_default_settings() -> serde_json::Value {
    serde_json::json!({
        "context_length_exceeded_behavior": "error",
        "session_affinity": true,
        "extra_body": {},
        "billing_api_key_name": null,
    })
}

impl FireworksSettings {
    /// Resolve a stored `provider_settings` blob over the defaults; an absent or
    /// malformed blob yields the defaults.
    pub fn resolve(stored: Option<&serde_json::Value>) -> Self {
        let mut merged = fireworks_default_settings();
        if let Some(overlay) = stored.filter(|v| v.is_object()) {
            cctui_proto::util::deep_merge(&mut merged, overlay.clone());
        }
        Self {
            context_length_exceeded_behavior: merged
                .get("context_length_exceeded_behavior")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned),
            session_affinity: merged
                .get("session_affinity")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            extra_body: merged
                .get("extra_body")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default(),
            billing_api_key_name: merged
                .get("billing_api_key_name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        }
    }

    /// Apply the settings to a JSON request body. Every injection is
    /// "only if absent" — an explicit client value always wins.
    pub fn apply_body(&self, body: &mut serde_json::Value, session_id: Option<&str>) {
        let Some(obj) = body.as_object_mut() else { return };
        if let Some(behavior) = self.context_length_exceeded_behavior.as_ref() {
            obj.entry("context_length_exceeded_behavior")
                .or_insert_with(|| serde_json::Value::String(behavior.clone()));
        }
        for (k, v) in &self.extra_body {
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if self.session_affinity
            && let Some(sid) = session_id
        {
            obj.entry("user").or_insert_with(|| serde_json::Value::String(sid.to_owned()));
        }
    }
}

/// The provider *family* of an account: which env vars it drives, and the key
/// `UNIQUE (account_id, family)` enforces one credential per. `fireworks` is its
/// own family — despite the `OpenAI` wire protocol — so a Fireworks key can sit
/// next to a codex credential on one account.
///
/// [`label`](Self::label) is the stored value of the generated `family` column;
/// per-family SQL predicates compare against it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    Anthropic,
    Openai,
    Fireworks,
}

impl Family {
    /// Derive the family from a stored `provider` value. Must agree with the
    /// generated `family` column (migration 078).
    pub fn from_provider(provider: &str) -> Self {
        if provider == "fireworks" {
            Self::Fireworks
        } else if provider.contains("openai") {
            Self::Openai
        } else {
            Self::Anthropic
        }
    }
    /// Parse a family label back (the `family` column / API `family` field).
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::Openai),
            "fireworks" => Some(Self::Fireworks),
            _ => None,
        }
    }
    /// Derive the family from a spawn adapter id (`codex*` → openai,
    /// `opencode*` → fireworks, else anthropic). This IS the spawn resolution
    /// key: the adapter names the harness family, and the account identity
    /// carries at most one provider row per family.
    pub fn from_adapter(adapter_id: &str) -> Self {
        if adapter_id.starts_with("opencode") {
            Self::Fireworks
        } else if adapter_id.starts_with("codex") {
            Self::Openai
        } else {
            Self::Anthropic
        }
    }
    /// Human label for error messages, and the stored `family` column value.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Fireworks => "fireworks",
        }
    }
}
