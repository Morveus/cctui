DROP TABLE IF EXISTS account_window_closes;
DROP INDEX IF EXISTS account_usage_samples_sampled_at_idx;
ALTER TABLE account_usage_samples
  DROP COLUMN IF EXISTS source,
  DROP COLUMN IF EXISTS amount_usd;
