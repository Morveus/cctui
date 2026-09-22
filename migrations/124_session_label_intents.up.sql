-- Labels chosen at spawn time, waiting for the session to register. Keyed by
-- the spawn key the gateway token is bound to (the pre-minted session id for
-- claude-code, the command id otherwise), like `session_auto_archive`: machine
-- spawns and draft launches return before the worker registers its own id, so
-- the server holds the labels and attaches them on registration. Rows nobody
-- claims are pruned by the reaper.
CREATE TABLE IF NOT EXISTS session_label_intents (
  spawn_key  TEXT PRIMARY KEY,
  label_ids  UUID[] NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
