//! Codex `model/list` catalog protocol.
//!
//! The account/machine-scoped model catalog is resolved by codex from a
//! remote, auth-gated, `client_version`-tagged endpoint
//! (`https://chatgpt.com/backend-api/codex/models`). When codex cannot refresh
//! its token it serves a stale bundled fallback, so the list is only trustworthy
//! over an AUTHENTICATED app-server connection.
//!
//! cctui routes codex through a gateway and injects the credential per session
//! (see [`super::app_server`]). A standalone `codex app-server` spawned with only
//! `PATH` (the poll) has no gateway credential and 401s on gateway-only
//! machines — the exact bug fixes. So there is no standalone poll here
//! anymore: this module is the pure protocol layer (request builder + response
//! parsing), reused by the session driver to issue `model/list` on the session's
//! EXISTING authenticated app-server connection at session start. The parsed
//! [`CodexModelCatalog`] is shipped to the server as an
//! [`AdapterEvent::CodexModels`](cctui_proto::adapter::AdapterEvent::CodexModels).

use cctui_proto::codex_catalog::CodexModel;
use serde_json::{Value, json};

/// Cap on `model/list` pages followed via `nextCursor` — a guard against a
/// pathological server that never terminates pagination.
pub const MAX_PAGES: usize = 20;

/// Parse one `model/list` `data[]` element (codex 0.144.1 `Model`) into a
/// [`CodexModel`]. Returns `None` for entries missing a usable id.
#[must_use]
pub fn parse_model(v: &Value) -> Option<CodexModel> {
    let id = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_owned();
    let model = v.get("model").and_then(Value::as_str).unwrap_or(&id).to_owned();
    let supported_efforts = v
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("reasoningEffort").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let input_modalities = v
        .get("inputModalities")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    Some(CodexModel {
        display_name: v.get("displayName").and_then(Value::as_str).unwrap_or(&id).to_owned(),
        description: v.get("description").and_then(Value::as_str).unwrap_or_default().to_owned(),
        hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false)
            || v.get("visibility").and_then(Value::as_str) == Some("hide"),
        minimal_client_version: v
            .get("minimalClientVersion")
            .or_else(|| v.get("minimal_client_version"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        is_default: v.get("isDefault").and_then(Value::as_bool).unwrap_or(false),
        default_effort: v
            .get("defaultReasoningEffort")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        supported_efforts,
        input_modalities,
        upgrade: v.get("upgrade").and_then(Value::as_str).map(str::to_owned),
        id,
        model,
    })
}

/// Parse the `result` of a `model/list` response into models.
#[must_use]
pub fn parse_model_list(result: &Value) -> Vec<CodexModel> {
    result
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(parse_model).collect())
        .unwrap_or_default()
}

/// Build a `model/list` request. `id` is the JSON-RPC correlation id;
/// `cursor` continues a paginated fetch.
#[must_use]
pub fn model_list_req(id: i64, cursor: Option<&str>) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("includeHidden".to_owned(), json!(true));
    if let Some(cursor) = cursor {
        params.insert("cursor".to_owned(), json!(cursor));
    }
    json!({"jsonrpc": "2.0", "id": id, "method": "model/list", "params": Value::Object(params)})
}

/// The `nextCursor` of a `model/list` `result`, if pagination continues.
#[must_use]
pub fn next_cursor(result: &Value) -> Option<String> {
    result.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_owned)
}

/// What to do after parsing one `model/list` response page: fetch the next
/// page with the given cursor, or stop and emit the accumulated catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageStep {
    Next { cursor: String },
    Done,
}

/// Decide whether to follow `nextCursor` into another page. Stops when the
/// server reports no further cursor or the [`MAX_PAGES`] guard is reached
/// (`pages_fetched` counts pages already parsed, including this one).
#[must_use]
pub fn page_step(pages_fetched: usize, result: &Value) -> PageStep {
    match next_cursor(result) {
        Some(cursor) if pages_fetched < MAX_PAGES => PageStep::Next { cursor },
        _ => PageStep::Done,
    }
}

