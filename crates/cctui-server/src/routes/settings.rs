//! `GET`/`PUT /api/v1/settings` — server-persisted per-user settings (//! epic).
//!
//! Each user owns a single row in `user_settings` holding a `version` and an
//! open JSON `data` blob. The blob is deliberately schema-less on the wire (we
//! store `serde_json::Value`, not a rigid struct) so the webui can grow new
//! keys without a server change or SQL migration.
//!
//! ## Versioning & lazy persistence
//!
//! `data` is versioned by an integer. Upgrades between payload shapes are
//! applied **in code** by [`migrate`] (NOT by SQL) — a pure, sequential chain
//! from the stored version up to [`CURRENT_VERSION`]. The chain runs on both
//! read and write:
//!   - On `GET` we upgrade the stored payload in memory so callers always see
//!     the current shape, but we do NOT write the upgraded row back. Persistence
//!     is **lazy**: the upgraded payload is written on the *next* `PUT` (the
//!     user's next settings edit), at which point the stored `version` advances.
//!     This keeps reads side-effect-free and avoids a write storm on deploy.
//!   - On `PUT` we upgrade the incoming payload to `CURRENT_VERSION` before the
//!     upsert and store that version, so writes are always current-shaped.
//!
//! ## Reserved keys
//!
//! `keymap` and `shortcutsEnabled` are reserved for the later keyboard-shortcuts
//! feature. Because `data` is an open JSON object, no work is needed now to
//! "reserve" them — adding those keys later is a no-op on the storage side.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;

use crate::auth::AuthContext;
use crate::state::AppState;

/// Current settings payload schema version. Bump when adding a `migrate` arm.
const CURRENT_VERSION: i32 = 1;

#[derive(Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SettingsPayload {
    pub version: i32,
    pub data: Value,
}

/// Upgrade a settings `data` payload from `from` up to [`CURRENT_VERSION`].
///
/// Pure function: applies sequential per-version transforms. For v1 there are no
/// prior versions, so this is a passthrough that simply clamps unknown/older
/// versions forward to the current shape. Adding a v1->v2 transform later is a
/// one-line `1 => { /* mutate data */ }` arm in the match.
fn migrate(mut data: Value, from: i32) -> Value {
    let mut v = from.max(0);
    while v < CURRENT_VERSION {
        upgrade_step(&mut data, v);
        v += 1;
    }
    data
}

/// Upgrade `data` in place from version `v` to `v + 1`. No prior versions exist
/// yet, so this is a no-op; add a `1 => { /* v1 -> v2 */ }` arm here when bumping
/// [`CURRENT_VERSION`].
const fn upgrade_step(data: &mut Value, v: i32) {
    let _ = (data, v);
}

/// The recognized harness modes. Anything else (typo, missing) clamps
/// to `bg` so a bad value can't wedge a daemon.
const HARNESS_MODES: [&str; 3] = ["bg", "sdk", "oneshot"];
const DEFAULT_HARNESS_MODE: &str = "bg";

/// Read the user-facing `harnessMode` from a settings `data` blob, clamping an
/// unknown/missing value to [`DEFAULT_HARNESS_MODE`].
fn harness_mode_of(data: &Value) -> &'static str {
    data.get("harnessMode").and_then(Value::as_str).map_or(DEFAULT_HARNESS_MODE, |m| {
        HARNESS_MODES.into_iter().find(|&v| v == m).unwrap_or(DEFAULT_HARNESS_MODE)
    })
}

/// Clamp `data.harnessMode` to a known value in place (rejecting typos), so a
/// stored row never carries an out-of-whitelist mode.
fn clamp_harness_mode(data: &mut Value) {
    let clamped = harness_mode_of(data);
    if let Some(obj) = data.as_object_mut() {
        // Only normalize when the key is present; absence means "default", which
        // we leave implicit rather than materializing.
        if obj.contains_key("harnessMode") {
            obj.insert("harnessMode".to_owned(), Value::String(clamped.to_owned()));
        }
    }
}

