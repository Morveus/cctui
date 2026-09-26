-- Who archived the session: 'user' or 'automatic'. NULL for sessions archived
-- before this column existed.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS archived_by TEXT;
