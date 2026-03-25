-- Migration 009: Add cost tracking and turns used per task (RIG-291)
ALTER TABLE tasks ADD COLUMN cost_usd REAL;
ALTER TABLE tasks ADD COLUMN turns_used INTEGER DEFAULT 0;
