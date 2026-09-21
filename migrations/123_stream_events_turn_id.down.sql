-- Reverting narrows the dedup key back to content alone, which the rows
-- written while the wider key was live can violate: two same-text turns
-- distinguished only by turn_id. Collapse those first, as migration 020 did,
-- or the unique index cannot be built.

DELETE FROM stream_events s
USING stream_events t
WHERE s.session_id = t.session_id
  AND s.event_type = t.event_type
  AND s.content_hash = t.content_hash
  AND s.id > t.id;

CREATE UNIQUE INDEX IF NOT EXISTS stream_events_dedup_idx
    ON stream_events (session_id, event_type, content_hash);

DROP INDEX IF EXISTS stream_events_dedup_turn_idx;

DROP INDEX IF EXISTS idx_stream_events_turn_id;

ALTER TABLE stream_events
    DROP COLUMN IF EXISTS turn_id;
