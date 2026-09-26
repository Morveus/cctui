-- Per-machine harness auto-update override (NULL inherits the instance
-- default in instance_settings.harness_autoupdate) and the last report the
-- daemon's heartbeat carried.
ALTER TABLE machines
  ADD COLUMN IF NOT EXISTS harness_autoupdate JSONB,
  ADD COLUMN IF NOT EXISTS harness_report JSONB,
  ADD COLUMN IF NOT EXISTS harness_report_at TIMESTAMPTZ;
