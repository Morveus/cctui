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
    -- Set, and committed, before the command is sent: a claimed row is never
    -- sent again, so a crash between the send and the cleanup cannot launch
    -- the same work twice. The reaper reconciles claims left behind.
    claimed_at    TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_spawn_queue_machine ON spawn_queue (machine_uuid, queued_at);

-- Launches let through on a machine with a ceiling, so a burst is judged on
-- what it is about to use and not only on the last heartbeat's figures.
CREATE TABLE IF NOT EXISTS machine_admissions (
    machine_uuid UUID NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
    admitted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_machine_admissions ON machine_admissions (machine_uuid, admitted_at);
