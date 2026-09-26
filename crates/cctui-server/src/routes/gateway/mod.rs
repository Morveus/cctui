//! `/gateway/anthropic/*` (and `/gateway/openai/*`) — the OAuth passthrough
//! gateway.
//!
//! This is a **pure passthrough** that owns only OAuth storage + refresh:
//!
//!   1. The worker carries a session-scoped cctui token (minted at spawn, mapped
//!      to `(session_id, account_id)`), sent as the upstream `Authorization`
//!      bearer (`ANTHROPIC_AUTH_TOKEN`).
//!   2. Per request we map that token → account, swap `Authorization` to the
//!      account's current OAuth access token (refreshing under a per-account
//!      mutex when near expiry), and stream the bytes both ways. Every other
//!      client header is preserved verbatim.
//!   3. Status codes, `retry-after`, overload/streaming reconnects pass through
//!      untouched — the harness handles backoff exactly as if talking upstream
//!      directly. **No retries, no rate-limit handling** — with one exception:
//!      when the bound account is out of allocation (soft-limit refusal or an
//!      upstream 429) and an explicit redirect rule names another account, the
//!      session is rebound to it and the worker's own 429 retry lands there
//!      ([`failover`], opt-in via `CCTUI_GATEWAY_FAILOVER`).
//!
//! Request bodies stream through unread unless Langfuse samples the call or a
//! Fireworks account opts into shaping ([`FireworksSettings`]); those buffer,
//! and only Fireworks re-serializes. The anthropic path forwards the client's
//! bytes verbatim under every feature — re-serializing sorts JSON keys and
//! destroys the prompt cache. Response bodies are never rewritten.
//!
//! Stats are opportunistic: request count + byte count, never buffered parsing.
//! Raw OAuth tokens never enter worker env, logs, or session records.

mod config;
mod failover;
mod limits;
mod mint;
mod proxy;
mod ratelimit;
mod refresh;
mod usage;
pub mod usage_notices;

pub use config::*;
pub use failover::*;
pub use limits::*;
pub use mint::*;
pub use proxy::*;
pub use ratelimit::*;
pub use refresh::*;
pub use usage::*;

/// Resolve the database a DB-gated test should run against.
///
/// Locally a missing URL skips the test. Under CI it panics instead: a silent
/// skip there means the gate never ran and the suite passes green having
/// exercised none of the SQL.
#[cfg(test)]
pub fn test_db_url(test_name: &str) -> Option<String> {
    let url = ["DATABASE_URL", "TEST_DATABASE_URL"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty());

    if url.is_none() {
        assert!(
            std::env::var_os("CI").is_none(),
            "{test_name}: DATABASE_URL/TEST_DATABASE_URL must point at a migrated database in CI"
        );
        eprintln!("skipping {test_name}: no DATABASE_URL/TEST_DATABASE_URL");
    }
    url
}

