-- Whether the gateway re-serialized the request that produced this turn.
-- Recorded at proxy time so a cache bust can be attributed to the gateway
-- instead of guessed at from the token counts alone.
ALTER TABLE session_token_usage
  ADD COLUMN IF NOT EXISTS gateway_rewrote_body BOOLEAN NOT NULL DEFAULT FALSE;
