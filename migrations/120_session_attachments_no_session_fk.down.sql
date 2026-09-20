DROP TRIGGER IF EXISTS sessions_delete_attachments ON sessions;
DROP FUNCTION IF EXISTS delete_session_attachments();

DELETE FROM session_attachments a
 WHERE NOT EXISTS (SELECT 1 FROM sessions s WHERE s.id = a.session_id);

ALTER TABLE session_attachments
    ADD CONSTRAINT session_attachments_session_id_fkey
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE;
