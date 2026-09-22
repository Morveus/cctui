-- End-to-end turn identity (CCT-1071).
--
-- Claude stores one logical human turn as several rows in several text
-- encodings (composer prose plus staged paths, the `[Image #N]`-prefixed copy,
-- the synthetic `[Image: …]` line). `turn_id` is minted by the client that sent
-- the turn and carried through the daemon onto every event the turn produces,
-- so clients collapse them by identity. NULL for turns cctui did not originate
-- (typed into Claude's own TUI) and for every row written before this column,
-- which keep using the client-side content fallback.
--
-- `stream_events_dedup_idx` (migration 019) is kept, not dropped: it is what
-- makes the daemon's post-reconnect replay idempotent (CCT-92), and a
-- turn_id-less row still has no other identity. But it is now wrong as it
-- stood. Two identical human turns in one session — `continue`, a repeated cron
-- prompt — hash the same, so the second was never inserted and, because
-- `newly_inserted` gates the broadcast, never reached any client either
-- (CCT-931). Distinct turn_ids are proof those are distinct turns, so the key
-- gains the turn under COALESCE: rows without one collapse to the nil UUID and
-- behave exactly as before (replay idempotency intact), a replay of the same
-- turn still carries the same turn_id and still conflicts, and two genuinely
-- distinct sends of the same text no longer erase one another.
--
-- The ON CONFLICT target in `insert_event` (routes/daemon.rs) must stay
-- character-identical to this index's expression list or inference fails and
-- the insert errors outright.
--
-- On a populated database build both indexes out of band with CONCURRENTLY
-- before deploying, so the IF NOT EXISTS clauses here are no-ops — this file
-- holds several statements and therefore cannot use CONCURRENTLY itself
-- (Postgres runs a multi-statement simple query in an implicit transaction
-- block, which CONCURRENTLY refuses; see migration 108). A failed CONCURRENTLY
-- build leaves an INVALID index that IF NOT EXISTS will skip: check
-- pg_index.indisvalid.

ALTER TABLE stream_events
    ADD COLUMN IF NOT EXISTS turn_id uuid;

CREATE INDEX IF NOT EXISTS idx_stream_events_turn_id
    ON stream_events (session_id, turn_id)
    WHERE turn_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS stream_events_dedup_turn_idx
    ON stream_events (
        session_id,
        event_type,
        content_hash,
        COALESCE(turn_id, '00000000-0000-0000-0000-000000000000'::uuid)
    );

DROP INDEX IF EXISTS stream_events_dedup_idx;
