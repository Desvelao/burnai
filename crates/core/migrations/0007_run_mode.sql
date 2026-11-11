ALTER TABLE accounts ADD COLUMN run_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE tasks ADD COLUMN run_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE automation_settings ADD COLUMN default_run_mode TEXT NOT NULL DEFAULT 'inherit';
