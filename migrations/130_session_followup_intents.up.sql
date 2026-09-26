-- Follow-up linkage chosen at spawn time, waiting for the session to register.
-- Keyed by the spawn key like `session_label_intents`; claimed on registration
-- into `sessions.parent_id` + `metadata.relation = "followup"`. Rows nobody
-- claims are pruned by the reaper.
CREATE TABLE IF NOT EXISTS session_followup_intents (
  spawn_key         TEXT PRIMARY KEY,
  parent_session_id TEXT NOT NULL,
  created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
