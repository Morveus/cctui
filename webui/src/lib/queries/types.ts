/** One selectable model on a compatible-endpoint account. */
export interface AccountModel {
  model: string;
  label: string;
  /** USD per million tokens. Account-owned data — the pricing a pay-per-token
   *  provider is metered against, editable per account. */
  price_input_per_mtok?: number | null;
  price_cached_input_per_mtok?: number | null;
  price_output_per_mtok?: number | null;
  context_length?: number | null;
}

/** One provider credential under an account identity. Tokens are
 *  never returned by the API — only provider/expiry/last-used + lightweight
 *  usage stats. `provider` is `anthropic` (claude), `openai` (codex), or a
 *  `*-compatible` base-url-overridden endpoint. */
export interface AccountProvider {
  id: string;
  account_id: string;
  /** `anthropic` | `openai` (native) | `anthropic-compatible` |
   *  `openai-compatible` (a base-url-overridden endpoint). */
  provider: string;
  /** Provider family: `anthropic` | `openai` | `fireworks`. At most one
   *  provider per family per account (guarded by construction). */
  family: string;
  /** Selectable models for a compatible endpoint; null/empty for
   *  native subscription accounts (which use the harness's native families).
   *  Safe to surface — model names aren't secret (unlike base URL + credential). */
  models: AccountModel[] | null;
  /** Per-provider logical→concrete model alias map, e.g.
   *  `{ opus: "claude-opus-4-8[1m]" }`. Resolved server-side at spawn.
   *  null/empty means no remapping. */
  model_aliases: Record<string, string> | null;
  /** True for a server-synthesized provider (the CCTUI_CLAUDE_LITELLM_* shim) —
   *  read-only: edit/delete are rejected server-side. */
  managed: boolean;
  /** Compatible-endpoint base URL; null for native providers. */
  base_url: string | null;
  /** `oauth` (native) | `bearer` | `api_key` (compatible). */
  auth_scheme: string;
  provider_account_id: string | null;
  expires_at: string | null;
  created_at: string;
  last_used_at: string | null;
  request_count: number;
  bytes_transferred: number;
  /** Total tokens (input + output + cache) across this provider's sessions. */
  total_tokens: number;
  /** Rough USD cost estimate from tokens (per-provider blended rate). */
  est_cost_usd: number;
  /** Per-provider soft limits on cctui's own share of the usage windows,
   *  keyed by canonical window id (`session` | `weekly_all` |
   *  `weekly_model:<slug>`). Absent key ⇒ no config for that window; null map ⇒
   *  none at all. The per-window bypass ignores that window's cap when
   *  it resets within that many minutes. */
  soft_limits: Record<string, SoftLimitConfig> | null;
  /** Usage ticker: a system-reminder to running sessions each time a usage
   *  window crosses another `step_pct` bucket. Null ⇒ off. */
  usage_notices: UsageNotices | null;
  /** Whether this credential's gauge shows in the header strip. */
  header_pin: boolean;
  /** Credential health: true once the gateway saw the upstream provider
   *  reject this credential; cleared on the next successful upstream call. UI
   *  shows a "reauthenticate" badge + button when set. */
  needs_reauth: boolean;
  last_auth_error: string | null;
  last_auth_error_at: string | null;
  /** Gateway request-shaping settings for this credential (fireworks:
   *  `context_length_exceeded_behavior`, `session_affinity`, `extra_body`).
   *  Distinct from `settings_json`, which is harness settings. */
  provider_settings: Record<string, unknown> | null;
  /** Validated, allowlisted harness settings applied to sessions run under this
   *  provider. Config, not secret → returned. Only SAFE/CARE
   *  settings.json keys; the server rejects MANAGED/SYSTEM keys on write. */
  settings_json: Record<string, unknown> | null;
  /** Per-(account, provider) gateway rate limits `{ rpm?, tpm? }`, enforced in
   *  the proxy path. Null ⇒ no throttling. */
  rate_limits: RateLimits | null;
}

/** Gateway rate limits shared across an account's concurrent sessions. Each
 *  dimension optional; absent/zero ⇒ that dimension is unlimited. */
export interface RateLimits {
  /** Max requests admitted per rolling 60s window. */
  rpm?: number | null;
  /** Max tokens counted per rolling 60s window. */
  tpm?: number | null;
}

/** An account identity: name + owner, with zero or more provider
 *  credentials attached. */