/// Map a user-facing harness mode (`bg`/`sdk`/`oneshot`, or unknown) to the
/// claude-code adapter's internal `config["mode"]` token. Today `bg`
/// is served by the existing `claude-daemon` path; `sdk`/`oneshot` pass through
/// as-is. Any unknown/missing value defaults to `bg`.
#[must_use]
pub fn harness_mode_to_adapter_token(harness_mode: Option<&str>) -> String {
    let mode = harness_mode.filter(|m| HARNESS_MODES.contains(m)).unwrap_or(DEFAULT_HARNESS_MODE);
    match mode {
        "bg" => "claude-daemon".to_owned(),
        other => other.to_owned(),
    }
}

/// Recognized UI locales. An unknown/missing value is left as the
/// implicit "auto" (no key), so the webui falls back to the browser language.
const LOCALES: [&str; 2] = ["en", "fr"];

/// Clamp `data.locale` in place on write: drop the key unless it is a
/// recognized locale, so a stored row never carries an unknown language token
/// (which the webui would ignore anyway). Absence means "auto".
fn clamp_locale(data: &mut Value) {
    let Some(obj) = data.as_object_mut() else { return };
    if !obj.contains_key("locale") {
        return;
    }
    let ok = obj.get("locale").and_then(Value::as_str).is_some_and(|l| LOCALES.contains(&l));
    if !ok {
        obj.remove("locale");
    }
}

const WHIP_MODES: [&str; 2] = ["extend", "replace"];
const DEFAULT_WHIP_MODE: &str = "extend";
const MAX_WHIP_PHRASES: usize = 200;
const MAX_WHIP_PHRASE_CHARS: usize = 200;
const MAX_WHIP_GUIDANCE_CHARS: usize = 2000;

