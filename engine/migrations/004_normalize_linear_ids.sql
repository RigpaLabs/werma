-- 004: Normalize linear_issue_id from UUIDs to identifiers (RIG-XX).
-- Old pipeline tasks stored Linear UUIDs; new code uses identifiers.
-- Clear UUIDs from completed/failed tasks to prevent dedup false negatives.
-- Active tasks (pending/running) with UUIDs are also cleared since they'll
-- be re-created with proper identifiers on next pipeline poll.
--
-- RIG-310: The original query only preserved 'RIG-%' identifiers, which
-- nuked FAT-XX (and any future team) identifiers on every DB open.
-- Fix: preserve any value matching the TEAM-NUMBER pattern (uppercase
-- letters followed by a dash and digits).
-- RIG-388: Also preserve GitHub Issues identifiers (repo#N format).
-- Previously only [A-Z]*-[0-9]* was preserved (Linear TEAM-N).
-- Now also preserves *#[0-9]* (e.g. honeyjourney#20).
UPDATE tasks
SET linear_issue_id = ''
WHERE linear_issue_id <> ''
  AND linear_issue_id NOT GLOB '[A-Z]*-[0-9]*'
  AND linear_issue_id NOT GLOB '*#[0-9]*';
