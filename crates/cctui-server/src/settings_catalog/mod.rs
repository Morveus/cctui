//! Per-account harness settings catalogs.
//!
//! Two catalogs share these types, one per provider family: the Claude Code
//! catalog below (anthropic) and the Codex one in [`codex`] (openai). Pick one
//! with [`for_family`].
//!
//! This module is the single source of truth for which Claude Code `settings.json`
//! keys and environment variables cctui may expose as per-account defaults, and how
//! dangerous each one is. It is consumed by:
//!
//! - the server's allowlist validation — reject a pasted settings/env blob
//!   that touches managed-only or unknown keys before persisting it, and
//! - the webui account settings editor — render the toggle list, grouped by
//!   policy, with types/enums/defaults.
//!
//! ## Two sources, one catalog
//!
//! - `claude-code-settings.schema.json` — the vendored `SchemaStore` JSON Schema
//!   (draft-07, `https://json.schemastore.org/claude-code-settings.json`). It is the
//!   authority for the **types / enums / defaults** of the 84 keys it covers, so those
//!   stay in sync when we re-vendor it. A CI job (`.github/workflows/schema-drift.yml`)
//!   re-fetches it and diffs to flag drift.
//! - `catalog.toml` — the hand-maintained delta the schema lacks: per-key **policy tags**
//!   (`safe`/`care`/`managed`/`system`), the docs-only keys the schema still lags on,
//!   a curated **env-var allowlist** (we do NOT expose all 284 documented vars), and the
//!   named **"Quiet defaults"** preset.
//!
//! Both files are embedded at compile time; there is no runtime fetch or file I/O.
//!
//! The public API is intentionally small and read-only: [`catalog`] returns the parsed
//! singleton, and [`Catalog`] exposes lookups plus [`Catalog::validate_settings`] /
//! [`Catalog::validate_free_env`] for the server-side allowlist check.

pub mod codex;

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

const RAW_SCHEMA: &str = include_str!("claude-code-settings.schema.json");
const RAW_CATALOG: &str = include_str!("catalog.toml");

/// Per-key exposure policy. Ordered least → most restrictive for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Good per-account toggle candidate; low blast radius.
    Safe,
    /// Exposable but has caveats (cost / security / footgun).
    Care,
    /// Org/admin managed-settings only; must NOT be set per-account.
    Managed,
    /// CLI-written state or session-only; not a user-facing toggle.
    System,
}

impl Policy {
    /// Whether a key/var with this policy may be set from a per-account settings blob.
    /// `safe` and `care` are exposable; `managed` and `system` are not.
    #[must_use]
    pub const fn account_exposable(self) -> bool {
        matches!(self, Self::Safe | Self::Care)
    }

    /// Lowercase tag string (`"safe"`, `"care"`, `"managed"`, `"system"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Care => "care",
            Self::Managed => "managed",
            Self::System => "system",
        }
    }
}

/// Where a key's definition comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Present in the vendored JSON schema (types/enums/defaults come from it).
    Schema,
    /// Docs-only key the schema still lags on (metadata hand-maintained here).
    Docs,
}

/// A single `settings.json` top-level key, enriched from the schema where possible.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct SettingKey {
    /// Top-level key name (e.g. `"model"`, `"disableBundledSkills"`).
    pub name: String,
    /// Exposure policy tag.
    pub tag: Policy,
    /// Schema vs docs origin.
    pub source: Source,
    /// JSON type(s), e.g. `"boolean"`, `"string"`, `"array"` (best-effort).
    pub r#type: Option<String>,
    /// Allowed enum values, comma-joined, when the key is an enum.
    pub r#enum: Option<String>,
    /// Documented default, as a display string.
    pub default: Option<String>,
    /// Human-readable notes (from the schema description or the hand catalog).
    pub notes: Option<String>,
    /// Editor grouping for the account-settings toggle list. Set in
    /// catalog.toml on the curated boolean keys only; a key with a group gets a
    /// tri-state toggle in the webui, everything else is raw-JSON-only.
    pub group: Option<String>,
    /// Human-readable toggle label (paired with `group`).
    pub label: Option<String>,
}

impl SettingKey {
    /// Convenience: may this key be set from a per-account settings blob?
    #[must_use]
    pub const fn account_exposable(&self) -> bool {
        self.tag.account_exposable()
    }
}

