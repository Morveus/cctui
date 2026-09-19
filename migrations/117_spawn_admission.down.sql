DROP TABLE IF EXISTS machine_admissions;
DROP TABLE IF EXISTS spawn_queue;
DELETE FROM sessions WHERE status = 'queued';
ALTER TABLE machines DROP COLUMN IF EXISTS mem_ceiling_bytes;
