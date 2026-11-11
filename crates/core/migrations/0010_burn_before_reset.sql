-- "Burn before reset": when a usage window resets within N hours, its threshold and reserve drop to 0.
-- NULL / 0 = off. Accounts: NULL = inherit the general setting.
ALTER TABLE automation_settings ADD COLUMN burn_session_hours REAL;
ALTER TABLE automation_settings ADD COLUMN burn_weekly_hours REAL;
ALTER TABLE accounts ADD COLUMN lim_burn_session_hours REAL;
ALTER TABLE accounts ADD COLUMN lim_burn_weekly_hours REAL;