/// Normalize a raw `whipStopPhrases` value into the clamped block
/// `{ mode, phrases, guidance? }`, or `None` when it reduces to the default
/// (`extend` + no phrases + no guidance) so an absent setting stays implicit.
///
/// Phrases are trimmed, lowercased (matching is case-insensitive substring), had
/// empties dropped, deduped, and capped in per-phrase length and count so a
/// pathological blob can't bloat every worker spawn.
fn normalize_whip_stop_phrases(raw: Option<&Value>) -> Option<Value> {
    let obj = raw.and_then(Value::as_object);
    let mode = obj
        .and_then(|o| o.get("mode"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| WHIP_MODES.contains(m))
        .unwrap_or(DEFAULT_WHIP_MODE);
    let mut phrases: Vec<String> = Vec::new();
    if let Some(arr) = obj.and_then(|o| o.get("phrases")).and_then(Value::as_array) {
        for p in arr {
            let Some(s) = p.as_str() else { continue };
            let s: String = s.trim().to_lowercase().chars().take(MAX_WHIP_PHRASE_CHARS).collect();
            if s.is_empty() || phrases.iter().any(|e| e == &s) {
                continue;
            }
            phrases.push(s);
            if phrases.len() >= MAX_WHIP_PHRASES {
                break;
            }
        }
    }
    let guidance = obj
        .and_then(|o| o.get("guidance"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(|g| g.chars().take(MAX_WHIP_GUIDANCE_CHARS).collect::<String>());
    if mode == DEFAULT_WHIP_MODE && phrases.is_empty() && guidance.is_none() {
        return None;
    }
    let mut block = serde_json::Map::new();
    block.insert("mode".to_owned(), Value::String(mode.to_owned()));
    block.insert(
        "phrases".to_owned(),
        Value::Array(phrases.into_iter().map(Value::String).collect()),
    );
    if let Some(g) = guidance {
        block.insert("guidance".to_owned(), Value::String(g));
    }
    Some(Value::Object(block))
}

/// Clamp `data.whipStopPhrases` in place on write, removing the key when
/// it reduces to the default so a stored row never carries a no-op block.
fn clamp_whip_stop_phrases(data: &mut Value) {
    let Some(obj) = data.as_object_mut() else { return };
    if !obj.contains_key("whipStopPhrases") {
        return;
    }
    match normalize_whip_stop_phrases(obj.get("whipStopPhrases")) {
        Some(v) => {
            obj.insert("whipStopPhrases".to_owned(), v);
        }
        None => {
            obj.remove("whipStopPhrases");
        }
    }
}

/// The clamped `whipStopPhrases` block for the daemon gateway-env pull,
/// or `None` when unset / reduced to the default (hook uses compiled defaults).
#[must_use]
pub fn whip_stop_phrases_of(data: &Value) -> Option<Value> {
    normalize_whip_stop_phrases(data.get("whipStopPhrases"))
}

const MAX_SCRUB_PATTERNS: usize = 100;
const MAX_SCRUB_NAME_CHARS: usize = 60;
const MAX_SCRUB_REGEX_CHARS: usize = 400;

/// Normalize `secretScrubPatterns` into a clamped array of
/// `{ name, regex, enabled }`. Each entry is trimmed, length-capped, and its
/// `regex` is **rejected unless it compiles** (returned in `Err` so `PUT` can
/// 400). Duplicates by regex are dropped and the count is capped. Returns an
/// empty vec when the key is absent or reduces to nothing.
fn normalize_scrub_patterns(raw: Option<&Value>) -> Result<Vec<Value>, String> {
    let Some(arr) = raw.and_then(Value::as_array) else { return Ok(Vec::new()) };
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for entry in arr {
        let name: String = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .chars()
            .take(MAX_SCRUB_NAME_CHARS)
            .collect();
        let regex: String = entry
            .get("regex")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .chars()
            .take(MAX_SCRUB_REGEX_CHARS)
            .collect();
        if regex.is_empty() {
            continue;
        }
        if let Err(e) = cctui_crypto::redact::validate_regex(&regex) {
            return Err(format!("invalid scrub regex {regex:?}: {e}"));
        }
        if seen.iter().any(|r| r == &regex) {
            continue;
        }
        let enabled = entry.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        seen.push(regex.clone());
        let name = if name.is_empty() { "custom".to_owned() } else { name };
        out.push(json!({ "name": name, "regex": regex, "enabled": enabled }));
        if out.len() >= MAX_SCRUB_PATTERNS {
            break;
        }
    }
    Ok(out)
}

/// Clamp `data.secretScrubEnabled` / `data.secretScrubPatterns` in place on
/// write, returning `Err(msg)` when a user regex does not compile so
/// `put_settings` can reject the PUT with a 400.
fn clamp_secret_scrub(data: &mut Value) -> Result<(), String> {
    let Some(obj) = data.as_object_mut() else { return Ok(()) };
    if obj.contains_key("secretScrubEnabled") {
        let on = obj.get("secretScrubEnabled").and_then(Value::as_bool).unwrap_or(true);
        obj.insert("secretScrubEnabled".to_owned(), Value::Bool(on));
    }
    if obj.contains_key("secretScrubPatterns") {
        let patterns = normalize_scrub_patterns(obj.get("secretScrubPatterns"))?;
        if patterns.is_empty() {
            obj.remove("secretScrubPatterns");
        } else {
            obj.insert("secretScrubPatterns".to_owned(), Value::Array(patterns));
        }
    }
    Ok(())
}

/// The effective [`SecretScrubConfig`] for the daemon Reconcile: the
/// enable flag plus the enabled, validated user patterns from a settings blob.
#[must_use]
pub fn secret_scrub_of(data: &Value) -> cctui_proto::ws::SecretScrubConfig {
    let enabled = data.get("secretScrubEnabled").and_then(Value::as_bool).unwrap_or(true);
    let patterns = normalize_scrub_patterns(data.get("secretScrubPatterns"))
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|p| {
            Some(cctui_proto::ws::ScrubPattern {
                name: p.get("name")?.as_str()?.to_owned(),
                regex: p.get("regex")?.as_str()?.to_owned(),
            })
        })
        .collect();
    cctui_proto::ws::SecretScrubConfig { enabled, patterns }
}

/// Clamp `data.sessionEmojiPrefix` in place on write: coerce a present value
/// to a real boolean so a stored row never carries a truthy string. Absence
/// means "off", which stays implicit.
///
/// The flag is read back in SQL, not here: `update_status_signals` joins
/// `user_settings` while it persists the agent-supplied session name (see
/// `crate::session_emoji`), so a status update costs no extra round-trip.
fn clamp_session_emoji_prefix(data: &mut Value) {
    let Some(obj) = data.as_object_mut() else { return };
    if obj.contains_key("sessionEmojiPrefix") {
        let on = obj.get("sessionEmojiPrefix").and_then(Value::as_bool).unwrap_or(false);
        obj.insert("sessionEmojiPrefix".to_owned(), Value::Bool(on));
    }
}

/// Clamp `data.autoResumeOnConnectionLoss` in place on write, like
/// [`clamp_session_emoji_prefix`]. Read back in SQL by `crate::auto_resume`,
/// which joins `user_settings` while it looks for stuck sessions.
const MAX_MACROS: usize = 100;
const MAX_MACRO_TITLE_CHARS: usize = 120;
const MAX_MACRO_PROMPT_CHARS: usize = 20_000;
const MAX_MACRO_FIELD_CHARS: usize = 400;

fn macro_str(entry: &Value, key: &str, max: usize) -> String {
    entry.get(key).and_then(Value::as_str).unwrap_or_default().trim().chars().take(max).collect()
}

/// Normalize the `macros` block (`{ enabled, items }`) the webui stores:
/// coerce `enabled` to a bool, keep only well-formed items (a title and a
/// prompt), cap lengths and count, and drop the key when nothing is left and
/// the feature is off. The server never runs a macro itself — the webui
/// turns one into a spawn — so this only guards the blob's size and shape.
fn clamp_macros(data: &mut Value) {
    let Some(obj) = data.as_object_mut() else { return };
    let Some(raw) = obj.get("macros") else { return };
    let enabled = raw.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let items: Vec<Value> = raw
        .get("items")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|e| e.is_object())
                .filter_map(|e| {
                    let title = macro_str(e, "title", MAX_MACRO_TITLE_CHARS);
                    let prompt = macro_str(e, "prompt", MAX_MACRO_PROMPT_CHARS);
                    if title.is_empty() || prompt.is_empty() {
                        return None;
                    }
                    let mut id = macro_str(e, "id", MAX_MACRO_FIELD_CHARS);
                    if id.is_empty() {
                        id = uuid::Uuid::new_v4().to_string();
                    }
                    let opt = |k: &str| {
                        let v = macro_str(e, k, MAX_MACRO_FIELD_CHARS);
                        if v.is_empty() { Value::Null } else { Value::String(v) }
                    };
                    let adapter = {
                        let a = macro_str(e, "adapter", MAX_MACRO_FIELD_CHARS);
                        if a.is_empty() { "claude-code".to_owned() } else { a }
                    };
                    Some(serde_json::json!({
                        "id": id,
                        "title": title,
                        "prompt": prompt,
                        "adapter": adapter,
                        "machine_id": opt("machine_id"),
                        "working_dir": opt("working_dir"),
                        "model": opt("model"),
                        "effort": opt("effort"),
                        "pool_id": opt("pool_id"),
                        "permission_mode": opt("permission_mode"),
                        "confirm": e.get("confirm").and_then(Value::as_bool).unwrap_or(true),
                    }))
                })
                .take(MAX_MACROS)
                .collect()
        })
        .unwrap_or_default();
    if !enabled && items.is_empty() {
        obj.remove("macros");
    } else {
        obj.insert("macros".to_owned(), serde_json::json!({ "enabled": enabled, "items": items }));
    }
}