/// Control shape for a curated env var in the account-settings editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "lowercase")]
pub enum EnvKind {
    /// Present as `"1"` / absent (a boolean switch).
    #[default]
    Flag,
    /// A numeric literal stored as a string.
    Number,
    /// A free-form string literal.
    String,
    /// One of a fixed set of values (`values`).
    Enum,
}

/// A curated environment variable exposed as an account default.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct EnvVar {
    /// Variable name (e.g. `"ANTHROPIC_MODEL"`).
    pub name: String,
    /// Grouping for the editor UI (`model`/`context`/`thinking`/`tokens`/`skills`/
    /// `timeouts`/`telemetry`).
    pub group: String,
    /// Exposure policy tag.
    pub tag: Policy,
    /// Control shape rendered by the editor.
    pub kind: EnvKind,
    /// Allowed values for an `enum`-kind var (e.g. effort levels).
    pub values: Option<Vec<String>>,
    /// settings.json key this env var aliases, when one exists — the
    /// editor merges the two into ONE row that reads/writes the settings key.
    pub settings_equiv: Option<String>,
    /// Another env var this is an exact alias of (`DO_NOT_TRACK` == `DISABLE_TELEMETRY`).
    /// The editor folds aliases into the primary's row so only one renders.
    pub env_alias_of: Option<String>,
    /// Human-readable row label (like curated settings keys have).
    pub label: Option<String>,
    /// Human-readable notes.
    pub notes: Option<String>,
}

/// A named bundle of settings + env applied together (e.g. "Quiet defaults").
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Preset {
    /// Stable id (e.g. `"quiet-defaults"`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// What it does / caveats.
    pub description: String,
    /// `settings.json` fragment this preset writes.
    pub settings: BTreeMap<String, Value>,
    /// Env fragment this preset writes.
    pub env: BTreeMap<String, String>,
}

/// The `"quiet-defaults"` preset id, referenced by the webui and server.
pub const QUIET_DEFAULTS_ID: &str = "quiet-defaults";

/// Session-critical / gateway-managed env vars that must NEVER be set from a
/// per-account env blob. Setting any of these would hijack gateway
/// routing or the session's identity. The full denylist also includes every
/// catalog env entry tagged `managed` (see [`Catalog::env_denylisted`]).
pub const ENV_DENYLIST: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_SESSION_KIND",
    "CLAUDE_BG_SOURCE",
    "CLAUDE_BG_BACKEND",
    "CLAUDE_BG_CLAIM_AUTH",
];

/// Whether a name is a POSIX-shell-safe env var name (`^[A-Za-z_][A-Za-z0-9_]*$`).
#[must_use]
pub fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A single allowlist violation from [`Catalog::validate_settings`] /
/// [`Catalog::validate_free_env`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    /// Offending key / env-var name.
    pub key: String,
    /// Why it was rejected.
    pub reason: String,
}

/// Outcome of validating a settings or env blob against the catalog allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationReport {
    /// Keys that are not exposable per-account (unknown, managed, or system).
    pub violations: Vec<Violation>,
}

impl ValidationReport {
    /// Whether the blob is safe to persist (no violations).
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// The parsed, enriched settings catalog. Access the singleton via [`catalog`].
#[derive(Debug)]
pub struct Catalog {
    label: &'static str,
    keys: Vec<SettingKey>,
    keys_by_name: BTreeMap<String, usize>,
    env: Vec<EnvVar>,
    env_by_name: BTreeMap<String, usize>,
    presets: Vec<Preset>,
}

impl Catalog {
    /// Look up a single key by exact name.
    #[must_use]
    pub fn key(&self, name: &str) -> Option<&SettingKey> {
        self.keys_by_name.get(name).map(|&i| &self.keys[i])
    }

    /// The curated env-var allowlist (NOT all 284 documented vars).
    #[must_use]
    pub fn env_allowlist(&self) -> &[EnvVar] {
        &self.env
    }

    /// Look up a single curated env var by exact name.
    #[must_use]
    pub fn env(&self, name: &str) -> Option<&EnvVar> {
        self.env_by_name.get(name).map(|&i| &self.env[i])
    }

