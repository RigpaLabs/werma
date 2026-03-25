-- Add retry tracking columns to tasks table.
-- retry_count: how many times this task has been auto-retried
-- retry_after: ISO timestamp; task won't be claimed until this time passes
ALTER TABLE tasks ADD COLUMN retry_count INTEGER DEFAULT 0;
ALTER TABLE tasks ADD COLUMN retry_after TEXT;
