-- CCT-1055: persist the permission posture the claude-code transcript reports
-- via AdapterEvent::Status. It is in neither state.json nor the control
-- socket, so before this the only way it reached the UI was a timeline marker
-- bubble per record.
ALTER TABLE sessions ADD COLUMN IF NOT EXISTS permission_mode TEXT;
