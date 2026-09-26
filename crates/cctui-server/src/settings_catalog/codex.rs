//! Per-account Codex (`~/.codex/config.toml`) settings catalog.
//!
//! The openai-family half of [`super`], sharing its [`Catalog`] /
//! [`SettingKey`] / [`Preset`] types so the endpoint, the validation and the
//! webui editor are family-agnostic. `codex-config.schema.json` is codex's own
//! `config.schema.json` vendored at the `CODEX_MIN_VERSION` release; keys it
//! covers are `source = "schema"`, the rest are hand-maintained against
//! <https://learn.chatgpt.com/docs/config-file/config-reference>.
//!
//! Curation rule: a key is exposable only if
//! [`cctui_proto::codex_config::CURATED`] can render it into a session, and the
//! two lists are held to exact agreement by a test here. Anything cctui sets
//! per session itself (gateway routing, permission posture, model/effort) stays
//! `managed`, so an account value can never clobber it.
//!
//! Dotted names (`history.persistence`) are stored literally as top-level JSON
//! keys and map 1:1 onto codex's own `-c` dotted path syntax.

use std::sync::LazyLock;

use super::{Catalog, build_from};

const RAW_CATALOG: &str = include_str!("codex-catalog.toml");
const RAW_SCHEMA: &str = include_str!("codex-config.schema.json");

/// The `service_tier` value pinning a session to the standard (non-Fast) tier.
/// Codex's own default is `priority` for every gpt-5.x model, so an unset tier is
/// the expensive one — cctui pins this explicitly rather than inheriting.
pub const SERVICE_TIER_DEFAULT: &str = "default";

/// The `service_tier` value for Fast mode (1.5x speed, increased usage — same
/// model, same quality). Codex maps it to the request value `priority`.
pub const SERVICE_TIER_FAST: &str = "fast";

/// The `settings_json` key carrying the account-level tier default.
pub const SERVICE_TIER_KEY: &str = "service_tier";

static CATALOG: LazyLock<Catalog> =
    LazyLock::new(|| build_from("Codex", RAW_CATALOG, Some(RAW_SCHEMA)));

/// The process-wide Codex settings catalog singleton.
#[must_use]
pub fn catalog() -> &'static Catalog {
    &CATALOG
}

/// Normalise a requested service tier: `default`/`fast` pass through, blank and
/// anything unrecognised become [`None`] so the next level decides.
#[must_use]
pub fn normalize_service_tier(raw: Option<&str>) -> Option<String> {
    let v = raw?.trim().to_ascii_lowercase();
    matches!(v.as_str(), SERVICE_TIER_DEFAULT | SERVICE_TIER_FAST).then_some(v)
}

/// The account-level tier default from a provider's `settings_json` blob.
#[must_use]
pub fn service_tier_from_settings(settings: Option<&serde_json::Value>) -> Option<String> {
    normalize_service_tier(settings?.get(SERVICE_TIER_KEY)?.as_str())
}

/// The tier a codex session launches on: an explicit per-session choice wins over
/// the account default, and with neither the session is pinned to the standard
/// tier rather than inheriting codex's `priority` default.
#[must_use]
pub fn resolve_service_tier(
    requested: Option<&str>,
    account_settings: Option<&serde_json::Value>,
) -> String {
    normalize_service_tier(requested)
        .or_else(|| service_tier_from_settings(account_settings))
        .unwrap_or_else(|| SERVICE_TIER_DEFAULT.to_owned())
}

