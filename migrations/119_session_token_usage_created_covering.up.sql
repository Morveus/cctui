-- no-transaction
--
-- `GET /sessions/stats/tokens` range-scans `session_token_usage` on
-- `created_at` and then fetches ~125,000 heap rows, ~92,000 buffers per call
-- (seconds cold). The INCLUDE list is exactly what the conditional-aggregate
-- scan in `routes/stats.rs` (`session_token_stats`) reads: `session_id` for the
-- join to `sessions`, and the three summed columns. Anything else would only
-- bloat the index -- in particular `cache_creation_tokens` and `model`, which
-- that query does not read (they belong to the `/sessions/stats/usage`
-- aggregates, a separate scan not covered here).
--
-- Index-only also needs a current visibility map: this table had never been
-- autovacuumed, so run `VACUUM (ANALYZE) session_token_usage` once after
-- applying and check the autovacuum thresholds.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_session_token_usage_created_covering
    ON session_token_usage (created_at)
    INCLUDE (session_id, input_tokens, output_tokens, cache_read_tokens);
