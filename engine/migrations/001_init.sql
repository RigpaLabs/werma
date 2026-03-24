PRAGMA journal_mode=WAL;
PRAGMA busy_timeout=5000;

CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK(status IN ('pending','running','completed','failed')),
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
    linear_issue_id TEXT DEFAULT '',
    linear_pushed   INTEGER DEFAULT 0,
    pipeline_stage  TEXT DEFAULT '',
    depends_on      TEXT DEFAULT '[]',
    context_files   TEXT DEFAULT '[]'
);

CREATE INDEX IF NOT EXISTS idx_tasks_status_priority ON tasks(status, priority);
CREATE INDEX IF NOT EXISTS idx_tasks_linear ON tasks(linear_issue_id) WHERE linear_issue_id != '';
CREATE INDEX IF NOT EXISTS idx_tasks_pipeline ON tasks(pipeline_stage) WHERE pipeline_stage != '';

CREATE TABLE IF NOT EXISTS schedules (
    id              TEXT PRIMARY KEY,
    cron_expr       TEXT NOT NULL,
    prompt          TEXT NOT NULL,
    type            TEXT NOT NULL DEFAULT 'research',
    model           TEXT NOT NULL DEFAULT 'opus',
    output_path     TEXT DEFAULT '',
    working_dir     TEXT NOT NULL,
    max_turns       INTEGER DEFAULT 0,
    enabled         INTEGER DEFAULT 1,
    context_files   TEXT DEFAULT '[]',
    last_enqueued   TEXT DEFAULT ''
);

CREATE TABLE IF NOT EXISTS pr_reviewed (
    pr_key          TEXT PRIMARY KEY,
    updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS daily_usage (
    date            TEXT PRIMARY KEY,
    opus_calls      INTEGER DEFAULT 0,
    sonnet_calls    INTEGER DEFAULT 0,
    haiku_calls     INTEGER DEFAULT 0
);
