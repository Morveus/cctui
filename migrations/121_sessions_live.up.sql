-- no-transaction
--
-- 164 of 14,008 sessions are not archived, yet every live-session query seq-scans
-- all 1,908 pages of `sessions` (38k seq scans / 528M tuples since the stats
-- reset). The partial index holds only the live rows, so it is ~3 pages.
--
-- The predicate must stay character-identical to the string produced by the
-- `live_sessions_predicate!` macro in `crates/cctui-server/src/live_sessions.rs`.
-- Every live-session query ANDs that exact conjunct in alongside its own
-- stricter status filter, because the planner matches a partial index by
-- finding an equal conjunct far more reliably than by proving that a
-- `status NOT IN (...)` array test implies `status <> 'archived'`.
--
-- Non-partial `idx_sessions_status_registered_at` (migration 110) is kept: it
-- still serves the archive-only queries (`status = 'archived'`), which this
-- index by construction cannot.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_sessions_live
    ON sessions (status, registered_at DESC)
    WHERE status <> 'archived';
