# Changelog

All notable changes to the werma engine are documented here.

## [0.2.0] — 2026-03-10

### Added
- `build.rs`: embed git version at compile time via `WERMA_GIT_VERSION` env var
- `version` command: output format `werma 0.2.0 (git-hash)` with runtime repo hash and DB path
- Pipeline automation: `werma pipeline poll/status/approve` for Linear issue lifecycle
- Reviewer task creation on engineer stage completion (RIG-60)
- Scheduling system: `werma sched add/ls/on/off` with cron expressions via launchd
- Notification improvements for pipeline stage transitions
- Fail-fast guard when `LINEAR_API_KEY` is missing (RIG-47)

### Changed
- Version display consolidated: binary hash shown inline on first line
- `build.rs` now always emits `WERMA_GIT_VERSION` (falls back to `"unknown"` on git failure)

## [0.1.0] — 2026-01-01

### Added
- Initial Rust rewrite of the werma orchestration CLI
- Task queue management: `werma add/run/status/list`
- Daemon with tick-based polling loop: `werma daemon install/uninstall`
- Linear integration: `werma linear sync` pulls issues into local queue
- SQLite-backed runtime state at `~/.werma/werma.db`
- Agent tmux session management for isolated task execution
- `werma migrate` for importing tasks from the legacy `aq` system
- `werma dash` status dashboard
