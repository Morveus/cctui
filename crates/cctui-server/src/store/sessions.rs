use sqlx::PgExecutor;

use crate::routes::sessions::DbSession;

pub async fn set_inactive(
    exec: impl PgExecutor<'_>,
    id: &str,
    require_archived: bool,
) -> Result<(), sqlx::Error> {
    let sql = if require_archived {
        "UPDATE sessions SET status = 'inactive' WHERE id = $1 AND status = 'archived'"
    } else {
        "UPDATE sessions SET status = 'inactive' WHERE id = $1"
    };
    sqlx::query(sql).bind(id).execute(exec).await?;
    Ok(())
}

pub async fn fetch_by_id(
    exec: impl PgExecutor<'_>,
    id: &str,
) -> Result<Option<DbSession>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.id, s.parent_id, s.machine_id, s.working_dir, s.status, \
                s.registered_at, s.last_heartbeat, s.metadata, s.adapter_id, \
                COALESCE(m.display_name, m.name) AS resolved_machine_name, \
                m.hue AS resolved_machine_hue, m.kind AS resolved_machine_kind \
         FROM sessions s \
         LEFT JOIN machines m ON m.id = s.machine_uuid \
         WHERE s.id = $1",
    )
    .bind(id)
    .fetch_optional(exec)
    .await
}

pub async fn adapter_id(
    exec: impl PgExecutor<'_>,
    id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT adapter_id FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(exec)
        .await
}

pub async fn working_dir(
    exec: impl PgExecutor<'_>,
    id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT working_dir FROM sessions WHERE id = $1")
        .bind(id)
        .fetch_optional(exec)
        .await
}

/// A session nested under a parent. `observe_only` marks a Task-tool subagent:
/// a transcript the daemon tails with no worker or job of its own, so it never
/// needs a `Remove`. Everything else (`CctuiAgent` children, forks) is a real job.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Child {
    pub id: String,
    pub observe_only: bool,
}

pub async fn children(
    exec: impl PgExecutor<'_>,
    parent_id: &str,
) -> Result<Vec<Child>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, COALESCE((metadata->>'subagent')::boolean, false) AS observe_only \
         FROM sessions WHERE parent_id = $1",
    )
    .bind(parent_id)
    .fetch_all(exec)
    .await
}

/// Archived children that own a claude job and so need a `Remove` of their own.
pub fn job_children<'a>(children: &'a [Child], archived: &[String]) -> Vec<&'a str> {
    children
        .iter()
        .filter(|c| !c.observe_only && archived.iter().any(|id| id == &c.id))
        .map(|c| c.id.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Child, children, job_children};

    fn child(id: &str, observe_only: bool) -> Child {
        Child { id: id.to_owned(), observe_only }
    }

    #[test]
    fn job_children_skips_observe_only_and_unarchived() {
        let kids = vec![child("job-a", false), child("task-b", true), child("pinned-c", false)];
        let archived = vec!["job-a".to_owned(), "task-b".to_owned(), "parent".to_owned()];
        assert_eq!(job_children(&kids, &archived), vec!["job-a"]);
    }

    #[tokio::test]
    async fn children_flags_task_subagents_only() {
        let name = "children_flags_task_subagents_only";
        let Some(url) = crate::routes::gateway::test_db_url(name) else { return };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect test db");
        let uid = uuid::Uuid::new_v4();
        let machine = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, name, key_hash) VALUES ($1, $2, $3)")
            .bind(uid)
            .bind(format!("{name}-{uid}"))
            .bind(format!("h-{uid}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO machines (id, user_id, name, key_hash) VALUES ($1, $2, 'm', $3)")
            .bind(machine)
            .bind(uid)
            .bind(format!("mk-{machine}"))
            .execute(&pool)
            .await
            .unwrap();
        let parent = format!("{name}-parent-{uid}");
        let rows: [(&str, Option<&str>, serde_json::Value); 4] = [
            (&parent, None, serde_json::json!({"short": "aaaaaaaa"})),
            (
                "job",
                Some(&parent),
                serde_json::json!({"short": "bbbbbbbb", "relation": "subagent"}),
            ),
            ("task", Some(&parent), serde_json::json!({"subagent": true, "agent_id": "a1"})),
            ("bare", Some(&parent), serde_json::json!({})),
        ];
        for (suffix, parent_id, metadata) in rows {
            let id =
                if parent_id.is_none() { suffix.to_owned() } else { format!("{parent}-{suffix}") };
            sqlx::query(
                "INSERT INTO sessions (id, parent_id, machine_id, machine_uuid, user_id, \
                 working_dir, status, metadata) VALUES ($1, $2, $3, $3, $4, '/w', 'active', $5)",
            )
            .bind(&id)
            .bind(parent_id)
            .bind(machine)
            .bind(uid)
            .bind(metadata)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut got = children(&pool, &parent).await.unwrap();
        got.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            got,
            vec![
                child(&format!("{parent}-bare"), false),
                child(&format!("{parent}-job"), false),
                child(&format!("{parent}-task"), true),
            ]
        );

        for sql in [
            "DELETE FROM sessions WHERE user_id = $1",
            "DELETE FROM machines WHERE user_id = $1",
            "DELETE FROM users WHERE id = $1",
        ] {
            sqlx::query(sql).bind(uid).execute(&pool).await.unwrap();
        }
    }
}
