-- Retention: a setting plus indexes that keep the dedup lookup and the prune scans cheap.
ALTER TABLE automation_settings ADD COLUMN retention_days INTEGER NOT NULL DEFAULT 90;
CREATE INDEX idx_notifications_dedup ON notifications(dedup_key, created_at);
CREATE INDEX idx_notifications_created ON notifications(created_at);
CREATE INDEX idx_audit_at ON audit_log(at);
