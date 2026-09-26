DROP INDEX IF EXISTS idx_sessions_keepalive_due;
ALTER TABLE sessions DROP COLUMN IF EXISTS last_keepalive_at;
ALTER TABLE sessions DROP COLUMN IF EXISTS keepalive_json;
