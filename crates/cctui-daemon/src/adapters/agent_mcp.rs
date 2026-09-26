//! Adapter-agnostic `CctuiAgent` MCP wiring.
//!
//! The same relay `claude_code` gets via `--mcp-config`, rendered for codex
//! (`-c mcp_servers.…` process overrides) and opencode (the `mcp` block of its
//! per-session config).
//!
//! The session id baked into the relay's argv is the LAUNCH key, not the id the
//! harness eventually mints: codex mints a thread id and opencode a `ses_…` only
//! after the process is already running. [`crate::agenttool::bind_session_alias`]
//! maps the launch key onto the real id once it is known, so a tool call made
//! against the launch key still resolves to the real parent session.

use std::path::PathBuf;

use cctui_proto::api::SpawnCapability;
use serde_json::{Value, json};

/// MCP server name registered in the harness config. Matches the key
/// `claude_code`'s `--mcp-config` uses so the tool is namespaced identically
/// across harnesses.
pub const SERVER_NAME: &str = "cctui";

/// A resolved `CctuiAgent` relay launch for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMcp {
    exe: String,
    session_key: String,
    sock: PathBuf,
}

impl AgentMcp {
    /// The relay for `session_key`, or `None` when the session has no spawn
    /// capability — a session the server never granted spawn rights must not
    /// even see the tool.
    #[must_use]
    pub fn for_capability(session_key: &str, capability: Option<&SpawnCapability>) -> Option<Self> {
        if capability.is_none_or(SpawnCapability::is_empty) {
            return None;
        }
        Self::for_session(session_key)
    }

    /// The relay for a session whose grant is already established. Takes no
    /// capability: every call it makes is authorized server-side against the
    /// durable grant, so declaring it cannot widen what the session may do.
    #[must_use]
    pub fn for_session(session_key: &str) -> Option<Self> {
        if session_key.trim().is_empty() {
            tracing::warn!("CctuiAgent: no session key for the launch; not offering the tool");
            return None;
        }
        let exe = std::env::current_exe()
            .map_err(|err| tracing::warn!(%err, "CctuiAgent: cannot resolve current_exe"))
            .ok()?;
        Some(Self::new(
            exe.to_string_lossy().into_owned(),
            session_key.to_owned(),
            crate::agenttool::socket_for_launch().to_path_buf(),
        ))
    }

    #[must_use]
    pub const fn new(exe: String, session_key: String, sock: PathBuf) -> Self {
        Self { exe, session_key, sock }
    }

    #[must_use]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    fn argv(&self) -> Vec<String> {
        vec![
            "mcp-agent".to_owned(),
            "--session".to_owned(),
            self.session_key.clone(),
            "--sock".to_owned(),
            self.sock.to_string_lossy().into_owned(),
        ]
    }

    /// `codex app-server -c key=value` overrides declaring the relay. The values
    /// are TOML literals (already quoted / bracketed), so they are passed
    /// through verbatim rather than re-quoted like the scalar config knobs.
    #[must_use]
    pub fn codex_config_overrides(&self) -> Vec<(String, String)> {
        let args = self.argv().iter().map(|a| toml_string(a)).collect::<Vec<_>>().join(", ");
        vec![
            (format!("mcp_servers.{SERVER_NAME}.command"), toml_string(&self.exe)),
            (format!("mcp_servers.{SERVER_NAME}.args"), format!("[{args}]")),
        ]
    }

    /// The `mcp` block merged into the session's `opencode.json`.
    #[must_use]
    pub fn opencode_config(&self) -> Value {
        let mut command = vec![self.exe.clone()];
        command.extend(self.argv());
        json!({
            SERVER_NAME: {
                "type": "local",
                "command": command,
                "enabled": true,
            }
        })
    }
}

