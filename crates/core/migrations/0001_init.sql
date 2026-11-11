CREATE TABLE providers (
    id INTEGER PRIMARY KEY,
    provider_type TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE accounts (
    id INTEGER PRIMARY KEY,
    provider_id INTEGER NOT NULL REFERENCES providers(id),
    label TEXT NOT NULL,
    subscription_plan TEXT NOT NULL DEFAULT '',
    billing_mode TEXT NOT NULL DEFAULT 'unverified',
    connection_status TEXT NOT NULL DEFAULT 'disconnected',
    config_dir TEXT,
    subscription_confirmed INTEGER NOT NULL DEFAULT 0,
    execution_policy TEXT NOT NULL DEFAULT 'approval',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(provider_id, label)
);

CREATE TABLE usage_snapshots (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id),
    session_remaining REAL,
    weekly_remaining REAL,
    session_reset_at TEXT,
    weekly_reset_at TEXT,
    confidence TEXT NOT NULL,
    source TEXT NOT NULL,
    captured_at TEXT NOT NULL
);
CREATE INDEX idx_usage_account_time ON usage_snapshots(account_id, captured_at);

CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    prompt TEXT NOT NULL,
    provider_type TEXT,
    account_id INTEGER REFERENCES accounts(id),
    allow_fallback INTEGER NOT NULL DEFAULT 0,
    model TEXT NOT NULL DEFAULT '',
    priority INTEGER NOT NULL DEFAULT 1,
    position INTEGER NOT NULL DEFAULT 0,
    estimated_consumption REAL NOT NULL DEFAULT 5,
    estimated_minutes INTEGER NOT NULL DEFAULT 10,
    maximum_budget REAL,
    timeout_secs INTEGER NOT NULL DEFAULT 1200,
    retry_max INTEGER NOT NULL DEFAULT 0,
    retry_count INTEGER NOT NULL DEFAULT 0,
    working_dir TEXT NOT NULL DEFAULT '',
    requires_approval INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'pending',
    schedule TEXT,
    expiration_at TEXT,
    result_path TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE task_dependencies (
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    dependency_task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, dependency_task_id)
);

CREATE TABLE executions (
    id INTEGER PRIMARY KEY,
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    account_id INTEGER REFERENCES accounts(id),
    started_at TEXT NOT NULL,
    completed_at TEXT,
    estimated_consumption REAL,
    actual_consumption REAL,
    status TEXT NOT NULL,
    error_message TEXT,
    result_path TEXT
);

CREATE TABLE automation_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    enabled INTEGER NOT NULL DEFAULT 0,
    execution_mode TEXT NOT NULL DEFAULT 'approval',
    approval_policy TEXT NOT NULL DEFAULT 'always',
    minimum_capacity_threshold REAL NOT NULL DEFAULT 65,
    reserved_capacity REAL NOT NULL DEFAULT 20,
    maximum_tasks_per_batch INTEGER NOT NULL DEFAULT 3,
    maximum_execution_minutes INTEGER NOT NULL DEFAULT 60,
    idle_requirement INTEGER NOT NULL DEFAULT 0,
    allowed_hours TEXT NOT NULL DEFAULT '',
    max_usage_age_minutes INTEGER NOT NULL DEFAULT 30,
    interval_normal_minutes INTEGER NOT NULL DEFAULT 5,
    interval_waiting_minutes INTEGER NOT NULL DEFAULT 2,
    quiet_hours TEXT NOT NULL DEFAULT '',
    notify_desktop INTEGER NOT NULL DEFAULT 1,
    notify_telegram INTEGER NOT NULL DEFAULT 0,
    telegram_chat_id TEXT NOT NULL DEFAULT '',
    notify_include_names INTEGER NOT NULL DEFAULT 0,
    working_root TEXT NOT NULL DEFAULT '',
    results_dir TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL
);
INSERT INTO automation_settings (id, updated_at) VALUES (1, '1970-01-01T00:00:00+00:00');

CREATE TABLE notifications (
    id INTEGER PRIMARY KEY,
    notification_type TEXT NOT NULL,
    message TEXT NOT NULL,
    dedup_key TEXT,
    created_at TEXT NOT NULL,
    read_at TEXT
);

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY,
    at TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);

CREATE TABLE task_templates (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    prompt TEXT NOT NULL,
    model TEXT NOT NULL DEFAULT '',
    estimated_consumption REAL NOT NULL DEFAULT 5,
    estimated_minutes INTEGER NOT NULL DEFAULT 10,
    created_at TEXT NOT NULL
);
