-- Usage history: samples kept for 90 days (pruned by the reaper), tagged with
-- where they came from, plus one row per closed quota window recording how
-- much of it was used before it reset.
ALTER TABLE account_usage_samples
  ADD COLUMN IF NOT EXISTS amount_usd DOUBLE PRECISION,
  ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'poll';

CREATE INDEX IF NOT EXISTS account_usage_samples_sampled_at_idx
  ON account_usage_samples (sampled_at);

CREATE TABLE IF NOT EXISTS account_window_closes (
    provider_id       UUID             NOT NULL REFERENCES account_providers(id) ON DELETE CASCADE,
    window_key        TEXT             NOT NULL,
    resets_at         TIMESTAMPTZ      NOT NULL,
    final_utilization DOUBLE PRECISION NOT NULL,
    wasted_pct        DOUBLE PRECISION NOT NULL,
    closed_at         TIMESTAMPTZ      NOT NULL DEFAULT now(),
    source            TEXT             NOT NULL DEFAULT 'sample',
    PRIMARY KEY (provider_id, window_key, resets_at)
);

CREATE INDEX IF NOT EXISTS account_window_closes_closed_at_idx
  ON account_window_closes (closed_at);
