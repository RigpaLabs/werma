-- Migration 007: callback_attempts column + performance indexes
-- Idempotent: ALTER TABLE ignores "duplicate column" error (handled in Rust).

ALTER TABLE tasks ADD COLUMN callback_attempts INTEGER DEFAULT 0;

-- Indexes for common pipeline queries (tasks_by_linear_issue, has_any_nonfailed_task_for_issue_stage)
CREATE INDEX IF NOT EXISTS idx_tasks_linear_issue ON tasks(linear_issue_id);
CREATE INDEX IF NOT EXISTS idx_tasks_linear_stage ON tasks(linear_issue_id, pipeline_stage, status);
