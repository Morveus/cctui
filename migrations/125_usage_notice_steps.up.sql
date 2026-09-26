-- Last notified step bucket per (session, usage window). Replaces the
-- per-replica in-memory DashMap: two replicas and every restart each fired
-- their own "first notice per window", so a session was notified 58x/day.
CREATE TABLE IF NOT EXISTS usage_notice_steps (
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  window_key  TEXT NOT NULL,
  step        INTEGER NOT NULL,
  notified_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (session_id, window_key)
);