export interface OAuthAccount {
  id: string;
  name: string;
  /** Owner-chosen identity glyph (one emoji); null ⇒ the generated letter square. */
  emoji: string | null;
  /** Owning user — shown to admins, who see all accounts. */
  user_id: string;
  user_name: string | null;
  created_at: string;
  updated_at: string;
  providers: AccountProvider[];
  /** The owner's pool veto: with this false, only the owner may enrol the
   *  account in a pool. Grantees can still launch on it by name. */
  pool_eligible: boolean;
  /** Relative plan size for the pool gauge (1 = one unit of budget). The
   *  provider only reports percentages, so the operator states the ratio. */
  pool_weight: number;
  /** Names (only) of the account's free-form extra env vars, sorted.
   *  Values stay write-only (never returned); the names drive the "currently
   *  set" display + replace-on-save affordance in the account editor. */
  env_names: string[];
  // env VALUES are deliberately absent — the `env_json` blob is write-only,
  // never returned over the API (may hold secrets), exactly like the OAuth tokens.
}

/** Single-provider back-compat: the spawn/dispatch pickers derive the
 *  credential from the account's provider-family union; the
 *  remaining first-row reader is DispatchersPanel until it grows a provider
 *  dimension. */
export const primaryProvider = (a: OAuthAccount): AccountProvider | undefined => a.providers[0];

/** One of a session's active per-family gateway credential bindings. */
export interface SessionBinding {
  family: "anthropic" | "openai" | "fireworks";
  credential_id: string;
  account_id: string;
  account_name: string;
}

/** Per-provider usage ticker setting. */
export interface UsageNotices {
  enabled: boolean;
  /** Percent granularity of the notices, 1–100. */
  step_pct: number;
}

/** One window's soft-limit config. Absent field ⇒ unset for that
 *  window: no `cap_pct` ⇒ no cap; no `bypass_minutes` ⇒ no bypass. */
export interface SoftLimitConfig {
  cap_pct?: number | null;
  /** Dollar cap; applies to the `session_usd` / `usd_5h` / `usd_7d` windows. */
  cap_usd?: number | null;
  bypass_minutes?: number | null;
  /** Max burn rate as a multiple of the window's linear budget; percent
   *  windows only. 1.5 ⇒ refuse once spent 50% faster than evenly. */
  pace_cap?: number | null;
}

/** One normalized, provider-agnostic usage window. `key` is the
 *  canonical id (`session` | `weekly_all` | `weekly_model:<slug>`), `label` the
 *  server-supplied display string, `utilization` a 0–100 percent (may exceed). */
export interface UsageWindow {
  key: string;
  kind: string;
  label: string;
  utilization: number;
  /** USD spent in the window; set only for the dollar windows. */
  amount_usd?: number | null;
  resets_at?: string | null;
  model_id?: string | null;
  model_display_name?: string | null;
  /** Burn rate against the window's linear budget; absent when the window has
   *  no reset time or no known length. */
  pace?: UsagePace | null;
}

/** Server-computed pace of a window: `ratio` < 1 is under the linear pace,
 *  > 1 is burning faster; `projected_wall_at` is when 100% lands at the
 *  current rate (absent while idle). */
export interface UsagePace {
  elapsed_fraction: number;
  expected_pct: number;
  ratio: number;
  projected_wall_at?: string | null;
  /** Hours between the earlier sample and now when the rate is a measured
   *  slope; absent when it is the window average. */
  slope_hours?: number | null;
}

/** Per-account subscription usage. `windows` is the normalized
 *  collection the UI renders (may be empty — distinct from a fetch error and from
 *  a provider with no usage API). `usage` keeps the raw upstream payload for the
 *  legacy chip. `age_secs` reflects the slow-refresh cache. */
export interface AccountUsage {
  account_id: string;
  provider: string;
  usage: {
    five_hour?: { utilization?: number | null; resets_at?: string | null } | null;
    seven_day?: { utilization?: number | null; resets_at?: string | null } | null;
    seven_day_opus?: { utilization?: number | null; resets_at?: string | null } | null;
    seven_day_sonnet?: { utilization?: number | null; resets_at?: string | null } | null;
  } | null;
  windows: UsageWindow[];
  age_secs: number;
  /** Whether a usage-limit reset can be claimed right now; absent when the
   *  provider's payload carries no reset mechanism. */
  limit_reset?: LimitResetStatus | null;
}

/** One row of `GET /accounts/usage`: a provider's usage plus the account it
 *  belongs to, so the header can group without a second request. */
