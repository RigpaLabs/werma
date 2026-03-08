# Werma ཝེར་མ

Agent identity, memory & orchestration for RigpaLabs pipeline.

**Werma** (ཝེར་མ) — warrior spirits from the Bön tradition. They protect, execute, and maintain order.

## Agents

| Agent | Role | Personality |
|-------|------|-------------|
| **Watchdog** | Infrastructure guardian | Silent sentinel, alerts only on problems |
| **Analyst** | Technical research & specs | Methodical researcher, thorough documenter |
| **Engineer** | Implementation | Pragmatic builder, clean code advocate |
| **Reviewer** | Code review | Sharp-eyed critic, fair but demanding |
| **QA** | Quality assurance | Meticulous tester, no shortcuts |
| **DevOps** | Deploy & monitoring | Calm operator, safety-first |

## Architecture

### Two-Layer Orchestration

```
Layer 1: heartbeat.sh (bash, */1 min, 0 tokens)
├── Unstick stuck agents
├── Enforce resource limits
├── Restart failed tasks
├── Queue drain check
└── Stale detection

Layer 2: werma.md (Opus, */15 min)
├── Pattern recognition across agents
├── Slack pulse reporting
├── Strategic task proposals
└── Cross-agent coordination
```

### Agent Files

Each agent has two files:
- `character.md` — personality scaffold, communication style, decision-making traits
- `memory.md` — persistent learnings, accumulated knowledge, patterns discovered

### Shared State

- `shared/signals.md` — inter-agent communication (status, handoffs, flags)
- `identity.json` — external identity (how Werma presents on Slack/Linear/GitHub)
- `limits.json` — resource constraints per agent

## Setup

```bash
./install.sh
```

Creates symlinks and registers launchd agents for heartbeat scheduling.

## Related

- [RigpaLabs/forge](https://github.com/RigpaLabs/forge) — Pipeline engine that invokes these agents