/// Pin a concrete `service_tier` into the settings blob served to the daemon on
/// every worker (re)launch, so a codex session never falls back to codex's own
/// `priority` default. An account default already in the blob is preserved.
#[must_use]
pub fn overlay_service_tier(settings: Option<serde_json::Value>) -> Option<serde_json::Value> {
    let tier = resolve_service_tier(None, settings.as_ref());
    let mut blob = settings.unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let obj = blob.as_object_mut()?;
    obj.insert(SERVICE_TIER_KEY.to_owned(), serde_json::Value::String(tier));
    Some(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_catalog::{Policy, SettingKey};
    use serde_json::json;
    use std::collections::BTreeSet;

    #[test]
    fn codex_catalog_loads_and_exposes_service_tier() {
        let c = catalog();
        let k = c.key(SERVICE_TIER_KEY).expect("service_tier catalogued");
        assert!(k.account_exposable());
        assert_eq!(k.group.as_deref(), Some("Speed & cost"));
        assert!(k.r#enum.as_deref().is_some_and(|e| e.contains("fast")));
        assert!(c.preset(super::super::QUIET_DEFAULTS_ID).is_some());
    }

    /// The catalog says what an account MAY set; `codex_config::CURATED` says
    /// what actually gets rendered into a session. A key in one and not the
    /// other is either an unsettable toggle or an unvetted injection, so the two
    /// must agree exactly — `service_tier` excepted, which is stored per account
    /// but supplied per thread rather than rendered.
    #[test]
    fn exposable_codex_keys_match_the_renderer() {
        let exposable: BTreeSet<&str> = catalog()
            .exposable_keys()
            .map(|k: &SettingKey| k.name.as_str())
            .filter(|n| *n != SERVICE_TIER_KEY)
            .collect();
        let rendered: BTreeSet<&str> =
            cctui_proto::codex_config::CURATED.iter().map(|c| c.name).collect();
        assert_eq!(exposable, rendered);
    }

    /// A catalogued type that disagrees with the renderer's TOML type emits a
    /// literal codex will reject, so the declared types must line up too.
    #[test]
    fn catalogued_types_match_the_rendered_toml_types() {
        use cctui_proto::codex_config::TomlType;
        for c in cctui_proto::codex_config::CURATED {
            let k = catalog().key(c.name).unwrap_or_else(|| panic!("{} catalogued", c.name));
            let want = match c.ty {
                TomlType::Str => "string",
                TomlType::Bool => "boolean",
                TomlType::Int => "number",
            };
            assert_eq!(k.r#type.as_deref(), Some(want), "{} type mismatch", c.name);
        }
    }

    /// The preset is applied by writing it straight into `settings_json`, so
    /// every value in it must survive validation and actually render.
    #[test]
    fn quiet_defaults_preset_validates_and_renders() {
        let c = catalog();
        let p = c.preset(super::super::QUIET_DEFAULTS_ID).expect("preset present");
        let blob = serde_json::Value::Object(
            p.settings.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        );
        assert!(c.validate_settings(&blob).ok(), "preset must pass its own validation");
        assert_eq!(
            cctui_proto::codex_config::render_lines(&blob).len(),
            p.settings.len(),
            "every preset key must render"
        );
    }

    #[test]
    fn gateway_critical_codex_keys_are_managed() {
        let c = catalog();
        for name in [
            "model_provider",
            "model_providers",
            "openai_base_url",
            "chatgpt_base_url",
            "approval_policy",
            "sandbox_mode",
            "model",
            "model_reasoning_effort",
        ] {
            let k = c.key(name).unwrap_or_else(|| panic!("{name} catalogued"));
            assert_eq!(k.tag, Policy::Managed, "{name} must be managed");
            assert!(!k.account_exposable());
        }
    }

    #[test]
    fn codex_validation_rejects_managed_and_claude_keys() {
        let c = catalog();
        assert!(c.validate_settings(&json!({"service_tier": "fast"})).ok());
        let r = c.validate_settings(&json!({"model_provider": "cctui"}));
        assert_eq!(r.violations.len(), 1);
        let r = c.validate_settings(&json!({"disableBundledSkills": true}));
        assert!(r.violations[0].reason.contains("Codex"));
    }

    #[test]
    fn service_tier_resolution_defaults_to_standard_tier() {
        assert_eq!(resolve_service_tier(None, None), SERVICE_TIER_DEFAULT);
        assert_eq!(resolve_service_tier(Some("fast"), None), SERVICE_TIER_FAST);
        assert_eq!(resolve_service_tier(Some(" FAST "), None), SERVICE_TIER_FAST);
        assert_eq!(resolve_service_tier(Some("priority"), None), SERVICE_TIER_DEFAULT);
        assert_eq!(resolve_service_tier(Some(""), None), SERVICE_TIER_DEFAULT);
    }

    #[test]
    fn account_default_applies_only_without_a_session_choice() {
        let acct = json!({"service_tier": "fast"});
        assert_eq!(resolve_service_tier(None, Some(&acct)), SERVICE_TIER_FAST);
        assert_eq!(resolve_service_tier(Some("default"), Some(&acct)), SERVICE_TIER_DEFAULT);
        assert_eq!(
            resolve_service_tier(None, Some(&json!({"service_tier": "nope"}))),
            SERVICE_TIER_DEFAULT
        );
        assert_eq!(resolve_service_tier(None, Some(&json!({}))), SERVICE_TIER_DEFAULT);
    }

    #[test]
    fn overlay_pins_a_tier_and_preserves_the_account_default() {
        assert_eq!(overlay_service_tier(None), Some(json!({"service_tier": "default"})));
        assert_eq!(
            overlay_service_tier(Some(json!({"history.persistence": "none"}))),
            Some(json!({"history.persistence": "none", "service_tier": "default"}))
        );
        assert_eq!(
            overlay_service_tier(Some(json!({"service_tier": "fast"}))),
            Some(json!({"service_tier": "fast"}))
        );
        assert_eq!(
            overlay_service_tier(Some(json!({"service_tier": "priority"}))),
            Some(json!({"service_tier": "default"}))
        );
    }
}
