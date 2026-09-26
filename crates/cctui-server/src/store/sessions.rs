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
    /// Generations below the archived root: a direct child is 1.
    pub depth: i32,
}

/// Cap on the recursion, so a `parent_id` cycle cannot spin the query.
const MAX_DESCENDANT_DEPTH: i32 = 32;

/// Every session nested under `root`, at any depth, deepest first.
///
/// Depth order is what the caller needs: a live grandchild holds its parent's
/// worktree, so it must be removed before that parent is.
pub async fn descendants(exec: impl PgExecutor<'_>, root: &str) -> Result<Vec<Child>, sqlx::Error> {
    sqlx::query_as(
        "WITH RECURSIVE tree AS ( \
             SELECT id, 1 AS depth FROM sessions WHERE parent_id = $1 \
             UNION ALL \
             SELECT s.id, t.depth + 1 FROM sessions s \
               JOIN tree t ON s.parent_id = t.id \
              WHERE t.depth < $2 \
         ) \
         SELECT t.id, \
                COALESCE((s.metadata->>'subagent')::boolean, false) AS observe_only, \
                t.depth \
           FROM tree t JOIN sessions s ON s.id = t.id \
          ORDER BY t.depth DESC, t.id",
    )
    .bind(root)
    .bind(MAX_DESCENDANT_DEPTH)
    .fetch_all(exec)
    .await
}

/// Archived descendants that own a claude job and so need a `Remove` of their
/// own, keeping the deepest-first order of [`descendants`].
pub fn job_children<'a>(children: &'a [Child], archived: &[String]) -> Vec<&'a str> {
    let mut jobs: Vec<&Child> = children
        .iter()
        .filter(|c| !c.observe_only && archived.iter().any(|id| id == &c.id))
        .collect();
    jobs.sort_by_key(|c| std::cmp::Reverse(c.depth));
    jobs.into_iter().map(|c| c.id.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::{Child, descendants, job_children};

    fn child(id: &str, observe_only: bool) -> Child {
        at_depth(id, observe_only, 1)
    }

    fn at_depth(id: &str, observe_only: bool, depth: i32) -> Child {
        Child { id: id.to_owned(), observe_only, depth }
    }

    #[test]
    fn job_children_skips_observe_only_and_unarchived() {
        let kids = [child("job-a", false), child("task-b", true), child("pinned-c", false)];
        let archived = ["job-a".to_owned(), "task-b".to_owned(), "parent".to_owned()];
        assert_eq!(job_children(&kids, &archived), ["job-a"]);
    }

    #[test]
    fn job_children_orders_the_deepest_descendant_first() {
        let kids = [
            at_depth("child", false, 1),
            at_depth("great-grandchild", false, 3),
            at_depth("grandchild", false, 2),
            at_depth("grandchild-task", true, 2),
        ];
        let archived: Vec<String> = ["child", "grandchild", "great-grandchild", "grandchild-task"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(job_children(&kids, &archived), ["great-grandchild", "grandchild", "child"]);
    }

    #[tokio::test]
    async fn descendants_recurse_and_flag_task_subagents_only() {
        let name = "descendants_recurse_and_flag_task_subagents_only";
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
        let job = format!("{parent}-job");
        let rows: [(&str, Option<&str>, serde_json::Value); 5] = [
            (&parent, None, serde_json::json!({"short": "aaaaaaaa"})),
            (
                "job",
                Some(&parent),
                serde_json::json!({"short": "bbbbbbbb", "relation": "subagent"}),
            ),
            ("task", Some(&parent), serde_json::json!({"subagent": true, "agent_id": "a1"})),
            ("bare", Some(&parent), serde_json::json!({})),
            ("grandchild", Some(&job), serde_json::json!({"short": "cccccccc"})),
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

        let got = descendants(&pool, &parent).await.unwrap();
        // Deepest first; ties broken by id.
        assert_eq!(
            got,
            [
                at_depth(&format!("{parent}-grandchild"), false, 2),
                child(&format!("{parent}-bare"), false),
                child(&format!("{parent}-job"), false),
                child(&format!("{parent}-task"), true),
            ]
        );

        let archived: Vec<String> = got.iter().map(|c| c.id.clone()).collect();
        let order = job_children(&got, &archived);
        let position = |id: &str| order.iter().position(|seen| *seen == id).unwrap();
        assert!(
            position(&format!("{parent}-grandchild")) < position(&format!("{parent}-job")),
            "a grandchild must be removed before the child that owns its worktree"
        );
        assert!(!order.contains(&format!("{parent}-task").as_str()));

        for sql in [
            "DELETE FROM sessions WHERE user_id = $1",
            "DELETE FROM machines WHERE user_id = $1",
            "DELETE FROM users WHERE id = $1",
        ] {
            sqlx::query(sql).bind(uid).execute(&pool).await.unwrap();
        }
    }
}