#[cfg(test)]
mod tests {
    use super::{
        AuthStage, Family, FireworksSettings, OrphanSpamMap, access_token_is_fresh,
        apply_anthropic_cache_defaults, apply_gateway_env, auth_error, bump_orphan_401,
        clear_orphan_fingerprint, failover_retry_response, map_wham_usage, merge_session_budget,
        needs_rebind, orphan_is_blocked_at, resolve_catalog_model, skip_request_header,
        skip_response_header, tees_response, ttl_hours_from, usage_cache_stale, window_utilization,
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    #[test]
    fn access_token_fresh_only_when_present_and_well_before_expiry() {
        let now = Utc::now();
        assert!(access_token_is_fresh(Some("tok"), Some(now + ChronoDuration::minutes(10)), now));
        assert!(!access_token_is_fresh(Some(""), Some(now + ChronoDuration::minutes(10)), now));
        assert!(!access_token_is_fresh(None, Some(now + ChronoDuration::minutes(10)), now));
        assert!(
            !access_token_is_fresh(Some("tok"), None, now),
            "NULL expiry is stale, not forever"
        );
        assert!(!access_token_is_fresh(Some("tok"), Some(now + ChronoDuration::seconds(30)), now));
        assert!(!access_token_is_fresh(Some("tok"), Some(now - ChronoDuration::seconds(1)), now));
        assert!(
            !access_token_is_fresh(
                Some("tok"),
                Some(now + ChronoDuration::seconds(super::REFRESH_SKEW_SECS)),
                now,
            ),
            "the skew boundary itself is stale (strictly-greater)"
        );
        assert!(access_token_is_fresh(
            Some("tok"),
            Some(now + ChronoDuration::seconds(super::REFRESH_SKEW_SECS + 1)),
            now,
        ));
    }

    #[test]
    fn passthrough_strips_only_swapped_and_hop_by_hop_headers() {
        for h in ["authorization", "host", "content-length", "connection"] {
            assert!(skip_request_header(h), "{h} must not be forwarded upstream");
        }
        for h in ["content-type", "anthropic-beta", "user-agent", "x-custom"] {
            assert!(!skip_request_header(h), "{h} must be preserved verbatim");
        }
        for h in ["connection", "transfer-encoding", "content-length"] {
            assert!(skip_response_header(h), "{h} must not be mirrored back");
        }
        for h in ["content-type", "retry-after", "authorization"] {
            assert!(!skip_response_header(h), "{h} must be mirrored back untouched");
        }
    }

    #[test]
    fn window_utilization_is_tokens_over_budget_percent() {
        assert!((window_utilization(0, 8_000_000) - 0.0).abs() < f64::EPSILON);
        assert!((window_utilization(4_000_000, 8_000_000) - 50.0).abs() < f64::EPSILON);
        assert!((window_utilization(8_000_000, 8_000_000) - 100.0).abs() < f64::EPSILON);
        assert!(window_utilization(12_000_000, 8_000_000) > 100.0, "overshoot is not clamped");
    }

    #[test]
    fn anthropic_1h_cache_flag_defaults_on_and_is_overridable() {
        let mut env = BTreeMap::new();
        apply_anthropic_cache_defaults(&mut env);
        assert_eq!(env.get("ENABLE_PROMPT_CACHING_1H").map(String::as_str), Some("1"));

        let mut off = BTreeMap::new();
        off.insert("ENABLE_PROMPT_CACHING_1H".to_string(), "0".to_string());
        apply_anthropic_cache_defaults(&mut off);
        assert_eq!(off.get("ENABLE_PROMPT_CACHING_1H").map(String::as_str), Some("0"));
    }

    #[test]
    fn anthropic_1h_cache_flag_is_curated_in_catalog() {
        let e = crate::settings_catalog::catalog()
            .env("ENABLE_PROMPT_CACHING_1H")
            .expect("1h cache flag curated in the catalog");
        assert!(e.tag.account_exposable());
        assert!(!crate::settings_catalog::catalog().env_denylisted("ENABLE_PROMPT_CACHING_1H"));
    }

    #[test]
    fn wham_usage_maps_to_five_and_seven_windows() {
        // Real-shaped `wham/usage` body: primary=5h, secondary=7d.
        let body = serde_json::json!({
            "rate_limit": {
                "primary_window":   { "used_percent": 1,  "limit_window_seconds": 18_000,  "reset_at": 1_782_955_425i64 },
                "secondary_window": { "used_percent": 14, "limit_window_seconds": 604_800, "reset_at": 1_783_403_309i64 },
            }
        });
        let mapped = map_wham_usage(&body).expect("rate_limit present");
        assert_eq!(mapped["five_hour"]["utilization"].as_f64(), Some(1.0));
        assert_eq!(mapped["seven_day"]["utilization"].as_f64(), Some(14.0));
        // Epoch seconds → rfc3339 (stable server reset, not client-drifted).
        assert_eq!(mapped["five_hour"]["resets_at"].as_str(), Some("2026-07-02T01:23:45+00:00"));
        assert_eq!(mapped["seven_day"]["resets_at"].as_str(), Some("2026-07-07T05:48:29+00:00"));
    }

    #[test]
    fn wham_usage_none_without_rate_limit() {
        // No rate_limit (or a partial body) → None so the caller falls back local.
        assert!(map_wham_usage(&serde_json::json!({ "user_id": "u" })).is_none());
        assert!(
            map_wham_usage(&serde_json::json!({ "rate_limit": { "primary_window": {} } }))
                .is_none()
        );
    }

    #[test]
    fn orphan_spam_blocks_after_threshold_and_skips_db() {
        let map = OrphanSpamMap::new();
        let now = Instant::now();
        let window = Duration::from_mins(1);
        let block = Duration::from_mins(5);
        let fp = "deadbeef";

        // Below threshold: counts climb, never blocked.
        for i in 1..3 {
            let (count, newly) = bump_orphan_401(&map, fp, now, 3, window, block);
            assert_eq!(count, i);
            assert!(!newly);
            assert!(!orphan_is_blocked_at(&map, fp, now));
        }
        // Crossing the threshold flags it exactly once.
        let (count, newly) = bump_orphan_401(&map, fp, now, 3, window, block);
        assert_eq!(count, 3);
        assert!(newly, "should flag on the threshold-crossing call");
        assert!(orphan_is_blocked_at(&map, fp, now));

        // Still blocked mid-block-window, and re-flagging does not re-fire.
        let mid = now + Duration::from_mins(2);
        assert!(orphan_is_blocked_at(&map, fp, mid));
        let (_, newly_again) = bump_orphan_401(&map, fp, mid, 3, window, block);
        assert!(!newly_again);

        // After the block expires, the fingerprint is clear again.
        let after = now + block + Duration::from_secs(1);
        assert!(!orphan_is_blocked_at(&map, fp, after));
    }

    #[test]
    fn orphan_spam_unknown_fingerprint_is_not_blocked() {
        let map = OrphanSpamMap::new();
        assert!(!orphan_is_blocked_at(&map, "nope", Instant::now()));
    }

    #[test]
    fn rebind_clears_a_blocked_fingerprint_immediately() {
        // An account rebind reuses the SAME token string, so a
        // fingerprint blocked while the binding was broken must be cleared on
        // rebind — otherwise the just-fixed binding keeps 401ing for the
        // remainder of the (up to 300s) block window.
        let map = OrphanSpamMap::new();
        let now = Instant::now();
        let window = Duration::from_mins(1);
        let block = Duration::from_mins(5);
        let fp = "deadbeef";
        for _ in 0..3 {
            bump_orphan_401(&map, fp, now, 3, window, block);
        }
        assert!(orphan_is_blocked_at(&map, fp, now), "precondition: fp is blocked");

        clear_orphan_fingerprint(&map, fp);
        // No longer blocked — the next gateway request goes back to the DB
        // lookup instead of being dropped.
        assert!(!orphan_is_blocked_at(&map, fp, now));
        // And the window restarts from scratch: one fresh 401 doesn't re-block.
        let (count, newly) = bump_orphan_401(&map, fp, now, 3, window, block);
        assert_eq!(count, 1);
        assert!(!newly);
        assert!(!orphan_is_blocked_at(&map, fp, now));
    }

    #[test]
    fn auth_error_distinguishes_session_token_from_provider_oauth() {
        // the two 401s must be tellable apart — different stage header
        // and a message naming which credential to fix.
        let session = auth_error(AuthStage::SessionToken, true);
        let provider = auth_error(AuthStage::ProviderOauth, true);
        assert_eq!(session.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(provider.status(), axum::http::StatusCode::UNAUTHORIZED);
        assert_eq!(session.headers().get("x-cctui-auth-stage").unwrap(), "session-token");
        assert_eq!(provider.headers().get("x-cctui-auth-stage").unwrap(), "provider-oauth");
    }

    #[test]
    fn auth_error_uses_native_error_envelope_per_family() {
        // Anthropic: top-level `type:error`; OpenAI: bare `error` object. The CLI
        // only renders the message when the envelope matches its provider.
        let anthropic = auth_error(AuthStage::SessionToken, true);
        let openai = auth_error(AuthStage::SessionToken, false);
        assert_eq!(
            anthropic.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            openai.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }

    #[test]
    fn cold_usage_cache_is_stale() {
        // No cached usage must force a refresh, not be
        // treated as "no data → allow". This is what let a capped account hit 100%
        // on the headless dispatch path where the accounts page never warms it.
        assert!(usage_cache_stale(None, Duration::from_mins(3)));
    }

    #[test]
    fn fresh_usage_cache_is_not_stale() {
        assert!(!usage_cache_stale(Some(Duration::from_secs(10)), Duration::from_mins(3)));
    }

    #[test]
    fn expired_usage_cache_is_stale() {
        // At/over the TTL → refresh (so a capped account re-checks within one TTL).
        assert!(usage_cache_stale(Some(Duration::from_mins(3)), Duration::from_mins(3)));
        assert!(usage_cache_stale(Some(Duration::from_mins(10)), Duration::from_mins(3)));
    }

    /// No refresh storm: the push rides the same staleness gate as the fetch, so
    /// however hot the gateway path gets, an account can emit at most one
    /// upstream call — and therefore one push — per TTL.
    #[test]
    fn a_fresh_usage_cache_neither_fetches_nor_pushes() {
        let ttl = Duration::from_mins(3);
        for age in [0_u64, 1, 30, 179] {
            assert!(
                !usage_cache_stale(Some(Duration::from_secs(age)), ttl),
                "an entry {age}s old must be served from cache, broadcasting nothing"
            );
        }
        assert!(usage_cache_stale(Some(Duration::from_mins(3)), ttl));
    }

    #[test]
    fn family_from_provider_maps_native_and_compatible() {
        // both native and `-compatible` providers collapse to a family.
        assert!(matches!(Family::from_provider("anthropic"), Family::Anthropic));
        assert!(matches!(Family::from_provider("anthropic-compatible"), Family::Anthropic));
        assert!(matches!(Family::from_provider("openai"), Family::Openai));
        assert!(matches!(Family::from_provider("openai-compatible"), Family::Openai));
    }

    #[test]
    fn family_from_adapter_is_the_spawn_resolution_key() {
        // the adapter id names the harness family spawn resolves the
        // account's provider row by.
        assert!(matches!(Family::from_adapter("codex"), Family::Openai));
        assert!(matches!(Family::from_adapter("codex-foo"), Family::Openai));
        assert!(matches!(Family::from_adapter("claude-code"), Family::Anthropic));
    }

    #[test]
    fn provider_row_family_and_label_line_up() {
        // (account, family) resolution picks rows via ProviderRow::family;
        // labels feed the "no <family> provider" 404s.
        let anthropic = super::ProviderRow {
            id: uuid::Uuid::new_v4(),
            provider: "anthropic-compatible".into(),
            model_aliases: None,
            models: None,
        };
        let openai = super::ProviderRow {
            id: uuid::Uuid::new_v4(),
            provider: "openai".into(),
            model_aliases: None,
            models: None,
        };
        let fireworks = super::ProviderRow {
            id: uuid::Uuid::new_v4(),
            provider: "fireworks".into(),
            model_aliases: None,
            models: None,
        };
        assert!(matches!(anthropic.family(), Family::Anthropic));
        assert!(matches!(openai.family(), Family::Openai));
        assert!(matches!(fireworks.family(), Family::Fireworks));
        assert_eq!(Family::Anthropic.label(), "anthropic");
        assert_eq!(Family::Openai.label(), "openai");
        assert_eq!(Family::Fireworks.label(), "fireworks");
    }

    #[test]
    fn fireworks_is_its_own_family_not_openai() {
        // The whole point of the third family: `fireworks` speaks the OpenAI
        // wire protocol but must never collapse onto the openai credential slot,
        // or the unique (account_id, family) index would forbid holding both.
        assert_eq!(Family::from_provider("fireworks"), Family::Fireworks);
        assert_ne!(Family::from_provider("fireworks"), Family::Openai);
        assert_eq!(Family::from_adapter("opencode"), Family::Fireworks);
        assert_eq!(Family::from_adapter("opencode-cli"), Family::Fireworks);
        assert_eq!(Family::from_label("fireworks"), Some(Family::Fireworks));
        assert_eq!(Family::from_label("nope"), None);
    }

    #[test]
    fn gateway_env_keys_are_disjoint_across_families() {
        // A worker may carry all three at once; overlapping keys would make the
        // last mint silently win and 401 the others.
        let env_for = |family| {
            let mut env = std::collections::BTreeMap::new();
            apply_gateway_env(&mut env, family, "https://cctui.example", "cctui_s_tok".into());
            env
        };
        let anthropic = env_for(Family::Anthropic);
        let openai = env_for(Family::Openai);
        let fireworks = env_for(Family::Fireworks);
        assert_eq!(
            fireworks.get("FIREWORKS_BASE_URL").map(String::as_str),
            Some("https://cctui.example/gateway/fireworks")
        );
        assert_eq!(fireworks.get("FIREWORKS_API_KEY").map(String::as_str), Some("cctui_s_tok"));
        for other in [&anthropic, &openai] {
            assert!(other.keys().all(|k| !fireworks.contains_key(k)));
        }
    }

    #[test]
    fn fireworks_settings_default_and_override() {
        let defaults = FireworksSettings::resolve(None);
        assert_eq!(defaults.context_length_exceeded_behavior.as_deref(), Some("error"));
        assert!(defaults.session_affinity);
        assert!(defaults.extra_body.is_empty());

        // A partial stored blob overrides only the keys it names.
        let stored = serde_json::json!({
            "session_affinity": false,
            "extra_body": { "temperature": 0.2 },
        });
        let merged = FireworksSettings::resolve(Some(&stored));
        assert_eq!(merged.context_length_exceeded_behavior.as_deref(), Some("error"));
        assert!(!merged.session_affinity);
        assert_eq!(merged.extra_body.get("temperature"), Some(&serde_json::json!(0.2)));

        // An explicit null opts the injection out entirely.
        let off = serde_json::json!({ "context_length_exceeded_behavior": null });
        assert!(FireworksSettings::resolve(Some(&off)).context_length_exceeded_behavior.is_none());
    }

    #[test]
    fn fireworks_body_injection_never_overrides_the_client() {
        let settings = FireworksSettings::resolve(Some(&serde_json::json!({
            "extra_body": { "temperature": 0.2 },
        })));
        let mut body = serde_json::json!({ "model": "kimi", "messages": [] });
        settings.apply_body(&mut body, Some("sess-1"));
        assert_eq!(body["context_length_exceeded_behavior"], serde_json::json!("error"));
        assert_eq!(body["temperature"], serde_json::json!(0.2));
        assert_eq!(body["user"], serde_json::json!("sess-1"));

        let mut explicit = serde_json::json!({
            "context_length_exceeded_behavior": "truncate",
            "temperature": 1.0,
            "user": "mine",
        });
        settings.apply_body(&mut explicit, Some("sess-1"));
        assert_eq!(explicit["context_length_exceeded_behavior"], serde_json::json!("truncate"));
        assert_eq!(explicit["temperature"], serde_json::json!(1.0));
        assert_eq!(explicit["user"], serde_json::json!("mine"));
    }

    #[test]
    fn fireworks_affinity_off_leaves_user_alone() {
        let settings =
            FireworksSettings::resolve(Some(&serde_json::json!({ "session_affinity": false })));
        let mut body = serde_json::json!({ "model": "kimi" });
        settings.apply_body(&mut body, Some("sess-1"));
        assert!(body.get("user").is_none());
    }

    #[test]
    fn catalog_resolves_id_label_and_falls_back() {
        let catalog = serde_json::json!([
            { "model": "accounts/fireworks/models/kimi-k3", "label": "Kimi K3" },
            { "model": "accounts/fireworks/models/kimi-k2p6", "label": "Kimi K2.6" },
        ]);
        assert_eq!(
            resolve_catalog_model(Some(&catalog), "accounts/fireworks/models/kimi-k2p6").as_deref(),
            Some("accounts/fireworks/models/kimi-k2p6")
        );
        assert_eq!(
            resolve_catalog_model(Some(&catalog), "Kimi K3").as_deref(),
            Some("accounts/fireworks/models/kimi-k3")
        );
        // Unknown / empty falls back to the first entry rather than sending a
        // model id Fireworks would reject.
        assert_eq!(
            resolve_catalog_model(Some(&catalog), "gpt-5").as_deref(),
            Some("accounts/fireworks/models/kimi-k3")
        );
        assert_eq!(
            resolve_catalog_model(Some(&catalog), "").as_deref(),
            Some("accounts/fireworks/models/kimi-k3")
        );
        assert_eq!(resolve_catalog_model(None, "x"), None);
        assert_eq!(resolve_catalog_model(Some(&serde_json::json!([])), "x"), None);
    }

    #[test]
    fn session_token_ttl_defaults_and_honors_positive_override() {
        assert_eq!(ttl_hours_from(None), 12);
        assert_eq!(ttl_hours_from(Some("6".into())), 6);
        // Zero / negative / garbage all fall back to the default rather than
        // minting an already-dead (or never-expiring) token.
        assert_eq!(ttl_hours_from(Some("0".into())), 12);
        assert_eq!(ttl_hours_from(Some("-3".into())), 12);
        assert_eq!(ttl_hours_from(Some("nope".into())), 12);
    }

    /// DB-gated: the gateway auth lookup must refuse an expired session token
    /// (past `expires_at`) while resolving a live one, and a NULL `expires_at`
    /// (legacy row) must still resolve. Runs the exact enforcement predicate the
    /// passthrough / `token-valid` queries share. Skips without a database.
    #[tokio::test]
    async fn expired_session_token_is_not_resolved() {
        let Some(url) = super::test_db_url("expired_session_token_is_not_resolved") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = uuid::Uuid::new_v4();
        let acct = uuid::Uuid::new_v4();
        let prov = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("ttl-test-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
            .bind(acct)
            .bind(uid)
            .bind("ttl-test-acct")
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

        let resolves = |hash: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS ( \
                        SELECT 1 FROM session_tokens t \
                          JOIN account_providers a ON a.id = t.account_id \
                         WHERE t.token_hash = $1 AND t.revoked_at IS NULL \
                           AND (t.expires_at IS NULL OR t.expires_at > now()))",
                )
                .bind(hash)
                .fetch_one(&pool)
                .await
                .expect("resolve query")
            }
        };
        let seed_tok = |hash: &'static str, expires: Option<chrono::DateTime<chrono::Utc>>| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO session_tokens (token_hash, session_id, account_id, expires_at) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(hash)
                .bind(format!("sess-{hash}"))
                .bind(prov)
                .bind(expires)
                .execute(&pool)
                .await
                .expect("seed token");
            }
        };