/// `false` disables the session-start catalog refresh (`model_catalog = false`).
/// Enabled by default; degrades silently to the webui's static fallback list.
#[must_use]
pub fn catalog_enabled(v: &Value) -> bool {
    v.get("model_catalog").and_then(Value::as_bool).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({"data": [
            {
                "id": "gpt-5.6-sol",
                "model": "gpt-5.6-sol",
                "displayName": "GPT-5.6 Sol",
                "description": "Flagship reasoning model.",
                "hidden": false,
                "isDefault": true,
                "defaultReasoningEffort": "medium",
                "supportedReasoningEfforts": [
                    {"reasoningEffort": "low", "description": "fast"},
                    {"reasoningEffort": "medium", "description": "balanced"},
                    {"reasoningEffort": "high", "description": "thorough"}
                ],
                "inputModalities": ["text", "image"],
                "upgrade": null
            },
            {
                "id": "gpt-5.4",
                "model": "gpt-5.4",
                "displayName": "GPT-5.4",
                "description": "",
                "hidden": true,
                "isDefault": false,
                "defaultReasoningEffort": "medium",
                "supportedReasoningEfforts": [
                    {"reasoningEffort": "low", "description": "fast"}
                ],
                "inputModalities": ["text"],
                "upgrade": "gpt-5.6-sol"
            },
            { "displayName": "no id, dropped" }
        ], "nextCursor": null})
    }

    #[test]
    fn parses_models_and_skips_idless() {
        let models = parse_model_list(&sample());
        assert_eq!(models.len(), 2);
        let sol = &models[0];
        assert_eq!(sol.id, "gpt-5.6-sol");
        assert_eq!(sol.display_name, "GPT-5.6 Sol");
        assert!(sol.is_default);
        assert!(!sol.hidden);
        assert_eq!(sol.default_effort, "medium");
        assert_eq!(sol.supported_efforts, ["low", "medium", "high"]);
        assert_eq!(sol.input_modalities, ["text", "image"]);
        assert_eq!(sol.upgrade, None);

        let old = &models[1];
        assert!(old.hidden);
        assert_eq!(old.upgrade.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(old.supported_efforts, ["low"]);
    }

    #[test]
    fn model_falls_back_to_id_when_slug_and_label_missing() {
        let m = parse_model(&json!({"id": "gpt-x"})).unwrap();
        assert_eq!(m.model, "gpt-x");
        assert_eq!(m.display_name, "gpt-x");
        assert!(m.supported_efforts.is_empty());
        assert!(m.input_modalities.is_empty());
    }

    #[test]
    fn missing_data_is_empty() {
        assert!(parse_model_list(&json!({})).is_empty());
        assert!(parse_model_list(&Value::Null).is_empty());
    }

    #[test]
    fn request_builder_shape() {
        let r = model_list_req(2, None);
        assert_eq!(r["method"], "model/list");
        assert_eq!(r["id"], 2);
        assert_eq!(r["params"]["includeHidden"], true);
        assert!(r["params"].get("cursor").is_none());
        assert_eq!(model_list_req(3, Some("CUR"))["params"]["cursor"], "CUR");
    }

    #[test]
    fn next_cursor_follows_until_null() {
        assert_eq!(next_cursor(&json!({"nextCursor": "abc"})).as_deref(), Some("abc"));
        assert_eq!(next_cursor(&json!({"nextCursor": ""})), None);
        assert_eq!(next_cursor(&json!({})), None);
    }

    #[test]
    fn page_step_stops_without_cursor_and_follows_with_one() {
        assert_eq!(page_step(1, &json!({"data": [], "nextCursor": null})), PageStep::Done);
        assert_eq!(
            page_step(1, &json!({"data": [], "nextCursor": "c2"})),
            PageStep::Next { cursor: "c2".to_owned() }
        );
    }

    #[test]
    fn page_step_stops_at_max_pages_guard() {
        assert_eq!(page_step(MAX_PAGES, &json!({"nextCursor": "more"})), PageStep::Done);
        assert_eq!(
            page_step(MAX_PAGES - 1, &json!({"nextCursor": "more"})),
            PageStep::Next { cursor: "more".to_owned() }
        );
    }

    #[test]
    fn session_start_refresh_accumulates_paginated_models() {
        let page1 = json!({"data": [{"id": "gpt-5.6-sol"}], "nextCursor": "c2"});
        let page2 = json!({"data": [{"id": "gpt-5.6-terra"}], "nextCursor": null});
        let mut models = Vec::new();
        let mut fetched = 0;

        models.extend(parse_model_list(&page1));
        fetched += 1;
        assert_eq!(page_step(fetched, &page1), PageStep::Next { cursor: "c2".to_owned() });

        models.extend(parse_model_list(&page2));
        fetched += 1;
        assert_eq!(page_step(fetched, &page2), PageStep::Done);

        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.6-sol", "gpt-5.6-terra"]);
    }

    #[test]
    fn catalog_enabled_defaults_on_and_toggles_off() {
        assert!(catalog_enabled(&json!({})));
        assert!(!catalog_enabled(&json!({"model_catalog": false})));
        assert!(catalog_enabled(&json!({"model_catalog": true})));
    }
}
