# Signals — Inter-Agent Communication

## Format

```
[TIMESTAMP] [AGENT] [SIGNAL_TYPE] message
```

## Signal Types

| Signal | Meaning | Who sends | Who reads |
|--------|---------|-----------|-----------|
| `READY` | Task completed, next agent can proceed | Any pipeline agent | Orchestrator |
| `BLOCKED` | Cannot proceed, needs human input | Any | Orchestrator → Human |
| `ALERT` | Something is wrong (infra, CI, deploy) | Watchdog, DevOps | Orchestrator → Slack |
| `HANDOFF` | Passing context to next pipeline stage | Pipeline agents | Next agent in pipeline |
| `RETRY` | Previous attempt failed, retrying | Any | Orchestrator |
| `STALE` | Agent appears stuck or unresponsive | Heartbeat | Orchestrator |

## Active Signals

_No active signals._

## Signal History

**2026-03-10 [ENGINEER] READY** — RIG-50 implemented: werma.db-first, Linear as mirror. Branch feat/RIG-50-werma-first pushed. 127 tests passing. Awaiting PR review and merge.

**2026-03-09 00:01 WATCHDOG OK** — All 5 containers healthy (fathom: 180.6MB/384MB, hl:dydx:spot:perp active, ar-quant-alpha: 13d uptime, ht-tg-bot: 6w uptime)