        seed_tok("ttl-live", Some(chrono::Utc::now() + chrono::Duration::hours(1))).await;
        seed_tok("ttl-dead", Some(chrono::Utc::now() - chrono::Duration::hours(1))).await;
        seed_tok("ttl-null", None).await;

        assert!(resolves("ttl-live").await, "unexpired token must resolve");
        assert!(!resolves("ttl-dead").await, "expired token must NOT resolve");
        assert!(resolves("ttl-null").await, "legacy NULL-expiry token must still resolve");

        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }

    /// DB-gated: the observed-identity signal — a token stamped `last_used_at`
    /// (as the gateway does on a successful passthrough) flips the session into
    /// the "traffic observed" set the session list derives; an unstamped bound
    /// token stays out of it (the warning state). Skips without a database.
    #[tokio::test]
    async fn last_used_stamp_drives_observed_traffic() {
        let Some(url) = super::test_db_url("last_used_stamp_drives_observed_traffic") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = uuid::Uuid::new_v4();
        let acct = uuid::Uuid::new_v4();
        let prov = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("obs-test-{uid}"))
            .bind(format!("kh-obs-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, 'obs-acct')")
            .bind(acct)
            .bind(uid)
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

        let sid = format!("obs-sess-{uid}");
        sqlx::query(
            "INSERT INTO session_tokens (token_hash, session_id, account_id) VALUES ($1, $2, $3)",
        )
        .bind(format!("obs-hash-{uid}"))
        .bind(&sid)
        .bind(prov)
        .execute(&pool)
        .await
        .expect("seed token");

        let observed = |sid: String| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS ( \
                        SELECT 1 FROM session_tokens \
                         WHERE session_id = $1 AND revoked_at IS NULL AND last_used_at IS NOT NULL)",
                )
                .bind(&sid)
                .fetch_one(&pool)
                .await
                .expect("observed query")
            }
        };
        assert!(!observed(sid.clone()).await, "unstamped bound token → no traffic observed (warn)");

        sqlx::query(
            "UPDATE session_tokens SET last_used_at = now() \
             WHERE token_hash = $1 \
               AND (last_used_at IS NULL OR last_used_at < now() - interval '60 seconds')",
        )
        .bind(format!("obs-hash-{uid}"))
        .execute(&pool)
        .await
        .expect("stamp last_used");
        assert!(observed(sid.clone()).await, "stamped token → traffic observed (no warn)");

        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }

    #[test]
    fn child_budget_becomes_a_session_usd_cap() {
        let merged = merge_session_budget(&crate::soft_limit::SoftLimits::default(), Some(0.75));
        assert_eq!(
            merged.limits[crate::soft_limit::KEY_SESSION_USD].cap_usd,
            Some(0.75),
            "the child's budget must enforce as a session_usd cap"
        );
    }

    #[test]
    fn child_budget_overrides_a_looser_account_cap_and_keeps_other_windows() {
        let account = crate::soft_limit::SoftLimits::from_json(Some(&serde_json::json!({
            "session_usd": { "cap_usd": 10.0 },
            "usd_7d": { "cap_usd": 50.0 },
        })));
        let merged = merge_session_budget(&account, Some(2.0));
        assert_eq!(merged.limits[crate::soft_limit::KEY_SESSION_USD].cap_usd, Some(2.0));
        assert_eq!(merged.limits[crate::soft_limit::KEY_USD_7D].cap_usd, Some(50.0));
    }

    #[test]
    fn no_or_invalid_budget_leaves_the_account_limits_untouched() {
        let account = crate::soft_limit::SoftLimits::from_json(Some(&serde_json::json!({
            "usd_5h": { "cap_usd": 3.0 },
        })));
        for budget in [None, Some(0.0), Some(-1.0), Some(f64::NAN)] {
            let merged = merge_session_budget(&account, budget);
            assert_eq!(merged, account, "budget {budget:?} must not alter the account limits");
        }
    }

    #[test]
    fn fireworks_alone_still_forces_an_identity_encoded_response() {
        assert!(tees_response(false, true), "an unsampled Fireworks call is teed for its usage");
        assert!(tees_response(true, false));
        assert!(tees_response(true, true));
        assert!(!tees_response(false, false), "an unobserved call stays a zero-copy passthrough");
    }

    #[test]
    fn rebind_only_fires_when_the_harness_renamed_the_session() {
        assert!(needs_rebind("6e0e-4d7d", "ses_058283e9"));
        assert!(!needs_rebind("6e0e-4d7d", "6e0e-4d7d"), "claude/codex local id IS the spawn key");
        assert!(!needs_rebind("", "ses_058283e9"), "no spawn key ⇒ nothing to re-key");
        assert!(!needs_rebind("6e0e-4d7d", ""));
    }

    /// DB-gated: the whole reason the rebind exists. An opencode child pulls its
    /// gateway env under the spawn UUID, then registers under a `ses_…` id the
    /// harness picked. Until the token is moved onto that id, every usage insert
    /// violates `session_token_usage.session_id → sessions(id)` and the child's
    /// spend silently reads $0. Skips without a database.
    #[tokio::test]
    async fn rebind_makes_a_renamed_session_meterable() {
        let Some(url) = super::test_db_url("rebind_makes_a_renamed_session_meterable") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = uuid::Uuid::new_v4();
        let acct = uuid::Uuid::new_v4();
        let prov = uuid::Uuid::new_v4();
        let spawn_key = uuid::Uuid::new_v4().to_string();
        let native_id = format!("ses_{}", uuid::Uuid::new_v4().simple());

        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("rebind-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
            .bind(acct)
            .bind(uid)
            .bind("rebind-acct")
            .execute(&pool)
            .await
            .expect("seed account");
        sqlx::query(
            "INSERT INTO account_providers \
                 (id, user_id, provider, encrypted_refresh_token, account_id) \
             VALUES ($1, $2, 'fireworks', 'x', $3)",
        )
        .bind(prov)
        .bind(uid)
        .bind(acct)
        .execute(&pool)
        .await
        .expect("seed provider");
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, adapter_id) \
             VALUES ($1, 'm1', '/w', $2, 'opencode')",
        )
        .bind(&native_id)
        .bind(uid)
        .execute(&pool)
        .await
        .expect("seed session under the harness id");
        sqlx::query(
            "INSERT INTO session_tokens (token_hash, session_id, account_id) VALUES ($1, $2, $3)",
        )
        .bind(format!("th-{spawn_key}"))
        .bind(&spawn_key)
        .bind(prov)
        .execute(&pool)
        .await
        .expect("seed token under the spawn key");

        let record = |sid: String| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO session_token_usage \
                         (session_id, message_id, input_tokens, output_tokens, model) \
                     VALUES ($1, $2, 1, 1, 'accounts/fireworks/models/kimi-k3')",
                )
                .bind(sid)
                .bind(format!("m-{}", uuid::Uuid::new_v4().simple()))
                .execute(&pool)
                .await
            }
        };

        assert!(
            record(spawn_key.clone()).await.is_err(),
            "usage under the un-rebound spawn key must violate the sessions FK"
        );

        crate::store::tokens::rebind_session_id(&pool, &spawn_key, &native_id)
            .await
            .expect("rebind");

        let bound: String =
            sqlx::query_scalar("SELECT session_id FROM session_tokens WHERE token_hash = $1")
                .bind(format!("th-{spawn_key}"))
                .fetch_one(&pool)
                .await
                .expect("read back token");
        assert_eq!(bound, native_id, "the token must follow the harness id");

        record(native_id.clone()).await.expect("usage under the rebound id must record");

        let metered: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM session_token_usage stu \
               JOIN session_tokens st ON st.session_id = stu.session_id \
              WHERE st.account_id = $1",
        )
        .bind(prov)
        .fetch_one(&pool)
        .await
        .expect("count metered rows");
        assert_eq!(metered, 1, "the account's usage window must see the child's spend");

        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }

    /// DB-gated: `session_usd` caps one session, so the account-level figure must
    /// be the dearest single session, never the account total — reporting the sum
    /// would show a cap breached while every session was under it.
    #[tokio::test]
    async fn session_usd_reports_the_dearest_session_not_the_account_total() {
        let Some(url) =
            super::test_db_url("session_usd_reports_the_dearest_session_not_the_account_total")
        else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = uuid::Uuid::new_v4();
        let acct = uuid::Uuid::new_v4();
        let prov = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("maxsess-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
            .bind(acct)
            .bind(uid)
            .bind("maxsess-acct")
            .execute(&pool)
            .await
            .expect("seed account");
        sqlx::query(
            "INSERT INTO account_providers \
                 (id, user_id, provider, encrypted_refresh_token, account_id) \
             VALUES ($1, $2, 'fireworks', 'x', $3)",
        )
        .bind(prov)
        .bind(uid)
        .bind(acct)
        .execute(&pool)
        .await
        .expect("seed provider");

        // 1 Mtok of input at $3/Mtok, so each session's spend is its multiplier.
        for (n, mtok) in [(1u32, 1_000_000_i64), (2, 3_000_000), (3, 2_000_000)] {
            let sid = format!("ses_maxsess_{uid}_{n}");
            sqlx::query(
                "INSERT INTO sessions (id, machine_id, working_dir, user_id, adapter_id) \
                 VALUES ($1, 'm1', '/w', $2, 'opencode')",
            )
            .bind(&sid)
            .bind(uid)
            .execute(&pool)
            .await
            .expect("seed session");
            sqlx::query(
                "INSERT INTO session_tokens (token_hash, session_id, account_id) \
                 VALUES ($1, $2, $3)",
            )
            .bind(format!("th-maxsess-{uid}-{n}"))
            .bind(&sid)
            .bind(prov)
            .execute(&pool)
            .await
            .expect("seed token");
            sqlx::query(
                "INSERT INTO session_token_usage \
                     (session_id, message_id, input_tokens, output_tokens, model) \
                 VALUES ($1, $2, $3, 0, 'accounts/fireworks/models/kimi-k3')",
            )
            .bind(&sid)
            .bind(format!("m-maxsess-{uid}-{n}"))
            .bind(mtok)
            .execute(&pool)
            .await
            .expect("seed usage");
        }

        let catalog = serde_json::json!([
            { "model": "accounts/fireworks/models/kimi-k3", "price_input_per_mtok": 3.0 }
        ]);
        let top = super::max_session_spend_usd(&pool, prov, Some(&catalog), "5 hours")
            .await
            .expect("a metered account reports a dearest session");
        assert!(
            (top - 9.0).abs() < 1e-6,
            "expected the 3 Mtok session ($9), got {top} (account total would be $18)"
        );

        let unmetered = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO account_providers \
                 (id, user_id, provider, encrypted_refresh_token, account_id) \
             VALUES ($1, $2, 'openai', 'x', $3)",
        )
        .bind(unmetered)
        .bind(uid)
        .bind(acct)
        .execute(&pool)
        .await
        .expect("seed second provider");
        assert_eq!(
            super::max_session_spend_usd(&pool, unmetered, Some(&catalog), "5 hours").await,
            None,
            "an account that metered nothing reports None, which the window renders as $0"
        );

        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }

    /// DB-gated: a capability must outlive the server process that recorded it,
    /// and must follow a renamed session, or the daemon stops offering
    /// `CctuiAgent` and the reviewer goes missing with no error anywhere.
    #[tokio::test]
    async fn spawn_capability_survives_restart_and_rebind() {
        let Some(url) = super::test_db_url("spawn_capability_survives_restart_and_rebind") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let spawn_key = uuid::Uuid::new_v4().to_string();
        let native_id = format!("ses_{}", uuid::Uuid::new_v4().simple());
        let cap = cctui_proto::api::SpawnCapability {
            adapters: vec!["opencode".to_owned()],
            max_budget_usd: None,
            max_children: Some(3),
        };

        crate::store::spawn_capabilities::upsert(&pool, &spawn_key, &cap).await.expect("upsert");
        assert_eq!(
            crate::store::spawn_capabilities::get(&pool, &spawn_key).await.expect("get"),
            Some(cap.clone()),
            "a fresh process must read the capability back from the table"
        );

        crate::store::spawn_capabilities::rebind(&pool, &spawn_key, &native_id)
            .await
            .expect("rebind");
        assert_eq!(
            crate::store::spawn_capabilities::get(&pool, &native_id).await.expect("get"),
            Some(cap),
            "the capability must follow the harness id"
        );
        assert_eq!(
            crate::store::spawn_capabilities::get(&pool, &spawn_key).await.expect("get"),
            None,
            "nothing may be left under the spawn key"
        );

        crate::store::spawn_capabilities::delete(&pool, &native_id).await.expect("delete");
        assert_eq!(
            crate::store::spawn_capabilities::get(&pool, &native_id).await.expect("get"),
            None,
            "session end must drop the capability"
        );
    }

    /// DB-gated: usage captured through the gateway path for an opencode session
    /// lands under the session FK with its model stamped, so its priced spend is
    /// non-zero and a `session_usd` cap below it fires — the 429 the proxy returns.
    #[tokio::test]
    async fn gateway_captured_opencode_usage_trips_the_usd_cap() {
        let Some(url) = super::test_db_url("gateway_captured_opencode_usage_trips_the_usd_cap")
        else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");

        let uid = uuid::Uuid::new_v4();
        let acct = uuid::Uuid::new_v4();
        let prov = uuid::Uuid::new_v4();
        let session_id = format!("ses_{}", uuid::Uuid::new_v4().simple());
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("cap-{uid}"))
            .bind(format!("kh-{uid}"))
            .execute(&pool)
            .await
            .expect("seed user");
        sqlx::query("INSERT INTO accounts (id, user_id, name) VALUES ($1, $2, $3)")
            .bind(acct)
            .bind(uid)
            .bind(format!("cap-acct-{uid}"))
            .execute(&pool)
            .await
            .expect("seed account");
        sqlx::query(
            "INSERT INTO account_providers \
                 (id, user_id, provider, encrypted_refresh_token, account_id) \
             VALUES ($1, $2, 'fireworks', 'x', $3)",
        )
        .bind(prov)
        .bind(uid)
        .bind(acct)
        .execute(&pool)
        .await
        .expect("seed provider");
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, working_dir, user_id, adapter_id) \
             VALUES ($1, 'm1', '/w', $2, 'opencode')",
        )
        .bind(&session_id)
        .bind(uid)
        .execute(&pool)
        .await
        .expect("seed session");
        sqlx::query(
            "INSERT INTO session_tokens (token_hash, session_id, account_id) VALUES ($1, $2, $3)",
        )
        .bind(format!("th-{session_id}"))
        .bind(&session_id)
        .bind(prov)
        .execute(&pool)
        .await
        .expect("seed token");

        super::record_fireworks_usage(
            pool.clone(),
            session_id.clone(),
            Some("accounts/fireworks/models/kimi-k3".to_owned()),
            crate::cost::CapturedUsage {
                message_id: Some("gw-cap-1".to_owned()),
                usage: crate::cost::TokenUsage { input: 2_000_000, cached_input: 0, output: 0 },
            },
            false,
        )
        .await;

        // 2 Mtok input at $3/Mtok = $6 spent for this one session.
        let catalog = serde_json::json!([
            { "model": "accounts/fireworks/models/kimi-k3", "price_input_per_mtok": 3.0 }
        ]);
        let spent = super::max_session_spend_usd(&pool, prov, Some(&catalog), "5 hours")
            .await
            .expect("the captured row must price to a non-zero session spend");
        assert!((spent - 6.0).abs() < 1e-6, "expected $6 from the stamped row, got {spent}");

        let caps = crate::soft_limit::SoftLimits::from_json(Some(&serde_json::json!({
            "session_usd": { "cap_usd": 5.0 }
        })));
        let windows =
            vec![crate::soft_limit::usd_window(crate::soft_limit::KEY_SESSION_USD, spent, None)];
        match crate::soft_limit::evaluate_soft_limit(&windows, &caps, None, Utc::now()) {
            crate::soft_limit::Decision::Block { key, .. } => {
                assert_eq!(key, crate::soft_limit::KEY_SESSION_USD);
            }
            crate::soft_limit::Decision::Allow => {
                panic!("a $6 session against a $5 cap must block (429)")
            }
        }

        sqlx::query("DELETE FROM users WHERE id = $1").bind(uid).execute(&pool).await.ok();
    }

    #[test]
    fn failover_retry_response_sends_the_worker_straight_back() {
        let resp = failover_retry_response("Secours", "pool", true);
        assert_eq!(resp.status(), http::StatusCode::TOO_MANY_REQUESTS);
        let retry = resp.headers().get(http::header::RETRY_AFTER).unwrap();
        assert_eq!(retry, "1", "an immediate retry, not the exhausted window's horizon");
        assert_eq!(resp.headers().get("x-cctui-failover").unwrap(), "Secours");
        assert_eq!(resp.headers().get(http::header::CONTENT_TYPE).unwrap(), "application/json");
    }
}
