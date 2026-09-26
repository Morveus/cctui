use std::sync::Arc;
use std::time::{Duration, Instant};

use cctui_proto::models::MachineLiveness;
use dashmap::DashMap;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::AuthConfig;
use crate::bus::Bus;
use crate::config::Config;
use crate::dispatchers::Registry as DispatcherRegistry;
use crate::registry::SharedRegistry;
use crate::routes::permissions::SharedPermissionStore;
use crate::skill_store::SkillStore;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    pub registry: SharedRegistry,
    pub permission_store: SharedPermissionStore,
    /// The single routing seam for all WS traffic: daemon/dispatcher
    /// connection registries, correlated round-trips, per-session streams and
    /// the server event fan-out all live behind it.
    pub bus: Bus,
    #[allow(dead_code)]
    pub auth_config: AuthConfig,
    /// The `WebAuthn` relying party, or `None` when this deployment can't run a
    /// passkey ceremony (no secure public URL configured). `None` makes every
    /// passkey route answer "unavailable" and leaves the token login alone.
    pub webauthn: Option<Arc<webauthn_rs::Webauthn>>,
    pub skills: Arc<SkillStore>,
    /// This pod's identity for replica-aware WS presence. Rows in
    /// `ws_presence` record which pod terminates each daemon/dispatcher WS so
    /// peers can forward WS-targeted requests instead of reporting a spurious
    /// "offline".
    pub presence: Arc<crate::presence::PodIdentity>,
    /// The cluster-internal shared secret authenticating pod-to-pod
    /// `/internal/bus/*` calls. `Some` only when the peer transport
    /// is enabled (`CCTUI_POD_IP` set); `None` makes the internal endpoints
    /// refuse everything.
    pub internal_secret: Option<Arc<str>>,
    /// Last broadcast liveness tier per enrolled dispatcher, peer of
    /// [`Self::machine_liveness`].
    pub dispatcher_liveness: Arc<DashMap<Uuid, MachineLiveness>>,
    pub dispatchers: Arc<DispatcherRegistry>,
    /// Last broadcast liveness tier per machine. The daemon-WS
    /// heartbeat handler and the reaper both re-derive a machine's tier from
    /// `machines.last_seen_at` age; this map lets them broadcast a
    /// [`ServerEvent::MachineLiveness`] only on an actual transition (e.g.
    /// online → offline) rather than on every tick.
    pub machine_liveness: Arc<DashMap<Uuid, MachineLiveness>>,
    /// Per-OAuth-account refresh mutex. OAuth refresh tokens are
    /// single-use; two concurrent sessions on the same account must not both
    /// refresh (the second would invalidate the first). The gateway grabs the
    /// account's lock around the read-expiry → refresh → persist sequence.
    pub account_locks: Arc<DashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
    /// Shared outbound HTTP client for the gateway passthrough.
    pub http_client: reqwest::Client,
    /// Newest upstream release, when newer than this build (`/version`).
    pub update_check: Arc<crate::update_check::UpdateCheck>,
    /// Serialises "Update" clicks: one self-update agent per release at a time.
    pub self_update: Arc<crate::routes::self_update::SelfUpdateGuard>,
    /// Commands awaiting their daemon `CommandResult`, keyed by `command_id`,
    /// so the result can be scoped to its session and a failed spawn can be
    /// persisted as a session row.
    pub pending_commands: Arc<DashMap<Uuid, PendingCommand>>,
    /// Optional Langfuse tracing sink for the `/gateway` proxy.
    /// `None` unless the `CCTUI_LANGFUSE_*` env is configured — when absent the
    /// gateway never reconstructs the body, so there is zero overhead and no
    /// behaviour change. When present, each gateway call fires a fire-and-forget
    /// trace beside the proxied request (never in its critical path).
    pub langfuse: Option<Arc<crate::langfuse::LangfuseClient>>,
    /// Pending "Sign in with Claude" OAuth logins, keyed by nonce.
    /// In-memory, TTL-bounded, scoped to the authenticated user. Entries are
    /// single-use (deleted on finish) and lazily swept on access.
    pub pending_oauth_logins: crate::routes::accounts::PendingOAuthLogins,
    /// Slow-refresh cache of per-account OAuth usage windows, keyed by
    /// account id. Anthropic's usage endpoint rate-limits per access token, so we
    /// serve a cached value and only re-fetch upstream past a TTL — the accounts
    /// view polls lazily and many clients share one cache entry per account.
    pub account_usage_cache: AccountUsageCache,
    /// PR status cache the classifier reads for the `Review` bucket. Has no
    /// feeder currently, so it stays empty (no `Review` bucket surfaces).
    pub pr_status_cache: cctui_proto::classifier::PrStatusCache,
    /// Orphan-token spam guard for the gateway, keyed by the SHA-256 fingerprint
    /// of the (unresolvable) session token. A worker whose session→account
    /// binding was lost retries `/gateway` indefinitely; each retry ran a DB
    /// lookup, so a single zombie could starve the connection pool and slow the
    /// whole server (including the webui). Once a fingerprint exceeds the spam
    /// threshold within the window we mark it blocked, and subsequent requests
    /// are dropped *before* any DB lookup until the block expires.
    pub gateway_orphan_spam: Arc<DashMap<String, OrphanSpam>>,
    /// Accounts whose upstream credentials the gateway has seen rejected
    /// (`needs_reauth`), keyed by account id. Mirrors the persisted
    /// `account_providers.needs_reauth` flag so the success path can clear it with a
    /// single DB write only on the actual transition — without this in-memory
    /// gate every successful passthrough would issue a conditional UPDATE.
    pub account_reauth: Arc<DashMap<Uuid, ()>>,
    /// Latest machine/account-scoped codex model catalog per machine,
    /// keyed by `machine_id`. Read-through cache over `codex_model_catalogs`,
    /// warmed on boot and written through on every daemon report.
    pub codex_catalogs: Arc<DashMap<Uuid, crate::routes::codex_models::CachedCatalog>>,
    /// Codex catalog read from upstream with an account's own OAuth, keyed by
    /// the provider credential that holds it. Outranks every machine catalog:
    /// a gateway-only machine's codex can only report its compiled-in list.
    pub codex_account_catalogs: Arc<DashMap<Uuid, crate::routes::codex_models::CachedCatalog>>,
    /// Latest `@openai/codex` release resolved from npm, with its own 1h TTL.
    /// The catalog upstream serves is gated by the `client_version` we send, so
    /// a stale value silently hides newly launched models.
    pub codex_latest_version:
        Arc<std::sync::Mutex<Option<crate::routes::codex_models::CachedVersion>>>,
    /// Rolling per-machine daemon-WS eviction counts; an escalation to
    /// ERROR when a machine flaps past the threshold is the eviction-loop alert.
    pub eviction_tracker: Arc<crate::bandwidth_watch::EvictionTracker>,
    /// Rolling per-machine daemon-WS connect counts; rapid reconnects mean a
    /// daemon crashloop (launchd/systemd respawn), which last-seen liveness
    /// never surfaces — escalated to ERROR + ntfy.
    pub connect_tracker: Arc<crate::bandwidth_watch::ConnectTracker>,
    /// Last-seen upload total vs persisted insert count per machine, so
    /// a heartbeat can flag uploads that grow without matching `stream_events`.
    pub divergence_tracker: Arc<crate::bandwidth_watch::DivergenceTracker>,
    /// Persisted `stream_events` inserts observed per machine since server start,
    /// the cheap in-memory signal the divergence detector reads.
    pub machine_event_inserts: Arc<DashMap<Uuid, u64>>,
    /// `CctuiAgent` spawn capabilities keyed by session id. Read-through cache
    /// over `session_spawn_capabilities`, which holds the durable copy.
    pub spawn_capabilities: Arc<DashMap<String, cctui_proto::api::SpawnCapability>>,
    /// Per-session dollar budgets applied to `CctuiAgent` children, keyed by the
    /// child's session id. Read on the gateway hot path only while non-empty.
    pub session_usd_budgets: Arc<DashMap<String, f64>>,
    /// Per-(account, provider) gateway rate-limit windows, keyed by provider row
    /// id. In-memory rolling RPM/TPM counters the proxy admits requests against;
    /// only touched for a provider whose `rate_limits_json` sets a limit.
    pub gateway_rate_windows: Arc<DashMap<Uuid, crate::routes::gateway::RateWindow>>,
}

