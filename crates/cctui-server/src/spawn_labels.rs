//! Labels chosen at spawn time.
//!
//! A spawn that carries `label_ids` leaves an intent row keyed by its spawn key
//! (the pre-minted session id for claude-code, the command id for adapters
//! that mint their own id), the same key `auto_archive` uses. When the daemon
//! registers the session, the intent is claimed and the labels are attached.
//! This is what lets a draft launch keep its labels: the launch returns before
//! the worker registers, so nothing on the client side knows the session id.
//!
//! A label deleted between the spawn and the registration is skipped, never
//! an error: labels are best-effort and must not fail the spawn.

use sqlx::PgPool;
use uuid::Uuid;

/// Unclaimed intents older than this are dropped: the spawn never registered.
const INTENT_TTL_SECS: i64 = 6 * 3600;

/// The label ids of a spawn request that parse as uuids, deduplicated in their
/// original order. A malformed id is dropped rather than refused, like a
/// deleted label.
pub fn parse_label_ids(raw: &[String]) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = Vec::new();
    for id in raw.iter().filter_map(|s| Uuid::parse_str(s.trim()).ok()) {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Remember the labels the session spawned under `spawn_key` should carry.
/// Best-effort: a lost intent leaves the session unlabelled.
pub async fn remember_intent(pool: &PgPool, spawn_key: &str, label_ids: &[String]) {
    let ids = parse_label_ids(label_ids);
    if ids.is_empty() {
        return;
    }
    if let Err(e) = sqlx::query(
        "INSERT INTO session_label_intents (spawn_key, label_ids) VALUES ($1, $2) \
         ON CONFLICT (spawn_key) DO UPDATE SET label_ids = EXCLUDED.label_ids",
    )
    .bind(spawn_key)
    .bind(&ids)
    .execute(pool)
    .await
    {
        tracing::warn!(%spawn_key, error = %e, "could not record the spawn labels");
    }
}

/// Attach the labels a pending intent holds to the session that just
/// registered. The session is looked up by its own id and by the spawn key the
/// adapter echoed, so both pre-minted (claude-code) and self-minted (codex)
/// ids resolve.
pub async fn claim_intent(pool: &PgPool, session_id: &str, spawn_key: Option<&str>) {
    let key = spawn_key.unwrap_or(session_id);
    let ids = match sqlx::query_scalar::<_, Vec<Uuid>>(
        "DELETE FROM session_label_intents WHERE spawn_key = $1 OR spawn_key = $2 \
         RETURNING label_ids",
    )
    .bind(session_id)
    .bind(key)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows.into_iter().flatten().collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(%session_id, error = %e, "spawn labels lookup failed");
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    attach(pool, session_id, &ids).await;
}

/// Attach existing labels to a session, skipping any that no longer exist.
async fn attach(pool: &PgPool, session_id: &str, ids: &[Uuid]) {
    match sqlx::query(
        "INSERT INTO session_labels (session_id, label_id) \
         SELECT $1, l.id FROM labels l WHERE l.id = ANY($2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(session_id)
    .bind(ids)
    .execute(pool)
    .await
    {
        Ok(res) => {
            tracing::info!(%session_id, attached = res.rows_affected(), "spawn labels attached");
        }
        Err(e) => tracing::warn!(%session_id, error = %e, "could not attach the spawn labels"),
    }
}

/// Make a draft row show the labels its payload carries, so the draft card
/// displays what the launch will attach.
pub async fn sync_draft(pool: &PgPool, draft_id: &str, label_ids: &[String]) {
    let ids = parse_label_ids(label_ids);
    let res = async {
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM session_labels WHERE session_id = $1")
            .bind(draft_id)
            .execute(&mut *tx)
            .await?;
        if !ids.is_empty() {
            sqlx::query(
                "INSERT INTO session_labels (session_id, label_id) \
                 SELECT $1, l.id FROM labels l WHERE l.id = ANY($2) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(draft_id)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(%draft_id, error = %e, "could not sync the draft labels");
    }
}

/// Labels currently attached to a draft row: the payload's, plus any the
/// operator put on the draft card after saving it.
pub async fn draft_label_ids(pool: &PgPool, draft_id: &str) -> Vec<String> {
    sqlx::query_scalar::<_, Uuid>("SELECT label_id FROM session_labels WHERE session_id = $1")
        .bind(draft_id)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(%draft_id, error = %e, "could not read the draft labels");
            Vec::new()
        })
        .into_iter()
        .map(|id| id.to_string())
        .collect()
}

/// Reaper tick: drop intents whose spawn never registered.
pub async fn sweep(pool: &PgPool) {
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds(INTENT_TTL_SECS);
    if let Err(e) = sqlx::query("DELETE FROM session_label_intents WHERE created_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "spawn labels intent prune failed");
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::{claim_intent, draft_label_ids, parse_label_ids, remember_intent, sync_draft};

    #[test]
    fn parse_label_ids_drops_malformed_and_duplicates() {
        let a = "0b7c6f1e-2d4a-4a8e-9f3c-1a2b3c4d5e6f";
        let b = "5f0e8d7c-6b5a-4938-8271-605f4e3d2c1b";
        let raw = vec![a.to_owned(), "not-a-uuid".to_owned(), format!(" {b} "), a.to_owned()];
        let ids: Vec<String> = parse_label_ids(&raw).iter().map(ToString::to_string).collect();
        assert_eq!(ids, vec![a.to_owned(), b.to_owned()]);
    }

    #[test]
    fn parse_label_ids_empty() {
        assert!(parse_label_ids(&[]).is_empty());
    }

    async fn test_pool(test_name: &str) -> Option<PgPool> {
        let url = crate::routes::gateway::test_db_url(test_name)?;
        Some(
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .expect("connect test db"),
        )
    }

    async fn label(pool: &PgPool) -> Uuid {
        sqlx::query_scalar("INSERT INTO labels (name, color) VALUES ($1, '#abcdef') RETURNING id")
            .bind(format!("spawn-labels-{}", Uuid::new_v4()))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn session(pool: &PgPool, status: &str) -> String {
        let uid = Uuid::new_v4();
        let machine = Uuid::new_v4();
        let sid = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("spawn-labels-{uid}"))
            .bind(format!("hsl-{uid}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO machines (id, user_id, name, key_hash) VALUES ($1, $2, 'm', $3)")
            .bind(machine)
            .bind(uid)
            .bind(format!("mksl-{machine}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, machine_id, machine_uuid, user_id, working_dir, status) \
             VALUES ($1, $2, $2, $3, '/w', $4)",
        )
        .bind(&sid)
        .bind(machine)
        .bind(uid)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
        sid
    }

    async fn labels_of(pool: &PgPool, sid: &str) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT label_id FROM session_labels WHERE session_id = $1")
                .bind(sid)
                .fetch_all(pool)
                .await
                .unwrap();
        ids.sort();
        ids
    }

    /// The draft launch path: the labels ride the spawn key and land on the
    /// session the worker registers under its own id; a label deleted in
    /// between is skipped, and the intent is consumed.
    #[tokio::test]
    async fn spawn_labels_attach_on_registration() {
        let Some(pool) = test_pool("spawn_labels_attach_on_registration").await else {
            return;
        };
        let (a, b, gone) = (label(&pool).await, label(&pool).await, label(&pool).await);
        let command_id = Uuid::new_v4().to_string();
        remember_intent(
            &pool,
            &command_id,
            &[a.to_string(), gone.to_string(), "junk".to_owned(), b.to_string()],
        )
        .await;
        sqlx::query("DELETE FROM labels WHERE id = $1").bind(gone).execute(&pool).await.unwrap();

        let sid = session(&pool, "active").await;
        claim_intent(&pool, &sid, Some(&command_id)).await;
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(labels_of(&pool, &sid).await, want);

        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM session_label_intents WHERE spawn_key = $1")
                .bind(&command_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0, "the intent is consumed on claim");

        // Pre-minted ids (claude-code) register under the spawn key itself.
        let pre = session(&pool, "active").await;
        remember_intent(&pool, &pre, &[a.to_string()]).await;
        claim_intent(&pool, &pre, None).await;
        assert_eq!(labels_of(&pool, &pre).await, vec![a]);
    }

    /// A draft row mirrors its payload's labels, and every save replaces them.
    #[tokio::test]
    async fn spawn_labels_sync_draft_row() {
        let Some(pool) = test_pool("spawn_labels_sync_draft_row").await else {
            return;
        };
        let (a, b) = (label(&pool).await, label(&pool).await);
        let draft = session(&pool, "draft").await;

        sync_draft(&pool, &draft, &[a.to_string(), b.to_string()]).await;
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(labels_of(&pool, &draft).await, want);

        sync_draft(&pool, &draft, &[b.to_string()]).await;
        assert_eq!(draft_label_ids(&pool, &draft).await, vec![b.to_string()]);

        sync_draft(&pool, &draft, &[]).await;
        assert!(labels_of(&pool, &draft).await.is_empty());
    }
}
