use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use cctui_proto::models::{Session, SessionStatus, TokenUsage};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineCommand {
    pub id: Uuid, // internal command ID — not a session ID
    pub command_type: String,
    pub payload: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Live-session metadata. Delivery concerns (the per-session stream broadcast
/// channel) moved into [`crate::bus::Bus`]; the registry keeps only
/// session bookkeeping.
#[derive(Debug)]
pub struct SessionHandle {
    pub session: Session,
    #[allow(dead_code)]
    pub last_heartbeat: Instant,
    #[allow(dead_code)]
    pub token_usage: TokenUsage,
    pub policy_rules: Vec<crate::policy::PolicyRule>,
}

pub type SharedRegistry = Arc<RwLock<Registry>>;

pub struct Registry {
    sessions: HashMap<String, SessionHandle>,
    machine_commands: HashMap<String, Vec<MachineCommand>>,
}

#[allow(dead_code)]
impl Registry {
    pub fn new() -> Self {
        Self { sessions: HashMap::new(), machine_commands: HashMap::new() }
    }

    pub fn shared() -> SharedRegistry {
        Arc::new(RwLock::new(Self::new()))
    }

    pub fn register(&mut self, session: Session) {
        self.sessions.insert(
            session.id.clone(),
            SessionHandle {
                session,
                last_heartbeat: Instant::now(),
                token_usage: TokenUsage::default(),
                policy_rules: Vec::new(),
            },
        );
    }

    pub fn deregister(&mut self, id: &str) -> Option<Session> {
        self.sessions.remove(id).map(|h| h.session)
    }

    pub fn get(&self, id: &str) -> Option<&SessionHandle> {
        self.sessions.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut SessionHandle> {
        self.sessions.get_mut(id)
    }

    pub fn list(&self) -> Vec<&SessionHandle> {
        self.sessions.values().collect()
    }

    pub fn update_heartbeat(&mut self, id: &str, tokens_in: u64, tokens_out: u64, cost_usd: f64) {
        if let Some(handle) = self.sessions.get_mut(id) {
            handle.last_heartbeat = Instant::now();
            handle.session.last_heartbeat = Utc::now();
            handle.session.status = SessionStatus::Active;
            handle.token_usage.tokens_in = tokens_in;
            handle.token_usage.tokens_out = tokens_out;
            handle.token_usage.cost_usd = cost_usd;
        }
    }

    pub fn update_token_usage(&mut self, id: &str, tokens_in: u64, tokens_out: u64, cost_usd: f64) {
        if let Some(handle) = self.sessions.get_mut(id) {
            handle.token_usage.tokens_in += tokens_in;
            handle.token_usage.tokens_out += tokens_out;
            handle.token_usage.cost_usd += cost_usd;
            handle.last_heartbeat = Instant::now();
            handle.session.last_heartbeat = Utc::now();
        }
    }

    /// Record activity for `id` — refresh the heartbeat and flip the session
    /// to `Active` if it was `New` or `Inactive`. Returns the new status if
    /// it changed, so the caller can broadcast a status change. Returns
    /// `None` if the session is unknown or was already `Active`.
    pub fn touch(&mut self, id: &str) -> Option<SessionStatus> {
        let handle = self.sessions.get_mut(id)?;
        handle.last_heartbeat = Instant::now();
        handle.session.last_heartbeat = Utc::now();
        if handle.session.status == SessionStatus::Active {
            None
        } else {
            handle.session.status = SessionStatus::Active;
            Some(SessionStatus::Active)
        }
    }

    /// Demote stale sessions to `Inactive`:
    /// - `Active` when idle longer than `inactive_after_secs`.
    /// - `New` when no turn arrives within `NEW_TTL_SECS` of registration
    ///   (prevents the "new" pill from lingering on sessions that never
    ///   produced activity). `Inactive` stays `Inactive` until revived.
    ///
    /// Returns the list of session ids that were just demoted.
    pub fn mark_stale(&mut self, inactive_after_secs: u64) -> Vec<String> {
        const NEW_TTL_SECS: u64 = 60;
        let now = Instant::now();
        let mut demoted = Vec::new();
        for handle in self.sessions.values_mut() {
            let elapsed = now.duration_since(handle.last_heartbeat).as_secs();
            let should_demote = match handle.session.status {
                SessionStatus::Active => elapsed > inactive_after_secs,
                SessionStatus::New => elapsed > NEW_TTL_SECS,
                // Archived sessions are deregistered, so this is defensive;
                // either way there's nothing to demote. Drafts are
                // never registered in memory, so this arm is also defensive.
                SessionStatus::Inactive
                | SessionStatus::Archived
                | SessionStatus::Draft
                | SessionStatus::Queued => false,
            };
            if should_demote {
                handle.session.status = SessionStatus::Inactive;
                demoted.push(handle.session.id.clone());
            }
        }
        demoted
    }

    pub fn set_policy(&mut self, session_id: &str, rules: Vec<crate::policy::PolicyRule>) {
        if let Some(handle) = self.sessions.get_mut(session_id) {
            handle.policy_rules = rules;
        }
    }

    pub fn queue_machine_command(
        &mut self,
        machine_id: &str,
        cmd_type: &str,
        payload: serde_json::Value,
    ) -> Uuid {
        let cmd = MachineCommand {
            id: Uuid::new_v4(),
            command_type: cmd_type.to_string(),
            payload,
            created_at: chrono::Utc::now(),
        };
        let id = cmd.id;
        self.machine_commands.entry(machine_id.to_string()).or_default().push(cmd);
        id
    }

    pub fn take_machine_commands(&mut self, machine_id: &str) -> Vec<MachineCommand> {
        self.machine_commands.remove(machine_id).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session(id: &str, parent: Option<String>) -> Session {
        Session {
            id: id.to_string(),
            parent_id: parent,
            account_id: None,
            machine_id: "test".into(),
            working_dir: "/tmp".into(),
            status: SessionStatus::Active,
            registered_at: Utc::now(),
            last_heartbeat: Utc::now(),
            metadata: serde_json::json!({}),
            adapter_id: None,
        }
    }

    #[test]
    fn register_and_list() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));
        assert_eq!(reg.list().len(), 1);
        assert!(reg.get(id).is_some());
    }

    #[test]
    fn deregister_removes_session() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));
        reg.deregister(id);
        assert!(reg.get(id).is_none());
        assert_eq!(reg.list().len(), 0);
    }

    #[test]
    fn mark_stale_demotes_active_to_inactive() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));

        // Fresh session — stays Active.
        let demoted = reg.mark_stale(90);
        assert!(demoted.is_empty());
        assert_eq!(reg.get(id).unwrap().session.status, SessionStatus::Active);

        // Backdate the heartbeat past the inactive window.
        if let Some(handle) = reg.get_mut(id) {
            handle.last_heartbeat =
                std::time::Instant::now().checked_sub(std::time::Duration::from_secs(100)).unwrap();
        }
        let demoted = reg.mark_stale(90);
        assert_eq!(demoted, vec![id.to_string()]);
        assert_eq!(reg.get(id).unwrap().session.status, SessionStatus::Inactive);

        // Inactive stays inactive; mark_stale is idempotent.
        let demoted = reg.mark_stale(90);
        assert!(demoted.is_empty());
        assert_eq!(reg.get(id).unwrap().session.status, SessionStatus::Inactive);
    }

    #[test]
    fn touch_revives_inactive_session() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        let mut s = make_session(id, None);
        s.status = SessionStatus::Inactive;
        reg.register(s);

        let changed = reg.touch(id);
        assert_eq!(changed, Some(SessionStatus::Active));
        assert_eq!(reg.get(id).unwrap().session.status, SessionStatus::Active);

        // Touching an already-active session is a no-op for status.
        assert!(reg.touch(id).is_none());
    }

    #[test]
    fn touch_promotes_new_to_active() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        let mut s = make_session(id, None);
        s.status = SessionStatus::New;
        reg.register(s);

        let changed = reg.touch(id);
        assert_eq!(changed, Some(SessionStatus::Active));
    }

    #[test]
    fn update_heartbeat_refreshes_session() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));

        reg.update_heartbeat(id, 100, 50, 0.01);

        let handle = reg.get(id).unwrap();
        assert_eq!(handle.token_usage.tokens_in, 100);
        assert_eq!(handle.token_usage.tokens_out, 50);
        assert_eq!(handle.session.status, SessionStatus::Active);
    }

    #[test]
    fn update_token_usage_accumulates() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));

        reg.update_token_usage(id, 100, 50, 0.01);
        reg.update_token_usage(id, 200, 100, 0.02);

        let handle = reg.get(id).unwrap();
        assert_eq!(handle.token_usage.tokens_in, 300);
        assert_eq!(handle.token_usage.tokens_out, 150);
    }

    #[test]
    fn set_policy_stores_rules() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));

        let rules = vec![crate::policy::PolicyRule {
            tool: "Bash".into(),
            action: crate::policy::PolicyAction::Deny,
            pattern: Some("rm -rf".into()),
            reason: Some("dangerous".into()),
        }];
        reg.set_policy(id, rules);

        let handle = reg.get(id).unwrap();
        assert_eq!(handle.policy_rules.len(), 1);
        assert_eq!(handle.policy_rules[0].tool, "Bash");
    }

    #[test]
    fn re_register_same_session_id_replaces() {
        let mut reg = Registry::new();
        let id = "claude-session-abc";
        reg.register(make_session(id, None));

        // Re-register with the same ID (simulates server restart + re-registration)
        reg.register(make_session(id, None));
        assert_eq!(reg.list().len(), 1);
    }
}
