ALTER TABLE accounts ADD COLUMN credentials_file TEXT;
ALTER TABLE accounts ADD COLUMN usage_source TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE accounts ADD COLUMN user_agent TEXT;
