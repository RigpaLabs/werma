-- 004: Normalize linear_issue_id from UUIDs to identifiers (RIG-XX).
-- Old pipeline tasks stored Linear UUIDs; new code uses identifiers.
-- Clear UUIDs from completed/failed tasks to prevent dedup false negatives.
-- Active tasks (pending/running) with UUIDs are also cleared since they'll
-- be re-created with proper identifiers on next pipeline poll.
UPDATE tasks
SET linear_issue_id = ''
WHERE linear_issue_id <> ''
  AND linear_issue_id NOT LIKE 'RIG-%';
