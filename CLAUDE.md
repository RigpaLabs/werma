# Werma — Agent Instructions

## What This Repo Is

Agent identity, memory & orchestration for RigpaLabs pipeline. Each agent has:
- `character.md` — personality, communication style, decision-making traits
- `memory.md` — persistent learnings, accumulated patterns

## Rules for Agents Working Here

1. **Read your character.md** before starting any task
2. **Update your memory.md** after completing tasks — record what you learned
3. **Check shared/signals.md** for active signals before starting
4. **Post signals** when completing, blocking, or failing
5. **Respect limits.json** — model tier, max turns, timeout
6. **`manual` label = human execution, agent review.** Agents must NOT pick up manual issues for execution stages (analyst, engineer, devops). But review and QA stages SHOULD run on manual issues — agents review human code just like agent code. Configured via `manual: skip | process` per stage in `engine/pipelines/default.yaml`

## Pipeline Configuration

Pipeline stages, transitions, and prompts are defined in YAML (`engine/pipelines/default.yaml`), compiled into the binary via `include_str!`. Runtime overrides go to `~/.werma/pipelines/`.

```bash
werma pipeline show              # display current pipeline stages/transitions
werma pipeline validate          # validate YAML config
werma pipeline eject             # export builtin config to ~/.werma/pipelines/ for editing
```

**Config format** — see `engine/pipelines/default.yaml` for full example. Key fields per stage:
- `linear_status` — Linear status(es) to poll (absent = spawned-only stage)
- `agent` / `model` — agent type and model
- `manual: skip | process` — how to handle `manual`-labeled issues
- `prompt` — inline (multiline string) or file path relative to `pipelines/`
- `transitions` — verdict → `{status, spawn?}` mapping

**Prompt template variables:** `{issue_id}`, `{issue_title}`, `{issue_description}`, `{previous_output}`, `{rejection_feedback}`, `{working_dir}`, plus custom `templates:` from config

## Engine

The werma CLI (`engine/`) is a Rust binary that manages the agent queue, scheduling, Linear integration, and pipeline automation. Key commands:

- `werma add/run/status/list` — task queue management
- `werma daemon` — heartbeat + scheduler (replaces heartbeat.sh)
- `werma sched` — cron-based scheduling
- `werma linear` — Linear issue integration
- `werma pipeline show/validate/eject` — YAML-driven CI/CD pipeline management
- `werma dash` — status dashboard
- `werma migrate` — import from old aq system

Runtime data: `~/.werma/` (SQLite database, logs, backups)

## Memory Update Protocol

After each task, append to your `memory.md`:
```markdown
## [DATE] — [TASK_ID]: [brief description]
- **Learned:** [what new pattern/knowledge was gained]
- **Changed:** [any calibration to approach]
```

Keep memory concise. Remove outdated entries. Memory should be useful for future tasks, not a diary.

## Signal Protocol

Post to `shared/signals.md`:
```
[YYYY-MM-DD HH:MM] [AGENT_NAME] [SIGNAL_TYPE] description
```

## File Ownership

| Path | Owner | Others |
|------|-------|--------|
| `agents/X/character.md` | Human (Ar) | Read-only |
| `agents/X/memory.md` | Agent X | Read-only for others |
| `shared/signals.md` | All agents | Append-only |
| `identity.json` | Human | Read-only |
| `limits.json` | Human | Read-only |

## Git Worktrees

Write tasks (code, full, refactor, pipeline-engineer, pipeline-devops) run in isolated git worktrees under `.trees/` in the working directory. Read-only tasks run directly. Worktrees are NOT auto-cleaned — failed tasks keep their worktree for inspection, completed tasks keep theirs for PR review.

## Agent Permission Tiers

| Tier | Operations |
|------|-----------|
| **Always OK** | Read files, run tests, commit to feature branch, create branches, create PRs with `ai-generated` label |
| **Ask first** | Add dependencies, modify CI/CD, architectural changes |
| **Never** | Force push, delete branches, commit secrets, push to main, merge PRs |

## Versioning (CI-driven, DO NOT do manually)

**Conventional Commits** — CI handles version bumps, CHANGELOG, tags, and GitHub Releases automatically.

```
RIG-XX feat: description   → minor bump (0.x.0)
RIG-XX fix: description    → patch bump (0.0.x)
RIG-XX docs:, refactor:, chore:, test:, ci:  → patch bump
RIG-XX feat!: or BREAKING CHANGE: → minor bump (pre-1.0)
```

**PR titles** must use the `RIG-XX type: description` format (squash merge uses PR title):
- `RIG-XX feat: description` or `RIG-XX fix: description`

**DO NOT:**
- Bump version in `Cargo.toml` — CI does it
- Update `CHANGELOG.md` — CI generates it
- Create git tags — CI creates them
- Create GitHub Releases — CI creates them

**Flow:** merge PR → `release.yml` parses commits → bumps version → creates tag `vX.Y.Z` → `build.yml` builds binary → GitHub Release with binary and changelog

**Update binary:** `werma update` (or `cargo build --release` from `engine/`)

## Conventions

- All communication in English (technical context)
- Structured markdown for all outputs
- No emojis in technical documents (identity.json excepted)
- Reference files by relative path from repo root
- `.trees/` directories are gitignored — do not commit worktree contents
