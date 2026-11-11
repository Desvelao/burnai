-- Emergency STOP and pause survive a daemon restart: the single row mirrors the in-memory Controller.
CREATE TABLE control_state (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    stopped      INTEGER NOT NULL DEFAULT 0,
    paused       INTEGER NOT NULL DEFAULT 0,
    paused_until TEXT
);
INSERT INTO control_state (id) VALUES (1);