export interface AccountUsageEntry extends AccountUsage {
  account: string;
  account_name: string;
  /** The account's identity glyph; null ⇒ the generated letter square. */
  account_emoji: string | null;
  header_pin: boolean;
}

/** Codex reset credits (`kind: codex`) or Claude Code's `/limit-reset`
 *  (`kind: claude`), normalized for the button in the usage row. */
export interface LimitResetStatus {
  kind: "codex" | "claude";
  available: boolean;
  title?: string | null;
  credit_id?: string | null;
  ineligible_reason?: string | null;
  next_available_at?: string | null;
  weekly_resets_at?: string | null;
}

/** Outcome of one limit-reset claim, upstream's verdict verbatim. */
export interface LimitResetResponse {
  account_id: string;
  provider: string;
  outcome: string;
  credit_id?: string | null;
  next_available_at?: string | null;
  weekly_resets_at?: string | null;
  idempotency_key: string;
  reused: boolean;
}

/** Register payload — the refresh token is sent cleartext once, stored
 *  encrypted, and never read back. */
export interface CreateAccount {
  name: string;
  /** Identity glyph; blank/absent ⇒ the letter square. */
  emoji?: string;
  provider: string;
  /** OAuth refresh token (native subscription accounts). */
  refresh_token?: string;
  /** Initial access token (native) OR the static credential for a compatible
   *  endpoint's bearer/api key. */
  access_token?: string;
  expires_at?: number;
  /** Compatible-endpoint base URL; required for `*-compatible`. */
  base_url?: string;
  /** Selectable models for a compatible endpoint. */
  models?: AccountModel[];
  /** Logical→concrete model alias map; honoured for every provider. */
  model_aliases?: Record<string, string>;
  /** `bearer` | `api_key` for a compatible endpoint. */
  auth_scheme?: string;
  /** Owner — required when authenticated with the admin token. */
  user_id?: string;
  /** Per-account soft limits keyed by canonical window id. */
  soft_limits?: Record<string, SoftLimitConfig>;
  /** Gateway settings; absent on a fireworks create seeds the defaults. */
  provider_settings?: Record<string, unknown>;
  /** Gateway rate limits `{ rpm?, tpm? }`; empty/zero ⇒ no throttling. */
  rate_limits?: RateLimits;
}

/** Provider create/attach payload: `POST /accounts/{id}/providers`.
 *  The pasted-token / compatible-endpoint path — the native OAuth flows attach
 *  via `oauth/start`'s `account_id` instead. 409 when the account already has
 *  a provider of the same family (anthropic/openai). */
export interface CreateProvider {
  provider: string;
  /** OAuth refresh token (native subscription providers). */
  refresh_token?: string;
  /** Initial access token (native) OR the static credential for a compatible
   *  endpoint. */
  access_token?: string;
  expires_at?: number;
  /** Compatible-endpoint base URL; required for `*-compatible`. */
  base_url?: string;
  models?: AccountModel[];
  model_aliases?: Record<string, string>;
  /** `bearer` | `api_key` for a compatible endpoint. */
  auth_scheme?: string;
  soft_limits?: Record<string, SoftLimitConfig>;
  settings_json?: Record<string, unknown>;
  /** Gateway settings; absent on a fireworks create seeds the defaults. */
  provider_settings?: Record<string, unknown>;
  /** Gateway rate limits `{ rpm?, tpm? }`; empty/zero ⇒ no throttling. */
  rate_limits?: RateLimits;
}

/** Identity-level edit payload: rename and/or replace the write-only
 *  extra-env map. Provider-credential fields moved to [`UpdateProvider`]. */
export interface UpdateAccount {
  name?: string;
  /** Identity glyph. A blank string clears it back to the letter square;
   *  absent leaves it unchanged. */
  emoji?: string;
  /** Replacement extra-env map. Provided → re-encrypts and replaces
   *  (an empty map clears it); absent → unchanged. WRITE-ONLY: never returned,
   *  so the editor only ever sends new values, it can't display stored ones. */
  env_json?: Record<string, string>;
  /** Stored env var names to delete server-side; ignored when
   *  `env_json` is provided (replace-all wins). */
  env_remove?: string[];
  /** Owner-only veto: whether grantees may enrol this account in their pools.
   *  Applied on its own statement server-side, so it also works on a managed
   *  account (whose identity is otherwise read-only). */
  pool_eligible?: boolean;
  /** Owner-only: relative plan size for the pool gauge; positive, 1 by default. */
  pool_weight?: number;
}

