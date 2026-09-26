//! Machine/account-scoped Codex model catalog from `model/list`,
//! keyed by `machine_id` on the server and consumed by the webui model picker.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One model advertised by `model/list` (codex 0.144.1 `Model`).
///
/// Only the fields the picker needs are retained; the rest of the codex shape
/// (service tiers, personality, NUX copy) is intentionally dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CodexModel {
    /// Catalog id — the value passed as `-c model="…"` on a spawn.
    pub id: String,
    /// Underlying model slug (codex `model` field). Usually equals `id`.
    pub model: String,
    /// Human label for the picker (codex `displayName`).
    pub display_name: String,
    /// Short description (codex `description`), used for a tooltip/title.
    #[serde(default)]
    pub description: String,
    /// Hidden from the default picker unless the user opts into hidden models.
    pub hidden: bool,
    /// The catalog default for a fresh session (codex `isDefault`).
    pub is_default: bool,
    /// Reasoning-effort levels this model supports (subset of
    /// `low`/`medium`/`high`/`xhigh`/…). The effort picker offers only these.
    pub supported_efforts: Vec<String>,
    /// The default effort for this model (codex `defaultReasoningEffort`).
    pub default_effort: String,
    /// Canonical input modalities the model accepts (`text`, `image`).
    #[serde(default)]
    pub input_modalities: Vec<String>,
    /// Id of the model this one should be upgraded to, when codex marks it as
    /// superseded (codex `upgrade`). Drives a disabled/label hint in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<String>,
    /// Lowest codex release the model is offered to; a machine below it cannot run it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal_client_version: Option<String>,
}

/// The full machine/account-scoped model catalog, as reported by one
/// machine's `codex app-server`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CodexModelCatalog {
    pub models: Vec<CodexModel>,
    /// The `client_version` the catalog was read under; set on responses only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}
