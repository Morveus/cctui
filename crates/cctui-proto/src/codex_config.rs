//! The single renderer for per-account Codex `config.toml` settings.
//!
//! Both injection paths — the daemon's `-c key=value` app-server flags and the
//! k8s worker's marker-delimited `~/.codex/config.toml` block — consume ONE
//! block rendered here and carried in [`CONFIG_TOML_ENV`], so the two cannot
//! drift.
//!
//! Two constraints shape the output:
//!
//! - **Dotted keys only, never a `[table]` header.** The worker splices this
//!   block into a config.toml that continues with `[model_providers.cctui]` and,
//!   last, the MCP tables; a header here would capture every bare key after it.
//! - **Typed literals.** Codex parses a `-c` right-hand side as TOML, so a
//!   quoted boolean (`hide_agent_reasoning="true"`) fails app-server startup outright.
//!   [`Curated`] carries each key's TOML type so booleans and integers are
//!   emitted bare.

use serde_json::Value;

/// Launch-env key carrying the rendered per-account config block. Read by the
/// daemon ([`overrides_from_block`]) and by `deploy/worker-entrypoint.sh`.
pub const CONFIG_TOML_ENV: &str = "CCTUI_CODEX_CONFIG_TOML";

/// The TOML type a curated key's value must render as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TomlType {
    /// A quoted TOML string.
    Str,
    /// A bare `true` / `false`.
    Bool,
    /// A bare integer.
    Int,
}

/// One curated `config.toml` key cctui will render from account settings.
#[derive(Debug, Clone, Copy)]
pub struct Curated {
    /// Dotted key path, exactly as it appears in `settings_json` and in TOML.
    pub name: &'static str,
    /// How its value is rendered.
    pub ty: TomlType,
}

const fn k(name: &'static str, ty: TomlType) -> Curated {
    Curated { name, ty }
}

/// The curated, renderable key set — the injection-side twin of the server's
/// codex settings catalog, which a server test holds to exact agreement.
///
/// `service_tier` is deliberately ABSENT. It is a per-session choice supplied
/// per thread via the daemon's `ThreadConfig`, and emitting it here would pin it
/// process-wide for every session the app-server serves.
pub const CURATED: &[Curated] = &[
    k("check_for_update_on_startup", TomlType::Bool),
    k("hide_agent_reasoning", TomlType::Bool),
    k("history.persistence", TomlType::Str),
    k("model_auto_compact_token_limit", TomlType::Int),
    k("model_context_window", TomlType::Int),
    k("model_reasoning_summary", TomlType::Str),
    k("model_verbosity", TomlType::Str),
    k("personality", TomlType::Str),
    k("plan_mode_reasoning_effort", TomlType::Str),
    k("show_raw_agent_reasoning", TomlType::Bool),
    k("web_search", TomlType::Str),
];

/// The curated entry for a key, or `None` when the key is not renderable.
#[must_use]
pub fn curated(name: &str) -> Option<&'static Curated> {
    CURATED.iter().find(|c| c.name == name)
}

/// Render one JSON value as a TOML literal of the declared type, or `None` when
/// the stored value cannot be that type. A settings blob is validated on
/// persist, but it is also editable as raw JSON and merged across accounts, so
/// a wrong-typed value must be dropped here rather than emitted and left to
/// brick the spawn.
fn literal(ty: TomlType, v: &Value) -> Option<String> {
    match ty {
        TomlType::Str => {
            let s = v.as_str()?;
            // A control character or a quote would break out of the literal and,
            // in the worker's case, out of the line. Refuse rather than escape:
            // no curated key has a legitimate value containing either.
            (!s.is_empty() && s.chars().all(|c| !c.is_control() && c != '"' && c != '\\'))
                .then(|| format!("\"{s}\""))
        }
        TomlType::Bool => v.as_bool().map(|b| b.to_string()),
        TomlType::Int => v.as_i64().map(|n| n.to_string()),
    }
}

/// `web_search` was a boolean before codex made it a mode enum; accounts may
/// still store the boolean.
fn legacy(name: &str, v: &Value) -> Value {
    match (name, v.as_bool()) {
        ("web_search", Some(true)) => Value::from("live"),
        ("web_search", Some(false)) => Value::from("disabled"),
        _ => v.clone(),
    }
}

/// Render the curated subset of a per-account settings blob as TOML lines.
///
/// Everything not in [`CURATED`] is dropped silently: the blob is deep-merged
/// across every account bound to the session, so it legitimately carries the
/// Claude family's camelCase keys too, and it carries `service_tier`, which is
/// per-session. Emitting any of those at codex would at best be ignored and at
/// worst fail app-server startup on an unknown key.
///
/// Output is sorted by key so a re-render of the same settings is byte-identical.
#[must_use]
pub fn render_lines(settings: &Value) -> Vec<String> {
    let Some(obj) = settings.as_object() else {
        return Vec::new();
    };
    let mut out: Vec<String> = CURATED
        .iter()
        .filter_map(|c| {
            let v = legacy(c.name, obj.get(c.name)?);
            Some(format!("{} = {}", c.name, literal(c.ty, &v)?))
        })
        .collect();
    out.sort();
    out
}

