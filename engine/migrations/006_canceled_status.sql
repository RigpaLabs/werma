-- Extend the status CHECK constraint to include 'canceled'.
-- SQLite does not support ALTER TABLE ... MODIFY COLUMN, so we recreate the table.
PRAGMA foreign_keys=OFF;

BEGIN;

CREATE TABLE tasks_new (
    id              TEXT PRIMARY KEY,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK(status IN ('pending','running','completed','failed','canceled')),
    priority        INTEGER NOT NULL DEFAULT 2,
    created_at      TEXT NOT NULL,
    started_at      TEXT,
    finished_at     TEXT,
    type            TEXT NOT NULL DEFAULT 'custom',
    prompt          TEXT NOT NULL,
    output_path     TEXT DEFAULT '',
    working_dir     TEXT NOT NULL,
    model           TEXT NOT NULL DEFAULT 'sonnet',
    max_turns       INTEGER NOT NULL DEFAULT 15,
    allowed_tools   TEXT DEFAULT '',
    session_id      TEXT DEFAULT '',
    issue_identifier TEXT DEFAULT '',
    linear_pushed   INTEGER DEFAULT 0,
    pipeline_stage  TEXT DEFAULT '',
    depends_on      TEXT DEFAULT '[]',
    context_files   TEXT DEFAULT '[]',
    repo_hash       TEXT DEFAULT '',
    estimate        INTEGER DEFAULT 0,
    callback_fired_at TEXT DEFAULT ''
);

INSERT INTO tasks_new SELECT
    id, status, priority, created_at, started_at, finished_at,
    type, prompt, output_path, working_dir, model, max_turns,
    allowed_tools, session_id, issue_identifier, linear_pushed,
    pipeline_stage, depends_on, context_files,
    COALESCE(repo_hash, ''),
    COALESCE(estimate, 0),
    COALESCE(callback_fired_at, '')
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX IF NOT EXISTS idx_tasks_status_priority ON tasks(status, priority);
CREATE INDEX IF NOT EXISTS idx_tasks_linear ON tasks(issue_identifier) WHERE issue_identifier != '';
CREATE INDEX IF NOT EXISTS idx_tasks_pipeline ON tasks(pipeline_stage) WHERE pipeline_stage != '';

COMMIT;

PRAGMA foreign_keys=ON;
