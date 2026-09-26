//! Follow-up sessions: a spawn whose first prompt embeds the parent's
//! transcript brief. The parent link is remembered under the spawn key (like
//! labels and auto-archive) and claimed when the worker registers, after the
//! daemon's own `relation: "root"` metadata has landed, so the claim wins.

use sqlx::PgPool;

use cctui_proto::api::SpawnRequest;

pub const RELATION: &str = "followup";

/// Unclaimed intents older than this are dropped: the spawn never registered.
const INTENT_TTL_SECS: i64 = 6 * 3600;

/// The parent a follow-up spawn continues, if the request is one.
#[must_use]
pub fn parent_of(req: &SpawnRequest) -> Option<&str> {
    if req.relation.as_deref() != Some(RELATION) {
        return None;
    }
    req.parent_session_id.as_deref().map(str::trim).filter(|p| !p.is_empty())
}

/// Remember which session the spawn under `spawn_key` follows up on.
/// Best-effort: a lost intent leaves the session unlinked.
pub async fn remember_intent(pool: &PgPool, spawn_key: &str, req: &SpawnRequest) {
    let Some(parent) = parent_of(req) else { return };
    if let Err(e) = sqlx::query(
        "INSERT INTO session_followup_intents (spawn_key, parent_session_id) VALUES ($1, $2) \
         ON CONFLICT (spawn_key) DO UPDATE SET parent_session_id = EXCLUDED.parent_session_id",
    )
    .bind(spawn_key)
    .bind(parent)
    .execute(pool)
    .await
    {
        tracing::warn!(%spawn_key, error = %e, "could not record the follow-up intent");
    }
}

/// Link the session that just registered to the parent a pending intent
/// names. Looked up by its own id and by the spawn key the adapter echoed, so
/// both pre-minted (claude-code) and self-minted (codex) ids resolve. The
/// parent must exist (FK), else the link is skipped.
pub async fn claim_intent(pool: &PgPool, session_id: &str, spawn_key: Option<&str>) {
    let key = spawn_key.unwrap_or(session_id);
    let parent = match sqlx::query_scalar::<_, String>(
        "DELETE FROM session_followup_intents WHERE spawn_key = $1 OR spawn_key = $2 \
         RETURNING parent_session_id",
    )
    .bind(session_id)
    .bind(key)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(parent)) => parent,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "follow-up intent lookup failed");
            return;
        }
    };
    if let Err(e) = sqlx::query(
        "UPDATE sessions SET \
             parent_id = COALESCE(parent_id, (SELECT id FROM sessions WHERE id = $2)), \
             metadata = COALESCE(metadata, '{}'::jsonb) || jsonb_build_object('relation', $3::text) \
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(&parent)
    .bind(RELATION)
    .execute(pool)
    .await
    {
        tracing::warn!(%session_id, %parent, error = %e, "could not link the follow-up session");
    }
}

/// Reaper tick: drop intents whose spawn never registered.
pub async fn sweep(pool: &PgPool) {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(INTENT_TTL_SECS);
    if let Err(e) = sqlx::query("DELETE FROM session_followup_intents WHERE created_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "follow-up intent prune failed");
    }
}

#[cfg(test)]
mod tests {
    use super::parent_of;
    use cctui_proto::api::SpawnRequest;

    fn req(relation: Option<&str>, parent: Option<&str>) -> SpawnRequest {
        let mut r: SpawnRequest =
            serde_json::from_value(serde_json::json!({"machine_id": "m", "working_dir": "/w"}))
                .unwrap();
        r.relation = relation.map(str::to_owned);
        r.parent_session_id = parent.map(str::to_owned);
        r
    }

    #[test]
    fn a_followup_names_its_parent() {
        assert_eq!(parent_of(&req(Some("followup"), Some(" p1 "))), Some("p1"));
    }

    #[test]
    fn other_relations_and_blank_parents_do_not_link() {
        assert_eq!(parent_of(&req(None, Some("p1"))), None);
        assert_eq!(parent_of(&req(Some("fork"), Some("p1"))), None);
        assert_eq!(parent_of(&req(Some("followup"), Some("  "))), None);
        assert_eq!(parent_of(&req(Some("followup"), None)), None);
    }

    #[test]
    fn spawn_request_decodes_without_the_new_fields() {
        let r: SpawnRequest =
            serde_json::from_value(serde_json::json!({"machine_id": "m", "working_dir": "/w"}))
                .unwrap();
        assert!(r.relation.is_none() && r.parent_session_id.is_none());
    }
}