/// [`render_lines`] joined into a block, or `None` when nothing rendered — so a
/// caller can skip setting [`CONFIG_TOML_ENV`] entirely rather than set it empty.
#[must_use]
pub fn render_block(settings: &Value) -> Option<String> {
    let lines = render_lines(settings);
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Parse a rendered block back into `-c key=value` pairs for the daemon's
/// app-server command line, with the TOML literal passed through verbatim.
///
/// Round-trips [`render_block`]; a line it did not produce (no ` = `, or a key
/// outside [`CURATED`]) is dropped, so a hand-edited env var cannot smuggle an
/// arbitrary override onto the command line.
#[must_use]
pub fn overrides_from_block(block: &str) -> Vec<(String, String)> {
    block
        .lines()
        .filter_map(|line| {
            let (key, value) = line.trim().split_once(" = ")?;
            curated(key)?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_each_type_as_a_real_toml_literal() {
        let block = render_block(&json!({
            "hide_agent_reasoning": true,
            "model_context_window": 272_000,
            "model_verbosity": "low",
        }))
        .expect("something rendered");
        assert_eq!(
            block,
            "hide_agent_reasoning = true\nmodel_context_window = 272000\nmodel_verbosity = \"low\""
        );
    }

    #[test]
    fn legacy_boolean_web_search_renders_as_a_mode() {
        assert_eq!(render_lines(&json!({"web_search": true})), vec!["web_search = \"live\""]);
        assert_eq!(render_lines(&json!({"web_search": false})), vec!["web_search = \"disabled\""]);
        assert_eq!(render_lines(&json!({"web_search": "cached"})), vec!["web_search = \"cached\""]);
    }

    /// The whole point of the typed emitter: a boolean must NOT come out quoted.
    #[test]
    fn booleans_are_never_quoted() {
        let block = render_block(&json!({"hide_agent_reasoning": true})).unwrap();
        assert_eq!(block, "hide_agent_reasoning = true");
        assert!(!block.contains('"'));
    }

    #[test]
    fn drops_uncurated_and_per_session_keys() {
        let settings = json!({
            "service_tier": "fast",
            "model_provider": "evil",
            "disableBundledSkills": true,
            "totallyNotAKey": 1,
            "model_verbosity": "high",
        });
        assert_eq!(render_lines(&settings), vec!["model_verbosity = \"high\""]);
    }

    #[test]
    fn drops_wrong_typed_and_unsafe_values() {
        // Right key, wrong JSON type: dropped, not coerced.
        assert!(render_lines(&json!({"hide_agent_reasoning": "true"})).is_empty());
        assert!(render_lines(&json!({"model_context_window": "big"})).is_empty());
        assert!(render_lines(&json!({"model_verbosity": true})).is_empty());
        // Quote / newline / backslash injection into a string value.
        assert!(render_lines(&json!({"personality": "a\"\nmodel_provider = \"evil"})).is_empty());
        assert!(render_lines(&json!({"personality": "a\\b"})).is_empty());
        assert!(render_lines(&json!({"personality": ""})).is_empty());
        // Non-object blobs.
        assert!(render_lines(&json!("nope")).is_empty());
        assert!(render_block(&json!({})).is_none());
    }

    /// The block is spliced above `[model_providers.cctui]` and the MCP tables in
    /// the worker's config.toml; a `[table]` header here would capture the bare
    /// keys printed after it.
    #[test]
    fn block_is_dotted_only_and_never_emits_a_table_header() {
        let all: serde_json::Map<String, Value> = CURATED
            .iter()
            .map(|c| {
                let v = match c.ty {
                    TomlType::Str => json!("x"),
                    TomlType::Bool => json!(true),
                    TomlType::Int => json!(1),
                };
                (c.name.to_owned(), v)
            })
            .collect();
        let block = render_block(&Value::Object(all)).unwrap();
        assert_eq!(block.lines().count(), CURATED.len());
        for line in block.lines() {
            assert!(!line.starts_with('['), "table header in block: {line}");
            assert!(line.contains(" = "), "not a dotted assignment: {line}");
        }
        // And it parses as TOML on its own, which is what codex will do to it.
        let parsed: toml::Table = toml::from_str(&block).expect("block is valid TOML");
        assert_eq!(parsed["hide_agent_reasoning"].as_bool(), Some(true));
        assert_eq!(parsed["history"]["persistence"].as_str(), Some("x"));
    }

    /// The two injection paths must not diverge: the daemon's `-c` pairs are
    /// derived from the exact block the worker writes.
    #[test]
    fn overrides_round_trip_the_rendered_block() {
        let settings = json!({
            "hide_agent_reasoning": true,
            "model_context_window": 272_000,
            "history.persistence": "none",
        });
        let block = render_block(&settings).unwrap();
        assert_eq!(
            overrides_from_block(&block),
            vec![
                ("hide_agent_reasoning".to_owned(), "true".to_owned()),
                ("history.persistence".to_owned(), "\"none\"".to_owned()),
                ("model_context_window".to_owned(), "272000".to_owned()),
            ]
        );
    }

    #[test]
    fn overrides_reject_lines_the_renderer_never_produces() {
        let smuggled = "model_provider = \"evil\"\nservice_tier = \"fast\"\ngarbage\n[table]";
        assert!(overrides_from_block(smuggled).is_empty());
    }

    #[test]
    fn curated_is_sorted_and_free_of_duplicates_and_session_keys() {
        let names: Vec<&str> = CURATED.iter().map(|c| c.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted, "CURATED must be sorted and unique");
        assert!(curated("service_tier").is_none(), "service_tier is per-session");
    }
}
