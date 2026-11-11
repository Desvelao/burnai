-- Recurring tasks: when a task finishes, the next instance is queued per this rule (NULL = one-off).
ALTER TABLE tasks ADD COLUMN repeat TEXT;
