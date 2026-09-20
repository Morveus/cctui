-- no-transaction
--
-- The sessions list takes the newest `message` event per listed session
-- (`DISTINCT ON (session_id) … ORDER BY session_id, created_at DESC` in
-- `routes/sessions.rs`). Through the general dedup index that walks 6,530 rows
-- to return 148, ~6,900 buffers per refresh; with this index each session is
-- one probe of the DESC leaf. The unread-count query over the same
-- `session_ids` fan-out shares it.
--
-- The predicate must stay character-identical to the `event_type = 'message'`
-- condition in both of those queries, and neither may gain a `role` filter
-- without adding it here, or the planner stops matching the index.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_stream_events_latest_message
    ON stream_events (session_id, created_at DESC)
    WHERE event_type = 'message';
