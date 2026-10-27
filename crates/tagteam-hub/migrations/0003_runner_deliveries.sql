CREATE TABLE runner_deliveries (
    id TEXT PRIMARY KEY,
    subtask_id TEXT NOT NULL REFERENCES subtasks(id),
    kind TEXT NOT NULL CHECK (kind IN ('answer', 'steer')),
    idempotency_key TEXT NOT NULL,
    payload JSON NOT NULL,
    created_at INTEGER NOT NULL,
    sent_at INTEGER,
    acknowledged_at INTEGER,
    UNIQUE (subtask_id, kind, idempotency_key)
);

CREATE INDEX pending_runner_deliveries
    ON runner_deliveries(subtask_id, created_at, id)
    WHERE acknowledged_at IS NULL;
