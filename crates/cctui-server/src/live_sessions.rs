//! `idx_sessions_live` (migration 117) is partial on exactly
//! `status <> 'archived'`. The planner matches an identical conjunct
//! reliably; it will not reliably prove that `status NOT IN ('archived',
//! 'ended')` implies it. So every live-session query ANDs this conjunct in
//! verbatim on top of its own stricter status filter, and rewording it here
//! requires rewording migration 117.

/// Expands to the `status <> 'archived'` conjunct, optionally table-qualified.
/// A macro, not a `const`, so it composes inside `concat!` in const SQL.
macro_rules! live_sessions_predicate {
    () => {
        "status <> 'archived'"
    };
    ($alias:literal) => {
        concat!($alias, ".status <> 'archived'")
    };
}

pub(crate) use live_sessions_predicate;

#[cfg(test)]
mod tests {
    #[test]
    fn predicate_matches_the_partial_index_in_migration_117() {
        let migration = include_str!("../../../migrations/117_sessions_live.up.sql");
        assert!(
            migration.contains(&format!("WHERE {}", live_sessions_predicate!())),
            "migration 117's index predicate drifted from live_sessions_predicate!"
        );
    }

    #[test]
    fn qualified_form_prefixes_the_alias() {
        assert_eq!(live_sessions_predicate!("s"), "s.status <> 'archived'");
    }
}
