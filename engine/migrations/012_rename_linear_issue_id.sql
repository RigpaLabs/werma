-- Migration 012: Rename linear_issue_id to issue_identifier.
-- The column now stores both Linear identifiers (RIG-123, FAT-42) and
-- GitHub Issues identifiers (honeyjourney#20), making the old name misleading.
-- SQLite 3.26+ automatically updates existing index definitions to reference
-- the renamed column, so no index changes are needed here.
ALTER TABLE tasks RENAME COLUMN linear_issue_id TO issue_identifier;