/// Sessions whose launch registered the tool, keyed by their real id.
///
/// A resume re-launches the harness process, so the `-c` / config declaration
/// has to be rebuilt; the gateway-env pull that carries the capability is
/// deliberately skipped there when the stored env still holds a credential, so
/// the launch decision is remembered here instead of re-derived.
static BY_SESSION: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, AgentMcp>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Remember `mcp` as `session_id`'s relay so a later resume re-declares it.
pub fn remember(session_id: &str, mcp: &AgentMcp) {
    if let Ok(mut map) = BY_SESSION.lock() {
        map.insert(session_id.to_owned(), mcp.clone());
    }
}

/// The relay a previous launch of `session_id` registered, if any.
#[must_use]
pub fn recall(session_id: &str) -> Option<AgentMcp> {
    BY_SESSION.lock().ok().and_then(|map| map.get(session_id).cloned())
}

/// Escape a value as a TOML basic string. Codex parses `-c key=value` as TOML,
/// so a path with a quote or backslash must not be able to break out.
fn toml_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::{AgentMcp, SERVER_NAME, toml_string};
    use cctui_proto::api::SpawnCapability;

    fn cap(adapters: &[&str]) -> SpawnCapability {
        SpawnCapability {
            adapters: adapters.iter().map(|a| (*a).to_owned()).collect(),
            ..Default::default()
        }
    }

    fn fixture() -> AgentMcp {
        AgentMcp::new(
            "/usr/bin/cctui-daemon".to_owned(),
            "spawn-key-1".to_owned(),
            "/run/cctui-agent.sock".into(),
        )
    }

    #[test]
    fn no_capability_means_no_tool() {
        assert!(AgentMcp::for_capability("spawn-key-1", None).is_none());
        assert!(
            AgentMcp::for_capability("spawn-key-1", Some(&cap(&[]))).is_none(),
            "an empty capability grants nothing, so the tool must stay absent"
        );
    }

    #[test]
    fn a_capability_without_a_session_key_offers_no_tool() {
        assert!(AgentMcp::for_capability("  ", Some(&cap(&["codex"]))).is_none());
    }

    #[test]
    fn a_capability_offers_the_relay_keyed_to_the_session() {
        let mcp = AgentMcp::for_capability("spawn-key-1", Some(&cap(&["codex"])))
            .expect("a non-empty capability offers the tool");
        assert_eq!(mcp.session_key(), "spawn-key-1");
    }

    /// A session launched without an explicit capability is granted the machine
    /// default, so it gets the relay — `CctuiAgent` and the read-only `CctuiUsage`.
    #[test]
    fn the_default_grant_mounts_the_relay() {
        let mcp =
            AgentMcp::for_capability("spawn-key-1", Some(&SpawnCapability::machine_default()))
                .expect("the default grant offers the tool");
        assert_eq!(mcp.session_key(), "spawn-key-1");
    }

    #[test]
    fn codex_overrides_register_the_relay_as_an_mcp_server() {
        let overrides = fixture().codex_config_overrides();
        let by_key = |k: &str| {
            overrides.iter().find(|(key, _)| key == k).map_or_else(
                || panic!("missing override {k}; got {overrides:?}"),
                |(_, v)| v.clone(),
            )
        };
        assert_eq!(
            by_key(&format!("mcp_servers.{SERVER_NAME}.command")),
            "\"/usr/bin/cctui-daemon\""
        );
        let args = by_key(&format!("mcp_servers.{SERVER_NAME}.args"));
        assert_eq!(
            args,
            "[\"mcp-agent\", \"--session\", \"spawn-key-1\", \"--sock\", \"/run/cctui-agent.sock\"]"
        );
    }

    #[test]
    fn opencode_config_declares_a_local_stdio_server() {
        let cfg = fixture().opencode_config();
        let server = &cfg[SERVER_NAME];
        assert_eq!(server["type"], "local");
        assert_eq!(server["enabled"], true);
        let command: Vec<&str> =
            server["command"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            command,
            vec![
                "/usr/bin/cctui-daemon",
                "mcp-agent",
                "--session",
                "spawn-key-1",
                "--sock",
                "/run/cctui-agent.sock",
            ]
        );
    }

    #[test]
    fn toml_values_are_escaped_so_a_path_cannot_break_out() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