fn clamp_auto_resume(data: &mut Value) {
    let Some(obj) = data.as_object_mut() else { return };
    if obj.contains_key("autoResumeOnConnectionLoss") {
        let on = obj.get("autoResumeOnConnectionLoss").and_then(Value::as_bool).unwrap_or(false);
        obj.insert("autoResumeOnConnectionLoss".to_owned(), Value::Bool(on));
    }
}

pub async fn get_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<SettingsPayload>, StatusCode> {
    let row = sqlx::query_as::<_, (i32, Value)>(
        "SELECT version, data FROM user_settings WHERE user_id = $1",
    )
    .bind(ctx.user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let payload = match row {
        // Upgrade in memory only — do NOT persist on read (lazy persistence; the
        // upgraded shape is written on the next PUT).
        Some((version, data)) => {
            SettingsPayload { version: CURRENT_VERSION, data: migrate(data, version) }
        }
        None => SettingsPayload { version: CURRENT_VERSION, data: json!({}) },
    };

    Ok(Json(payload))
}

pub async fn put_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<SettingsPayload>,
) -> Result<Json<SettingsPayload>, StatusCode> {
    // Upgrade the incoming payload to the current shape before persisting, so
    // stored rows are always current-versioned.
    let mut data = migrate(body.data, body.version);
    // Whitelist harnessMode on write so a typo can't be stored (and later
    // wedge a daemon's reconcile); unknown → bg.
    clamp_harness_mode(&mut data);
    clamp_whip_stop_phrases(&mut data);
    // Reject the whole PUT if any user scrub regex fails to compile.
    if let Err(msg) = clamp_secret_scrub(&mut data) {
        tracing::info!("rejecting settings PUT: {msg}");
        return Err(StatusCode::BAD_REQUEST);
    }
    clamp_locale(&mut data);
    clamp_session_emoji_prefix(&mut data);
    clamp_auto_resume(&mut data);
    clamp_macros(&mut data);
    let new_mode = harness_mode_of(&data);
    let new_scrub = serde_json::to_value(secret_scrub_of(&data)).unwrap_or(Value::Null);

    // Snapshot the stored harnessMode before the upsert so we only push a fresh
    // Reconcile when it actually changes — unrelated settings edits must not
    // trigger a reconcile storm across the user's machines.
    let prev: Option<Value> =
        sqlx::query_scalar("SELECT data FROM user_settings WHERE user_id = $1")
            .bind(ctx.user_id)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| {
                tracing::error!("db error: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    let prev_mode = prev.as_ref().map_or(DEFAULT_HARNESS_MODE, |d| harness_mode_of(d));
    let prev_scrub = prev
        .as_ref()
        .and_then(|d| serde_json::to_value(secret_scrub_of(d)).ok())
        .unwrap_or(Value::Null);

    let (version, data) = sqlx::query_as::<_, (i32, Value)>(
        "INSERT INTO user_settings (user_id, version, data, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (user_id) DO UPDATE \
         SET version = EXCLUDED.version, data = EXCLUDED.data, updated_at = now() \
         RETURNING version, data",
    )
    .bind(ctx.user_id)
    .bind(CURRENT_VERSION)
    .bind(data)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("db error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Live-push a fresh Reconcile to every machine the user owns the instant the
    // harness mode changes, so connected daemons pick up the new mode without a
    // reconnect. Best-effort, only on change.
    if new_mode != prev_mode || new_scrub != prev_scrub {
        let machines: Vec<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM machines WHERE user_id = $1 AND deleted_at IS NULL")
                .bind(ctx.user_id)
                .fetch_all(&state.pool)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!("db error listing machines for reconcile push: {e}");
                    Vec::new()
                });
        for machine_id in machines {
            if let Err(err) = crate::bus::push_reconcile(&state, machine_id).await {
                tracing::debug!(%machine_id, %err, "push_reconcile after harnessMode change failed");
            }
        }
    }

    Ok(Json(SettingsPayload { version, data }))
}

/// `POST /api/v1/settings/rescrub`: apply the caller's effective scrub
/// list to their already-stored `stream_events`. `dry_run` reports counts and
/// writes nothing; the real pass masks matching rows and is idempotent (a second
/// run reports zero changes). Optional `session_ids` / `since` scope the sweep.
#[derive(Deserialize, TS)]
#[ts(export)]
pub struct RescrubRequest {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub session_ids: Option<Vec<uuid::Uuid>>,
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize, TS)]
#[ts(export)]
pub struct RescrubReport {
    pub dry_run: bool,
    pub rows_scanned: u64,
    pub rows_changed: u64,
    pub substitutions: u64,
    /// Substitution counts keyed by category (e.g. `github_token`).
    pub by_category: std::collections::BTreeMap<String, u64>,
}

