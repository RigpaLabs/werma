# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-03-10

### Added
- Rust CLI engine (13 modules, SQLite WAL, 78 tests) [PR #1]
- Pipeline callback system with stage routing [PR #2]
- Context sharing: dependency outputs + handoff files [PR #2]
- Manual label support (skip execution, allow review/qa) [PR #2]
- Worktree isolation for write tasks under `.trees/` [PR #4]
- Agent-first patterns: stage-specific prompts, reviewer protocol [PR #5]
- `werma review <target>` command [PR #5]
- Fail-fast when LINEAR_API_KEY missing [PR #7]
- Wire pipeline callbacks + replace sqlite3 CLI [PR #9]

### Changed
- Daemon tick 60s → 5s [PR #3]
- Pipeline working_dir propagated instead of hardcoded [PR #4]

### Fixed
- TOCTOU race (JSON → SQLite BEGIN IMMEDIATE) [PR #1]
- ID collisions (RANDOM → sequential YYYYMMDD-NNN) [PR #1]
- Command injection via tmux prompt [PR #1]
- Empty verdict auto-approve [PR #1]
- GraphQL String! → ID! for Linear API compatibility

## [0.1.0] - 2026-03-09

### Added
- Initial repo structure, agent identities, memory, shared signals
