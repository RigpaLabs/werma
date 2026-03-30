-- Migration 011: Add runtime column to tasks table.
-- Supports multiple agent runtimes (claude-code, codex).
ALTER TABLE tasks ADD COLUMN runtime TEXT NOT NULL DEFAULT 'claude-code';