/** Provider-credential edit payload. Every field optional; an absent field leaves that
 *  column unchanged. The compatible-endpoint fields are only honoured for a
 *  non-managed `*-compatible` provider. A blank `base_url`/`access_token`
 *  keeps the stored value (they are never read back). */
export interface UpdateProvider {
  base_url?: string;
  auth_scheme?: string;
  models?: AccountModel[];
  /** Replacement alias map; provided replaces wholesale (empty clears),
   *  absent leaves it unchanged. Editable for every provider. */
  model_aliases?: Record<string, string>;
  /** New static credential; omit/blank to keep the stored one. */
  access_token?: string;
  /** Replacement soft-limit map, keyed by canonical window id.
   *  Provided → REPLACES the whole stored map ({} clears all, a dropped key
   *  removes that window's config); absent → unchanged. */
  soft_limits?: Record<string, SoftLimitConfig>;
  /** Replacement usage ticker setting; `enabled: false` turns it off. */
  usage_notices?: UsageNotices;
  /** Show this credential's gauge in the header strip; absent → unchanged. */
  header_pin?: boolean;
  /** Replacement validated settings blob. Provided → replaces
   *  the stored settings wholesale (an empty object clears it); absent →
   *  unchanged. Validated against the SAFE/CARE allowlist before persist. */
  settings_json?: Record<string, unknown>;
  /** Replacement gateway settings object; provided replaces wholesale
   *  (an empty object drops back to the family defaults). */
  provider_settings?: Record<string, unknown>;
  /** Replacement rate-limit object `{ rpm?, tpm? }`; provided replaces the
   *  stored value (empty/zero clears it), absent → unchanged. */
  rate_limits?: RateLimits;
}

/** "Sign in with Claude" OAuth start payload/response. */
export interface OAuthStartResponse {
  nonce: string;
  authorize_url: string;
}

/**
 * Finish payload. For Claude: the `code#state` pair pasted from claude.ai. For
 * Codex: the full localhost:1455 callback URL pasted from the address bar.
 */
export interface OAuthFinish {
  nonce: string;
  /** New-account name; ignored when the flow was started with an attach
   *  target (`account_id` on start). */
  name?: string;
  code?: string;
  callback_url?: string;
}

/** One live share grant on an account: who it's shared with + since
 *  when. No secrets; `user_name` is the grantee's login joined for display. */
export interface ShareInfo {
  account_id: string;
  user_id: string;
  user_name: string;
  action: string;
  granted_at: string;
}

/** Grant payload. `user` is a UUID or a login; `action` defaults to
 *  `use` server-side. */
export interface GrantShare {
  user: string;
  action?: string;
}

/** One live share grant on any shareable resource: the polymorphic
 *  generalization of {@link ShareInfo}. `resource_type` is the DB kind
 *  (`account` | `machine` | `dispatcher` | `context_pack`). */
export interface ResourceShareInfo {
  resource_type: string;
  resource_id: string;
  user_id: string;
  user_name: string;
  action: string;
  granted_at: string;
}

/** A file the user uploaded into a session, kept in the blob store so the
 *  conversation can show it again (`GET /sessions/{id}/attachments`). */
export interface SessionAttachment {
  id: string;
  session_id: string;
  message_id: string | null;
  name: string;
  hash: string;
  size: number;
  content_type: string | null;
  /** Epoch ms. */
  created_at: number;
  /** The session's machine, null until the worker has registered. Lets a chip
   *  fall back to the staged copy when the blob store no longer has the bytes. */
  machine_id: string | null;
}

export const ATTACHMENT_CLOCK_SLACK_MS = 60_000;

/** The upload a user message refers to by `name`: the newest one recorded
 *  before the message (plus clock slack), else the earliest of that name. */
export function pickAttachment(
  all: SessionAttachment[],
  name: string,
  messageTs: number,
): SessionAttachment | null {
  const same = all.filter((a) => a.name === name);
  if (same.length === 0) return null;
  const before = same.filter(
    (a) => a.created_at <= messageTs + ATTACHMENT_CLOCK_SLACK_MS,
  );
  if (before.length)
    return before.reduce((best, a) => (a.created_at > best.created_at ? a : best));
  return same.reduce((best, a) => (a.created_at < best.created_at ? a : best));
}

export function attachmentBlobUrl(sessionId: string, hash: string): string {
  return `/api/v1/sessions/${encodeURIComponent(sessionId)}/blobs/${encodeURIComponent(hash)}`;
}