/// id-keyset batch size for the re-scrub sweep.
const RESCRUB_BATCH: i64 = 500;

pub async fn rescrub_settings(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<RescrubRequest>,
) -> Result<Json<RescrubReport>, StatusCode> {
    let data: Value = sqlx::query_scalar("SELECT data FROM user_settings WHERE user_id = $1")
        .bind(ctx.user_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .unwrap_or(Value::Null);
    let scrub_cfg = secret_scrub_of(&data);
    let user: Vec<(String, String)> =
        scrub_cfg.patterns.into_iter().map(|p| (p.name, p.regex)).collect();
    // The defaults always apply on an explicit re-scrub, regardless of the live
    // `secretScrubEnabled` toggle.
    let patterns = cctui_crypto::redact::compile(true, &user, &cctui_crypto::vault_key());

    let mut report = RescrubReport {
        dry_run: req.dry_run,
        rows_scanned: 0,
        rows_changed: 0,
        substitutions: 0,
        by_category: std::collections::BTreeMap::new(),
    };
    let mut cursor: i64 = 0;
    loop {
        let rows: Vec<(i64, Value)> = sqlx::query_as(
            "SELECT se.id, se.payload FROM stream_events se \
             JOIN sessions s ON s.id = se.session_id \
             WHERE s.user_id = $1 AND se.id > $2 \
               AND ($3::uuid[] IS NULL OR se.session_id = ANY($3)) \
               AND ($4::timestamptz IS NULL OR se.created_at >= $4) \
             ORDER BY se.id LIMIT $5",
        )
        .bind(ctx.user_id)
        .bind(cursor)
        .bind(req.session_ids.as_deref())
        .bind(req.since)
        .bind(RESCRUB_BATCH)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("db error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        if rows.is_empty() {
            break;
        }
        for (id, mut payload) in rows {
            cursor = id;
            report.rows_scanned += 1;
            let hit_counts = cctui_crypto::redact::redact_json_stats(&mut payload, &patterns);
            let n: usize = hit_counts.values().sum();
            if n == 0 {
                continue;
            }
            report.rows_changed += 1;
            report.substitutions += n as u64;
            for (cat, c) in hit_counts {
                *report.by_category.entry(cat).or_insert(0) += c as u64;
            }
            if !req.dry_run {
                // A redacted payload can collide with an existing redacted row on
                // the (session, type, content_hash, turn) dedup index — ignore that,
                // the equivalent row already exists.
                if let Err(e) = sqlx::query("UPDATE stream_events SET payload = $1 WHERE id = $2")
                    .bind(&payload)
                    .bind(id)
                    .execute(&state.pool)
                    .await
                {
                    tracing::warn!(id, "rescrub update skipped: {e}");
                }
            }
        }
    }
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use super::{
        clamp_auto_resume, clamp_harness_mode, clamp_locale, clamp_macros, clamp_secret_scrub,
        clamp_session_emoji_prefix, clamp_whip_stop_phrases, harness_mode_of,
        harness_mode_to_adapter_token, secret_scrub_of, whip_stop_phrases_of,
    };
    use serde_json::json;

    #[test]
    fn clamp_auto_resume_coerces_to_a_boolean() {
        let mut truthy = json!({ "autoResumeOnConnectionLoss": "yes" });
        clamp_auto_resume(&mut truthy);
        assert_eq!(truthy["autoResumeOnConnectionLoss"], json!(false));
        let mut on = json!({ "autoResumeOnConnectionLoss": true });
        clamp_auto_resume(&mut on);
        assert_eq!(on["autoResumeOnConnectionLoss"], json!(true));
        let mut absent = json!({});
        clamp_auto_resume(&mut absent);
        assert_eq!(absent, json!({}));
    }

    #[test]
    fn clamp_session_emoji_prefix_coerces_to_a_boolean() {
        // A truthy string must not survive: the SQL read compares the stored
        // JSON to `true`, so only a real boolean can enable the prefix.
        let mut truthy = json!({ "sessionEmojiPrefix": "yes" });
        clamp_session_emoji_prefix(&mut truthy);
        assert_eq!(truthy["sessionEmojiPrefix"], json!(false));

        let mut on = json!({ "sessionEmojiPrefix": true });
        clamp_session_emoji_prefix(&mut on);
        assert_eq!(on["sessionEmojiPrefix"], json!(true));

        // Absence stays implicit rather than being materialized as `false`.
        let mut absent = json!({});
        clamp_session_emoji_prefix(&mut absent);
        assert_eq!(absent, json!({}));
    }

    #[test]
    fn clamp_secret_scrub_rejects_bad_regex_and_keeps_valid() {
        let mut good = json!({
            "secretScrubEnabled": true,
            "secretScrubPatterns": [{ "name": "acme", "regex": "ACME-[0-9]+", "enabled": true }],
        });
        assert!(clamp_secret_scrub(&mut good).is_ok());
        assert_eq!(good["secretScrubPatterns"][0]["regex"], "ACME-[0-9]+");

        let mut bad = json!({ "secretScrubPatterns": [{ "name": "x", "regex": "([a-z" }] });
        assert!(clamp_secret_scrub(&mut bad).is_err());
    }

    #[test]
    fn secret_scrub_of_returns_only_enabled_patterns() {
        let data = json!({
            "secretScrubEnabled": true,
            "secretScrubPatterns": [
                { "name": "on", "regex": "AAA[0-9]+", "enabled": true },
                { "name": "off", "regex": "BBB[0-9]+", "enabled": false },
            ],
        });
        let cfg = secret_scrub_of(&data);
        assert!(cfg.enabled);
        assert_eq!(cfg.patterns.len(), 1);
        assert_eq!(cfg.patterns[0].name, "on");
    }

    #[test]
    fn secret_scrub_is_on_when_the_user_never_chose() {
        assert!(secret_scrub_of(&json!({})).enabled);
        assert!(secret_scrub_of(&json!(null)).enabled);
        assert!(!secret_scrub_of(&json!({ "secretScrubEnabled": false })).enabled);
    }

    #[test]
    fn clamp_secret_scrub_drops_empty_patterns_key() {
        let mut data = json!({ "secretScrubPatterns": [{ "regex": "  " }] });
        assert!(clamp_secret_scrub(&mut data).is_ok());
        assert!(data.get("secretScrubPatterns").is_none());
    }

    #[test]
    fn harness_mode_of_clamps_unknown_and_missing_to_bg() {
        assert_eq!(harness_mode_of(&json!({})), "bg");
        assert_eq!(harness_mode_of(&json!({ "harnessMode": "wat" })), "bg");
        assert_eq!(harness_mode_of(&json!({ "harnessMode": 3 })), "bg");
        assert_eq!(harness_mode_of(&json!({ "harnessMode": "sdk" })), "sdk");
        assert_eq!(harness_mode_of(&json!({ "harnessMode": "oneshot" })), "oneshot");
    }

    #[test]
    fn clamp_rewrites_a_typo_but_leaves_absence_implicit() {
        let mut bad = json!({ "harnessMode": "wat", "theme": "dark" });
        clamp_harness_mode(&mut bad);
        assert_eq!(bad["harnessMode"], "bg");
        assert_eq!(bad["theme"], "dark");

        let mut none = json!({ "theme": "dark" });
        clamp_harness_mode(&mut none);
        assert!(none.get("harnessMode").is_none());
    }

    #[test]
    fn whip_phrases_absent_stays_none() {
        assert_eq!(whip_stop_phrases_of(&json!({})), None);
        let mut data = json!({ "theme": "dark" });
        clamp_whip_stop_phrases(&mut data);
        assert!(data.get("whipStopPhrases").is_none());
    }

    #[test]
    fn whip_phrases_default_block_is_dropped() {
        let mut data = json!({ "whipStopPhrases": { "mode": "extend", "phrases": [] } });
        clamp_whip_stop_phrases(&mut data);
        assert!(data.get("whipStopPhrases").is_none());
    }

    #[test]
    fn whip_phrases_trims_lowercases_dedupes_and_drops_empties() {
        let mut data = json!({
            "whipStopPhrases": {
                "mode": "extend",
                "phrases": ["  Pour Une Autre Session ", "", "pour une autre session", "  "]
            }
        });
        clamp_whip_stop_phrases(&mut data);
        assert_eq!(
            data["whipStopPhrases"],
            json!({ "mode": "extend", "phrases": ["pour une autre session"] })
        );
    }

    #[test]
    fn whip_phrases_replace_mode_and_guidance_survive() {
        let mut data = json!({
            "whipStopPhrases": {
                "mode": "replace",
                "phrases": ["prêt pour ta relecture"],
                "guidance": "  Continue en français.  "
            }
        });
        clamp_whip_stop_phrases(&mut data);
        assert_eq!(
            data["whipStopPhrases"],
            json!({
                "mode": "replace",
                "phrases": ["prêt pour ta relecture"],
                "guidance": "Continue en français."
            })
        );
    }

    #[test]
    fn whip_phrases_unknown_mode_clamps_to_extend() {
        let block = whip_stop_phrases_of(&json!({
            "whipStopPhrases": { "mode": "wat", "phrases": ["x"] }
        }))
        .unwrap();
        assert_eq!(block["mode"], "extend");
    }

    #[test]
    fn whip_phrases_caps_count() {
        let many: Vec<String> = (0..500).map(|i| format!("phrase {i}")).collect();
        let block = whip_stop_phrases_of(&json!({
            "whipStopPhrases": { "mode": "replace", "phrases": many }
        }))
        .unwrap();
        assert_eq!(block["phrases"].as_array().unwrap().len(), super::MAX_WHIP_PHRASES);
    }

    #[test]
    fn clamp_locale_keeps_known_drops_unknown_and_leaves_absence() {
        let mut en = json!({ "locale": "en", "theme": "dark" });
        clamp_locale(&mut en);
        assert_eq!(en["locale"], "en");
        assert_eq!(en["theme"], "dark");

        let mut fr = json!({ "locale": "fr" });
        clamp_locale(&mut fr);
        assert_eq!(fr["locale"], "fr");

        let mut bad = json!({ "locale": "de" });
        clamp_locale(&mut bad);
        assert!(bad.get("locale").is_none());

        let mut typed = json!({ "locale": 3 });
        clamp_locale(&mut typed);
        assert!(typed.get("locale").is_none());

        let mut absent = json!({ "theme": "dark" });
        clamp_locale(&mut absent);
        assert!(absent.get("locale").is_none());
    }

    #[test]
    fn adapter_token_maps_bg_to_claude_daemon_and_passes_others_through() {
        assert_eq!(harness_mode_to_adapter_token(Some("bg")), "claude-daemon");
        assert_eq!(harness_mode_to_adapter_token(None), "claude-daemon");
        assert_eq!(harness_mode_to_adapter_token(Some("typo")), "claude-daemon");
        assert_eq!(harness_mode_to_adapter_token(Some("sdk")), "sdk");
        assert_eq!(harness_mode_to_adapter_token(Some("oneshot")), "oneshot");
    }

    #[test]
    fn clamp_macros_keeps_well_formed_items_and_drops_an_empty_off_block() {
        let mut data = serde_json::json!({
            "macros": {
                "enabled": "yes",
                "items": [
                    { "title": "  Nettoyage ", "prompt": "range le dépôt", "model": "opus",
                      "effort": "high", "pool_id": "p1", "confirm": false, "adapter": "" },
                    { "title": "sans prompt", "prompt": "   " },
                    "garbage"
                ]
            }
        });
        clamp_macros(&mut data);
        let block = &data["macros"];
        assert_eq!(block["enabled"], serde_json::json!(false));
        let items = block["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["title"], "Nettoyage");
        assert_eq!(items[0]["adapter"], "claude-code");
        assert_eq!(items[0]["confirm"], serde_json::json!(false));
        assert_eq!(items[0]["pool_id"], "p1");
        assert!(items[0]["machine_id"].is_null());
        assert!(items[0]["id"].as_str().is_some_and(|id| !id.is_empty()));

        let mut off = serde_json::json!({ "macros": { "enabled": false, "items": [] } });
        clamp_macros(&mut off);
        assert!(off.get("macros").is_none());

        let mut on = serde_json::json!({ "macros": { "enabled": true } });
        clamp_macros(&mut on);
        assert_eq!(on["macros"]["enabled"], serde_json::json!(true));
        assert_eq!(on["macros"]["items"], serde_json::json!([]));
    }
}
