-- Per-session cache keep-alive: `{interval_secs, max_ticks, ticks_sent, until}`
-- while enabled, NULL when off. `last_keepalive_at` is the claim timestamp the
-- reaper sweep compares-and-sets so only one replica sends a given tick.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS keepalive_json JSONB;
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS last_keepalive_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS idx_sessions_keepalive_due
    ON sessions (last_keepalive_at) WHERE keepalive_json IS NOT NULL;
