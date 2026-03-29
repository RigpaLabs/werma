# Werma

**AI delivery engine that turns Linear issues into merged PRs through a reliable multi-stage pipeline.**

AI can write code. Werma makes sure it ships.

![werma st](docs/images/werma-st.svg)

## What It Does

- **Full delivery pipeline** — Linear issue &rarr; analyst &rarr; engineer &rarr; reviewer &rarr; QA &rarr; deploy &rarr; done. Each stage is an AI agent running in tmux with appropriate permissions (read-only for review, edit for code, full shell for deploy).
- **Transactional outbox** — External API calls (Linear, GitHub, Slack) go through a durable outbox with exponential retry and dead-letter queue. No more lost state transitions.
- **Single binary + SQLite** — No external services, no Docker, no Kubernetes. One Rust binary, one database file. Install and run in 30 seconds.

## Install

### GitHub Releases (recommended)

Download the latest binary from [Releases](https://github.com/RigpaLabs/werma/releases).

### Build from source

```bash
cargo install --git https://github.com/RigpaLabs/werma werma
```

Or clone and build:

```bash
git clone https://github.com/RigpaLabs/werma
cd werma/engine
cargo build --release
# Binary at target/release/werma
```

### Requirements

- Rust 1.88+ (build from source)
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI (agent runtime)
- tmux (agent sessions)
- macOS or Linux

## Quick Start

```bash
# Add a task to the queue
werma add "Research best practices for error handling in async Rust" -t research

# Run the next pending task (launches a Claude Code agent in tmux)
werma run

# Run all pending tasks in parallel
werma run-all

# Check status of all tasks
werma st

# Review a pull request with an AI reviewer agent
werma review 42
```

### Pipeline Mode (Linear Integration)

```bash
# Install the daemon (polls Linear, processes completions, drains outbox)
werma daemon install

# Check pipeline status — which issues are at which stage
werma pipeline status

# Approve an issue for deployment
werma pipeline approve RIG-42
```

## Architecture

```
                    ┌─────────────┐
                    │   Linear    │  Issue tracker
                    └──────┬──────┘
                           │ poll (30s)
                    ┌──────▼──────┐
                    │   Daemon    │  Tick loop: poll, callbacks, effects
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
        ┌─────▼─────┐ ┌───▼───┐ ┌─────▼─────┐
        │  Pipeline  │ │ Queue │ │  Effects  │
        │  Callback  │ │       │ │  Outbox   │
        └─────┬──────┘ └───┬───┘ └─────┬─────┘
              │            │            │
              │      ┌─────▼─────┐     │
              └─────►│  Runner   │◄────┘
                     └─────┬─────┘
                           │ spawn
                     ┌─────▼─────┐
                     │   tmux    │  Isolated agent sessions
                     │ sessions  │  (one per task)
                     └───────────┘
```

### Pipeline Stages

Issues flow through a YAML-configured pipeline. Each stage runs a specialized agent:

| Stage | Agent Type | What It Does |
|-------|-----------|--------------|
| **Analyst** | read-only | Writes technical spec, sets labels |
| **Engineer** | edit | Creates branch, writes code, opens PR |
| **Reviewer** | read-only | Reviews PR on GitHub, approves or requests changes |
| **QA** | edit | Verifies CI green, runs tests |
| **DevOps** | full (shell) | Merges PR, triggers deploy, runs health checks |

Each agent emits a **verdict** on its last output line (e.g., `VERDICT=DONE`, `REVIEW_VERDICT=APPROVED`). The pipeline engine parses verdicts and transitions issues to the next stage automatically.

### Agent Roles

| Agent | Role | Personality |
|-------|------|-------------|
| **Watchdog** | Infrastructure guardian | Silent sentinel, alerts only on problems |
| **Analyst** | Technical research & specs | Methodical researcher, thorough documenter |
| **Engineer** | Implementation | Pragmatic builder, clean code advocate |
| **Reviewer** | Code review | Sharp-eyed critic, fair but demanding |
| **QA** | Quality assurance | Meticulous tester, no shortcuts |
| **DevOps** | Deploy & monitoring | Calm operator, safety-first |

### Task Types

| Type | Permissions | Use For |
|------|------------|---------|
| `research` | Read-only | Research, documentation, analysis |
| `analyze` | Read-only | Code review, audits, specs |
| `code` | Edit files | Writing/modifying code |
| `full` | Edit + shell | Deploy, infra, anything needing bash |

## Configuration

Werma stores runtime data in `~/.werma/`:

```
~/.werma/
├── werma.db        # SQLite database (tasks, effects, schedules)
├── config.toml     # User configuration (optional)
├── .env            # Credentials (LINEAR_API_KEY, etc.)
├── logs/           # Per-task agent logs + daemon.log
├── backups/        # Automatic DB backups
└── pipelines/      # Pipeline config overrides (optional)
```

### Environment Variables

Copy `.env.example` to `~/.werma/.env` and fill in your keys:

| Variable | Required | Description |
|----------|----------|-------------|
| `LINEAR_API_KEY` | For pipeline | Linear API key for issue tracking |
| `SLACK_BOT_TOKEN` | No | Slack bot token for notifications |
| `GITHUB_TOKEN` | No | GitHub token for self-update / private repos |

### Custom Pipeline

The default pipeline is compiled into the binary. To customize, copy the built-in config and edit it:

```bash
# View the current pipeline configuration
werma pipeline show

# Place your custom config at ~/.werma/pipelines/default.yaml
# (runtime overrides take precedence over the compiled-in default)

# Validate your changes
werma pipeline validate
```

## Effects Outbox

External side effects (Linear state changes, GitHub PR creation, Slack notifications) are processed through a durable outbox:

```bash
# List pending and failed effects
werma effects

# Show dead-lettered effects (exhausted retries)
werma effects dead

# Retry a failed effect
werma effects retry <id>
```

Effects use exponential backoff (5s &rarr; 30s &rarr; 120s &rarr; 300s &rarr; 600s) and are classified as **blocking** (must succeed for pipeline to continue) or **best-effort** (retried independently).

## Scheduling

Run tasks on a cron schedule via the daemon:

```bash
# Add a daily research task
werma sched add daily-review "0 9 * * 1-5" "Review open PRs and summarize status" --type research

# List schedules
werma sched ls

# Enable/disable without deleting
werma sched on daily-review
werma sched off daily-review
```

## Contributing

See [CLAUDE.md](CLAUDE.md) for agent conventions, pipeline configuration format, and development workflow.

```bash
cd engine
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

## Philosophy

Werma (&#x0F5D;&#x0F7A;&#x0F62;&#x0F58;) means "warrior spirit" in the Bon tradition. They protect, execute, and maintain order. This tool embodies the same principle: reliable execution of well-defined tasks, with clear boundaries and accountability at each stage.

## License

[MIT](LICENSE)