/// Sliding-window spam state for one orphan token fingerprint. See
/// [`AppState::gateway_orphan_spam`].
#[derive(Clone)]
pub struct OrphanSpam {
    /// Unresolved-401 count within the current window.
    pub count: u32,
    /// Start of the current counting window.
    pub window_start: std::time::Instant,
    /// When set and still in the future, requests for this fingerprint are
    /// dropped before any DB work.
    pub blocked_until: Option<std::time::Instant>,
}

/// A cached usage fetch: when it was fetched and the JSON payload (the raw
/// upstream usage windows). `None` payload means "fetched, but no usage" (e.g. a
/// non-anthropic account) — still cached so we don't re-hit upstream.
#[derive(Clone)]
pub struct CachedUsage {
    pub fetched_at: std::time::Instant,
    pub usage: Option<serde_json::Value>,
}

/// Per-account usage cache.
pub type AccountUsageCache = Arc<DashMap<Uuid, CachedUsage>>;

/// Everything the daemon's `CommandResult` needs to write a failed spawn as a
/// `sessions` row: the daemon never registers a session that failed to start,
/// so this is the only place the row's identity survives.
#[derive(Debug, Clone)]
pub struct FailedSpawnRow {
    pub session_id: String,
    pub machine_id: Uuid,
    pub user_id: Uuid,
    pub adapter_id: String,
    pub working_dir: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingCommand {
    /// Existing session the command targets (interrupt, set-model).
    pub session_id: Option<String>,
    /// Set for spawns; consumed only when the spawn fails.
    pub spawn: Option<FailedSpawnRow>,
    pub at: Instant,
}

/// A daemon that never answers would leak entries forever.
pub const PENDING_COMMAND_TTL: Duration = Duration::from_mins(10);

/// Register `command_id` and drop entries older than [`PENDING_COMMAND_TTL`].
pub fn track_command(
    map: &DashMap<Uuid, PendingCommand>,
    command_id: Uuid,
    session_id: Option<String>,
    spawn: Option<FailedSpawnRow>,
) {
    let now = Instant::now();
    map.retain(|_, c| now.duration_since(c.at) < PENDING_COMMAND_TTL);
    map.insert(command_id, PendingCommand { session_id, spawn, at: now });
}
