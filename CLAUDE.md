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

## Conventions

- All communication in English (technical context)
- Structured markdown for all outputs
- No emojis in technical documents (identity.json excepted)
- Reference files by relative path from repo root
