ALTER TABLE machines
  DROP COLUMN IF EXISTS harness_report_at,
  DROP COLUMN IF EXISTS harness_report,
  DROP COLUMN IF EXISTS harness_autoupdate;
DELETE FROM instance_settings WHERE key = 'harness_autoupdate';
