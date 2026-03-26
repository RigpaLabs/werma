-- Effects outbox table
CREATE TABLE IF NOT EXISTS effects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dedup_key TEXT NOT NULL,
    task_id TEXT NOT NULL,
    issue_id TEXT NOT NULL DEFAULT '',
    effect_type TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    blocking INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK(status IN ('pending','running','done','failed','dead')),
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    created_at TEXT NOT NULL,
    next_retry_at TEXT,
    executed_at TEXT,
    error TEXT,
    FOREIGN KEY (task_id) REFERENCES tasks(id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_effects_dedup ON effects(dedup_key);
CREATE INDEX IF NOT EXISTS idx_effects_pending ON effects(status, next_retry_at) WHERE status IN ('pending', 'failed');
CREATE INDEX IF NOT EXISTS idx_effects_task ON effects(task_id);

-- Handoff content stored in DB instead of filesystem
ALTER TABLE tasks ADD COLUMN handoff_content TEXT NOT NULL DEFAULT '';
