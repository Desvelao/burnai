-- Templates now carry full task defaults (never the target account, schedule, expiry or dependencies).
ALTER TABLE task_templates ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE task_templates ADD COLUMN priority INTEGER NOT NULL DEFAULT 1;
ALTER TABLE task_templates ADD COLUMN run_mode TEXT NOT NULL DEFAULT 'inherit';
ALTER TABLE task_templates ADD COLUMN maximum_budget REAL;
ALTER TABLE task_templates ADD COLUMN timeout_secs INTEGER NOT NULL DEFAULT 1200;
ALTER TABLE task_templates ADD COLUMN retry_max INTEGER NOT NULL DEFAULT 0;
ALTER TABLE task_templates ADD COLUMN working_dir TEXT NOT NULL DEFAULT '';
ALTER TABLE task_templates ADD COLUMN requires_approval INTEGER NOT NULL DEFAULT 1;
-- NULL = any provider.
ALTER TABLE task_templates ADD COLUMN provider_type TEXT DEFAULT 'claude';
ALTER TABLE task_templates ADD COLUMN allow_fallback INTEGER NOT NULL DEFAULT 0;
