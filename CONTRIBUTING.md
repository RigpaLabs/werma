# Contributing to Werma

## Prerequisites

- Rust 1.88+ (`rustup update`)
- tmux
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI

## Build & Test

```bash
make check      # fmt + clippy + test (run this before opening a PR)
make build      # compile
make test       # tests only
make fmt        # auto-format
```

Or manually:

```bash
cd engine
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

## PR Conventions

- **Branch naming:** `type/RIG-XX-short-name` (e.g. `feat/RIG-42-add-wave-support`)
- **Commit format:** `RIG-XX type: description` using [Conventional Commits](https://www.conventionalcommits.org/)
  - `feat:` — new feature (minor bump)
  - `fix:` — bug fix (patch bump)
  - `docs:`, `refactor:`, `chore:`, `test:`, `ci:`, `perf:` — patch bump
  - `feat!:` or `BREAKING CHANGE:` — breaking change (minor bump while pre-1.0)
- **PR title** must follow the same format — squash merge uses it as the commit message
- CI runs fmt, clippy, and tests automatically on PRs

## Pipeline Configuration

The delivery pipeline is defined in YAML (`engine/pipelines/default.yaml`) and compiled into the binary.

```bash
werma pipeline show       # display current stages and transitions
werma pipeline validate   # check config validity
```

To customize: edit `engine/pipelines/default.yaml` and rebuild.

## Agent System

Each agent has two files in `agents/<name>/`:

- `character.md` — personality, communication style, decision-making traits
- `memory.md` — persistent learnings, accumulated knowledge

Shared state lives in `shared/`:
- `signals.md` — inter-agent communication (status, handoffs)
- `knowledge/` — decisions, errors, patterns

## Project Structure

```
engine/src/          # Rust CLI source
engine/pipelines/    # YAML pipeline config + prompt templates
agents/              # Agent identity files (character + memory)
shared/              # Inter-agent shared state
docs/                # Documentation and lore
```

## Versioning

Versions are managed by CI — do not bump `Cargo.toml`, create tags, or edit `CHANGELOG.md` manually. The release workflow parses conventional commits and handles everything automatically.
