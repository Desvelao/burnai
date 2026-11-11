-- Per-account eligibility overrides. NULL = inherit the general setting.
ALTER TABLE accounts ADD COLUMN lim_start_threshold REAL;
ALTER TABLE accounts ADD COLUMN lim_reserved_capacity REAL;
ALTER TABLE accounts ADD COLUMN lim_idle_requirement INTEGER;
ALTER TABLE accounts ADD COLUMN lim_allowed_hours TEXT;
ALTER TABLE accounts ADD COLUMN lim_max_usage_age_minutes INTEGER;
ALTER TABLE accounts ADD COLUMN lim_max_tasks_per_batch INTEGER;
ALTER TABLE accounts ADD COLUMN lim_max_execution_minutes INTEGER;
-- Session (5 h) window rules; NULL = same as the weekly/general rule.
ALTER TABLE accounts ADD COLUMN lim_session_threshold REAL;
ALTER TABLE accounts ADD COLUMN lim_session_reserved REAL;
ALTER TABLE accounts ADD COLUMN lim_ignore_session INTEGER NOT NULL DEFAULT 0;
-- Persistent "eligible ignoring thresholds/reserve/idle/hours" switch.
ALTER TABLE accounts ADD COLUMN lim_ignore_limits INTEGER NOT NULL DEFAULT 0;
