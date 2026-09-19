-- Spawn admission: an optional per-machine RAM ceiling. NULL (the default)
-- means no ceiling, so upgrading changes nothing until an operator sets one.
ALTER TABLE machines ADD COLUMN IF NOT EXISTS mem_ceiling_bytes BIGINT;

-- Spawns held back because their machine was over its ceiling. The session
-- row (status 'queued') is what the UI shows; this row holds what the launch
-- needs and the UI must never see: the env, encrypted with the vault key, and
-- the staged uploads.
CREATE TABLE IF NOT EXISTS spawn_queue (
    session_id    TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    machine_uuid  UUID NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
    -- Who asked, by identity only: their rights are re-derived from the
    -- current ACLs at launch, so a revocation or demotion meanwhile applies.
    caller_id     UUID NOT NULL,
    caller_key_id UUID NOT NULL,
    request       JSONB NOT NULL,
    env_enc       TEXT,
    uploads       JSONB,
    queued_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Stable id of the launch command, the same on every attempt.
    command_id    UUID NOT NULL,
    -- 'waiting'   : nothing sent, the reaper may launch it;
    -- 'sending'   : an attempt is in flight. Written and committed BEFORE the
    --               command is sent, so no second attempt can start;
    -- 'uncertain' : the send broke, or the server died mid-attempt: nobody can
    --               tell whether the daemon got it. The request is kept as is,
    --               never sent again on its own, and a human settles it.
    state         TEXT NOT NULL DEFAULT 'waiting'
                  CHECK (state IN ('waiting', 'sending', 'uncertain')),
    -- When the in-flight attempt started, for the reaper to settle the ones
    -- whose server died.
    sending_since TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_spawn_queue_machine ON spawn_queue (machine_uuid, queued_at);

-- Launches let through on a machine with a ceiling, so a burst is judged on
-- what it is about to use and not only on the last heartbeat's figures.
CREATE TABLE IF NOT EXISTS machine_admissions (
    machine_uuid UUID NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
    admitted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_machine_admissions ON machine_admissions (machine_uuid, admitted_at);
