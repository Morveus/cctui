-- Messages scheduled for later delivery to a session. Holds only the text the
-- user typed: gateway credentials are minted at delivery time, never stored.
CREATE TABLE IF NOT EXISTS session_message_queue (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    user_id         UUID REFERENCES users(id) ON DELETE CASCADE,
    body            TEXT NOT NULL,
    ask_picks       JSONB,
    -- scheduled | sending | sent | cancelled | dead
    state           TEXT NOT NULL DEFAULT 'scheduled',
    deliver_at      TIMESTAMPTZ NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL,
    last_error      TEXT,
    -- Identity of the delivered turn, so the transcript can mark it scheduled.
    turn_id         UUID NOT NULL DEFAULT gen_random_uuid(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at         TIMESTAMPTZ,
    -- human | macro
    origin          TEXT NOT NULL DEFAULT 'human',
    CONSTRAINT session_message_queue_state_check
        CHECK (state IN ('scheduled', 'sending', 'sent', 'cancelled', 'dead')),
    CONSTRAINT session_message_queue_origin_check CHECK (origin IN ('human', 'macro'))
);

CREATE INDEX IF NOT EXISTS idx_session_message_queue_due
    ON session_message_queue (state, deliver_at)
    WHERE state = 'scheduled';

CREATE INDEX IF NOT EXISTS idx_session_message_queue_session
    ON session_message_queue (session_id, created_at);