    /// Look up a preset by id.
    #[must_use]
    pub fn preset(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// The "Quiet defaults" preset. Present in the catalog by construction; panics at
    /// load time (in [`build`]) if it is ever removed, so this cannot return `None`.
    #[must_use]
    pub fn quiet_defaults(&self) -> &Preset {
        self.preset(QUIET_DEFAULTS_ID).expect("quiet-defaults preset present (checked at load)")
    }

    /// The per-account-exposable (`safe`/`care`) keys, in catalog order. This is
    /// what the catalog endpoint serves — `managed`/`system` keys never leave the
    /// server as settable options.
    pub fn exposable_keys(&self) -> impl Iterator<Item = &SettingKey> {
        self.keys.iter().filter(|k| k.account_exposable())
    }

    /// Validate a `settings.json` object against the per-account allowlist policy.
    ///
    /// Each top-level key must be known to the catalog AND tagged `safe`/`care`.
    /// Unknown keys, `managed`, and `system` keys are violations. `value` must be a
    /// JSON object; a non-object is a single violation.
    ///
    /// Note: this checks TOP-LEVEL keys only. Nested footguns (e.g.
    /// `permissions.defaultMode: bypassPermissions`) are out of scope here and are the
    /// injection layer's concern.
    #[must_use]
    pub fn validate_settings(&self, value: &Value) -> ValidationReport {
        let mut violations = Vec::new();
        let Some(obj) = value.as_object() else {
            violations.push(Violation {
                key: "$".to_string(),
                reason: "settings must be a JSON object".to_string(),
            });
            return ValidationReport { violations };
        };
        for (name, value) in obj {
            match self.key(name) {
                None => violations.push(Violation {
                    key: name.clone(),
                    reason: format!("unknown settings key (not in the {} catalog)", self.label),
                }),
                Some(k) if !k.account_exposable() => violations.push(Violation {
                    key: name.clone(),
                    reason: format!(
                        "key is tagged `{}` and cannot be set per-account",
                        k.tag.as_str()
                    ),
                }),
                Some(_) => {}
            }
            // The nested `env` block applies env to the live process:
            // it must be an object of string values, each a well-formed,
            // non-denylisted name — the same free-form rules as the account env.
            if name == "env" {
                self.validate_settings_env(value, &mut violations);
            }
        }
        ValidationReport { violations }
    }

    /// Validate the nested `settings_json.env` block: a JSON object of
    /// string values whose names are well-formed and not denylisted. Appends any
    /// violations (keyed `env.NAME`) to `out`.
    fn validate_settings_env(&self, value: &Value, out: &mut Vec<Violation>) {
        let Some(env) = value.as_object() else {
            out.push(Violation {
                key: "env".to_string(),
                reason: "settings `env` must be a JSON object of string values".to_string(),
            });
            return;
        };
        for (name, v) in env {
            if !v.is_string() {
                out.push(Violation {
                    key: format!("env.{name}"),
                    reason: "env values must be strings".to_string(),
                });
            }
            if !valid_env_name(name) {
                out.push(Violation {
                    key: format!("env.{name}"),
                    reason: "invalid env var name (must match [A-Za-z_][A-Za-z0-9_]*)".to_string(),
                });
            } else if self.env_denylisted(name) {
                out.push(Violation {
                    key: format!("env.{name}"),
                    reason: "env var is session-critical / gateway-managed and cannot be set \
                             per-account"
                        .to_string(),
                });
            }
        }
    }

    /// Whether an env var name is denylisted for per-account use: the
    /// fixed [`ENV_DENYLIST`] plus every catalog env entry tagged `managed`.
    #[must_use]
    pub fn env_denylisted(&self, name: &str) -> bool {
        ENV_DENYLIST.contains(&name)
            || self.env(name).is_some_and(|v| matches!(v.tag, Policy::Managed | Policy::System))
    }

    /// Validate a FREE-FORM per-account env map: each name must be a
    /// well-formed env var name and NOT denylisted. Values are arbitrary (they may
    /// be secrets). This replaces the old curated-allowlist gate for the
    /// account-level encrypted env blob.
    #[must_use]
    pub fn validate_free_env(&self, env: &BTreeMap<String, String>) -> ValidationReport {
        let mut violations = Vec::new();
        for name in env.keys() {
            if !valid_env_name(name) {
                violations.push(Violation {
                    key: name.clone(),
                    reason: "invalid env var name (must match [A-Za-z_][A-Za-z0-9_]*)".to_string(),
                });
            } else if self.env_denylisted(name) {
                violations.push(Violation {
                    key: name.clone(),
                    reason: "env var is session-critical / gateway-managed and cannot be set \
                             per-account"
                        .to_string(),
                });
            }
        }
        ValidationReport { violations }
    }
}

// ---- loading / parsing ----------------------------------------------------

/// Raw TOML shape of `catalog.toml`.
#[derive(Debug, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    keys: Vec<RawKey>,
    #[serde(default)]
    env: Vec<RawEnv>,
    #[serde(default)]
    presets: Vec<RawPreset>,
}

#[derive(Debug, Deserialize)]
struct RawKey {
    name: String,
    tag: Policy,
    source: Source,
    r#type: Option<String>,
    r#enum: Option<String>,
    default: Option<String>,
    notes: Option<String>,
    group: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEnv {
    name: String,
    group: String,
    tag: Policy,
    #[serde(default)]
    kind: EnvKind,
    values: Option<Vec<String>>,
    settings_equiv: Option<String>,
    env_alias_of: Option<String>,
    label: Option<String>,
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPreset {
    id: String,
    name: String,
    description: String,
    #[serde(default)]
    settings: BTreeMap<String, Value>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Best-effort extraction of `type` / `enum` / `default` / `description` for one key
/// from the vendored schema's `properties` map.
fn enrich_from_schema(
    props: Option<&Value>,
    name: &str,
) -> (Option<String>, Option<String>, Option<String>, Option<String>) {
    let Some(p) = props.and_then(|p| p.get(name)) else {
        return (None, None, None, None);
    };
    let type_ = match p.get("type") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => {
            Some(a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" | "))
        }
        _ => None,
    };
    let enum_ = p.get("enum").and_then(Value::as_array).map(|a| {
        a.iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    });
    let default = p.get("default").map(|d| match d {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let desc = p.get("description").and_then(Value::as_str).map(str::to_string);
    (type_, enum_, default, desc)
}

/// Parse both embedded artifacts into a [`Catalog`]. Panics on malformed embedded data
/// (a build-time invariant — the files are checked into the repo and covered by tests).
fn build() -> Catalog {
    build_from("Claude Code", RAW_CATALOG, Some(RAW_SCHEMA))
}

/// Parse one catalog TOML, enriching `source = "schema"` keys from `schema` when a
/// vendored JSON Schema backs this family.
fn build_from(label: &'static str, raw_toml: &str, raw_schema: Option<&str>) -> Catalog {
    let raw: RawCatalog = toml::from_str(raw_toml).expect("catalog.toml parses");
    let schema: Value = raw_schema
        .map_or(Value::Null, |s| serde_json::from_str(s).expect("vendored schema parses"));
    let props = schema.get("properties");

    let mut keys = Vec::with_capacity(raw.keys.len());
    let mut keys_by_name = BTreeMap::new();
    for (i, k) in raw.keys.into_iter().enumerate() {
        let (s_type, s_enum, s_default, s_desc) = if matches!(k.source, Source::Schema) {
            enrich_from_schema(props, &k.name)
        } else {
            (None, None, None, None)
        };
        keys_by_name.insert(k.name.clone(), i);
        keys.push(SettingKey {
            name: k.name,
            tag: k.tag,
            source: k.source,
            // Hand catalog wins where present (docs delta); otherwise use the schema.
            r#type: k.r#type.or(s_type),
            r#enum: k.r#enum.or(s_enum),
            default: k.default.or(s_default),
            notes: k.notes.or(s_desc),
            group: k.group,
            label: k.label,
        });
    }

    let mut env = Vec::with_capacity(raw.env.len());
    let mut env_by_name = BTreeMap::new();
    for (i, e) in raw.env.into_iter().enumerate() {
        env_by_name.insert(e.name.clone(), i);
        env.push(EnvVar {
            name: e.name,
            group: e.group,
            tag: e.tag,
            kind: e.kind,
            values: e.values,
            settings_equiv: e.settings_equiv,
            env_alias_of: e.env_alias_of,
            label: e.label,
            notes: e.notes,
        });
    }

    let presets: Vec<Preset> = raw
        .presets
        .into_iter()
        .map(|p| Preset {
            id: p.id,
            name: p.name,
            description: p.description,
            settings: p.settings,
            env: p.env,
        })
        .collect();

    assert!(
        presets.iter().any(|p| p.id == QUIET_DEFAULTS_ID),
        "catalog.toml must define the `{QUIET_DEFAULTS_ID}` preset"
    );

    Catalog { label, keys, keys_by_name, env, env_by_name, presets }
}

static CATALOG: LazyLock<Catalog> = LazyLock::new(build);

/// The process-wide settings catalog singleton.
#[must_use]
pub fn catalog() -> &'static Catalog {
    &CATALOG
}

/// The catalog governing a provider family's `settings_json`. `fireworks` has no
/// harness settings of its own and shares the Claude one.
#[must_use]
pub fn for_family(family: crate::routes::gateway::Family) -> &'static Catalog {
    match family {
        crate::routes::gateway::Family::Openai => codex::catalog(),
        _ => catalog(),
    }
}

/// The catalog governing a stored `provider` value's `settings_json`.
#[must_use]
pub fn for_provider(provider: &str) -> &'static Catalog {
    for_family(crate::routes::gateway::Family::from_provider(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_loads_and_indexes() {
        let c = catalog();
        assert!(c.keys.len() >= 100, "expected the full key catalog");
        assert!(!c.env_allowlist().is_empty());
        // Index round-trips.
        for k in &c.keys {
            assert!(c.key(&k.name).is_some(), "{} not indexed", k.name);
        }
        for e in c.env_allowlist() {
            assert!(c.env(&e.name).is_some());
        }
    }

    /// Drift guard: every property in the vendored schema must have a catalog entry
    /// tagged `source = schema`. If `SchemaStore` adds a key, re-vendor + tag it.
    #[test]
    fn every_schema_property_is_catalogued() {
        let c = catalog();
        let schema: Value = serde_json::from_str(RAW_SCHEMA).unwrap();
        let props =
            schema.get("properties").and_then(Value::as_object).expect("schema has properties");
        let missing: Vec<&String> = props
            .keys()
            .filter(|name| !matches!(c.key(name), Some(k) if matches!(k.source, Source::Schema)))
            .collect();
        assert!(
            missing.is_empty(),
            "schema properties missing from catalog.toml (source=schema): {missing:?}"
        );
    }

    #[test]
    fn schema_keys_enriched_from_schema() {
        // `model` is a schema key; it should have carried a type from the schema.
        let k = catalog().key("model").expect("model key present");
        assert_eq!(k.source, Source::Schema);
        assert!(k.r#type.is_some(), "model type should come from the schema");
        // An enum key carries its allowed values.
        let e = catalog().key("effortLevel").expect("effortLevel present");
        assert!(e.r#enum.as_deref().is_some_and(|s| s.contains("high")));
    }

    #[test]
    fn docs_only_keys_present() {
        let name = "ultracode";
        let k = catalog().key(name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(k.source, Source::Docs, "{name} should be a docs-delta key");
    }

    /// Toggle metadata: every key with a `group` must be an exposable
    /// boolean — the webui renders a tri-state toggle for exactly these.
    #[test]
    fn grouped_keys_are_exposable_booleans() {
        let c = catalog();
        let grouped: Vec<&SettingKey> = c.keys.iter().filter(|k| k.group.is_some()).collect();
        assert!(grouped.len() >= 25, "expected the curated toggle set, got {}", grouped.len());
        for k in grouped {
            assert!(k.account_exposable(), "{} is grouped but not exposable", k.name);
            assert!(k.label.is_some(), "{} is grouped but has no label", k.name);
            let ty = k.r#type.as_deref().unwrap_or("");
            assert!(
                ty == "boolean" || ty == "bool",
                "{} is grouped but not boolean (type {ty:?})",
                k.name
            );
        }
    }

    #[test]
    fn validate_settings_allows_safe_rejects_managed_and_unknown() {
        let c = catalog();
        let ok = serde_json::json!({ "model": "x", "disableBundledSkills": true });
        assert!(c.validate_settings(&ok).ok(), "safe keys should pass");

        let bad = serde_json::json!({
            "forceLoginMethod": "console", // managed
            "totallyNotAKey": 1            // unknown
        });
        let report = c.validate_settings(&bad);
        assert!(!report.ok());
        let keys: Vec<&str> = report.violations.iter().map(|v| v.key.as_str()).collect();
        assert!(keys.contains(&"forceLoginMethod"));
        assert!(keys.contains(&"totallyNotAKey"));

        // Non-object payload is a single violation.
        assert!(!c.validate_settings(&serde_json::json!([1, 2, 3])).ok());
    }

    #[test]
    fn env_entries_carry_kind_and_aliases() {
        let c = catalog();
        let effort = c.env("CLAUDE_CODE_EFFORT_LEVEL").expect("effort env present");
        assert_eq!(effort.kind, EnvKind::Enum);
        assert_eq!(effort.settings_equiv.as_deref(), Some("effortLevel"));
        assert!(effort.values.as_ref().is_some_and(|v| v.iter().any(|s| s == "max")));
        assert_eq!(effort.label.as_deref(), Some("Reasoning effort"));

        let model = c.env("ANTHROPIC_MODEL").expect("model env present");
        assert_eq!(model.kind, EnvKind::String);
        assert_eq!(model.settings_equiv.as_deref(), Some("model"));

        let bundled = c.env("CLAUDE_CODE_DISABLE_BUNDLED_SKILLS").expect("bundled env present");
        assert_eq!(bundled.kind, EnvKind::Flag);
        assert_eq!(bundled.settings_equiv.as_deref(), Some("disableBundledSkills"));

        // DO_NOT_TRACK is folded into DISABLE_TELEMETRY so the editor renders one row.
        let dnt = c.env("DO_NOT_TRACK").expect("DO_NOT_TRACK present");
        assert_eq!(dnt.env_alias_of.as_deref(), Some("DISABLE_TELEMETRY"));

        // Only channel dispatched workers honor for the bg edit guard — must stay curated and off the denylist.
        let bg = c.env("CLAUDE_BG_ISOLATION").expect("CLAUDE_BG_ISOLATION present");
        assert_eq!(bg.kind, EnvKind::Enum);
        assert!(bg.values.as_ref().is_some_and(|v| v.contains(&"none".to_string())));
        assert!(bg.settings_equiv.is_none());
        assert!(!c.env_denylisted("CLAUDE_BG_ISOLATION"));

        // Every curated env var carries a label for the row.
        for e in c.env_allowlist() {
            assert!(e.label.is_some(), "{} has no label", e.name);
        }
    }

    #[test]
    fn free_env_accepts_freeform_rejects_denylist() {
        let c = catalog();
        let mut ok = BTreeMap::new();
        ok.insert("MY_TOKEN".to_string(), "secret".to_string());
        ok.insert("_x1".to_string(), "v".to_string());
        assert!(c.validate_free_env(&ok).ok(), "well-formed free-form names accepted");

        for denied in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_SESSION_KIND",
            "CLAUDE_BG_SOURCE",
            "CLAUDE_BG_BACKEND",
            "CLAUDE_BG_CLAIM_AUTH",
        ] {
            let mut bad = BTreeMap::new();
            bad.insert(denied.to_string(), "x".to_string());
            assert!(!c.validate_free_env(&bad).ok(), "{denied} must be denylisted");
        }

        // Malformed names are rejected.
        let mut malformed = BTreeMap::new();
        malformed.insert("1BAD".to_string(), "x".to_string());
        malformed.insert("has-dash".to_string(), "x".to_string());
        let report = c.validate_free_env(&malformed);
        assert_eq!(report.violations.len(), 2);
    }

    #[test]
    fn validate_settings_checks_nested_env_block() {
        let c = catalog();
        // A well-formed env block passes.
        let ok = serde_json::json!({ "env": { "DISABLE_TELEMETRY": "1", "MY_VAR": "x" } });
        assert!(c.validate_settings(&ok).ok());

        // Denylisted names inside env are rejected.
        let denied = serde_json::json!({ "env": { "ANTHROPIC_AUTH_TOKEN": "sk" } });
        let report = c.validate_settings(&denied);
        assert!(!report.ok());
        assert!(report.violations.iter().any(|v| v.key == "env.ANTHROPIC_AUTH_TOKEN"));

        // Non-string values and a non-object env are rejected.
        assert!(!c.validate_settings(&serde_json::json!({ "env": { "X": 1 } })).ok());
        assert!(!c.validate_settings(&serde_json::json!({ "env": [1, 2] })).ok());
    }

    #[test]
    fn quiet_defaults_preset_is_complete() {
        let p = catalog().quiet_defaults();
        assert_eq!(p.id, QUIET_DEFAULTS_ID);
        assert_eq!(p.settings.get("disableBundledSkills"), Some(&Value::Bool(true)));
        assert_eq!(p.settings.get("remoteControlAtStartup"), Some(&Value::Bool(false)));
        assert_eq!(
            p.env.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").map(String::as_str),
            Some("1")
        );
        // The preset's own settings must all be known catalog keys (though some are
        // MANAGED — the preset is applied by cctui, not validated as user input).
        let c = catalog();
        for name in p.settings.keys() {
            assert!(c.key(name).is_some(), "preset key {name} not in catalog");
        }
    }
}
