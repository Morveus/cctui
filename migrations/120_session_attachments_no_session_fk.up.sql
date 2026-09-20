-- Spawn-modal attachments are recorded before the daemon dispatches, so the
-- worker has not registered and `sessions` has no row for the id yet — the FK
-- rejects them. `session_spawn_capabilities` (080) has no FK for exactly this
-- reason. Keep the "rows cascade away with the session" guarantee with an
-- AFTER DELETE trigger instead.
ALTER TABLE session_attachments DROP CONSTRAINT IF EXISTS session_attachments_session_id_fkey;

CREATE OR REPLACE FUNCTION delete_session_attachments() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM session_attachments WHERE session_id = OLD.id;
    RETURN OLD;
END;
$$;

DROP TRIGGER IF EXISTS sessions_delete_attachments ON sessions;
CREATE TRIGGER sessions_delete_attachments
    AFTER DELETE ON sessions
    FOR EACH ROW EXECUTE FUNCTION delete_session_attachments();
