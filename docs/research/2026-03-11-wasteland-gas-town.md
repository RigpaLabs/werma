# Wasteland & Gas Town — Research Report

> Date: 2026-03-11 | Sources: 12 | Search queries: 5

## TL;DR

- **Wasteland** — федеративная сеть Gas Town'ов (AI agent workspace'ов) со shared Wanted Board, reputation stamps, и Dolt-based data federation. Launched March 2026 by Steve Yegge
- **Gas Town** — multi-agent orchestrator для Claude Code (Go, 189k LOC, 11.5k stars), конкурент/аналог werma. Координирует 5-30 параллельных агентов через Git-backed state (Beads)
- **Reputation stamps** — multi-dimensional оценки (quality, reliability, creativity) за выполненную работу. Portable, auditable, append-only. По сути верифицированное резюме
- **Dolt** — SQL database с Git semantics (branch, merge, PR на данных). Пока не интегрирован в Gas Town, но запланирован как основа federation
- **Для werma/RigpaLabs:** прямая интеграция маловероятна (разные стеки, разные модели), но идеи reputation system и federated task board применимы. Wasteland пока pre-product — больше vision, чем рабочий протокол

## Findings

### 1. Что такое Wasteland

Wasteland — это следующий шаг от Gas Town. Если Gas Town — это локальный multi-agent workspace (один человек, много агентов), то Wasteland — сеть Gas Town'ов, где участники публикуют задачи и берут чужие в работу.

**Ключевые концепции:**
- **Wanted Board** — общая доска задач. Кто угодно публикует идею/задачу, другие берут её своими агентами
- **Rigs** — участники с AI агентами. Rig = человек + его Gas Town setup
- **Posters** — публикуют работу на Wanted Board
- **Validators** — проверяют выполненную работу и ставят stamps
- **Trust Ladder** — registered → contributor → maintainer (растёт с количеством stamps)

**Workflow** заимствует Git модель: claim task → fork → execute → submit PR → validator reviews → stamp → reputation grows.

[Welcome to the Wasteland: A Thousand Gas Towns](https://steve-yegge.medium.com/welcome-to-the-wasteland-a-thousand-gas-towns-a5eb9bc8dc1f)

### 2. Gas Town — Architecture

Gas Town — Go-based orchestrator (~189k LOC, 6040 commits) для координации множества Claude Code инстансов.

**Компоненты:**
| Role | Function |
|------|----------|
| **Mayor** | Primary AI coordinator, planning |
| **Polecats** | Ephemeral worker agents |
| **Refinery** | Merge queue management |
| **Witness** | Health monitoring |
| **Deacon** | Patrol loops |
| **Dogs** | Maintenance tasks |
| **Crew** | Persistent collaborative agents |

**State management:** Git-backed "Beads" — issue tracking system, одновременно data plane и control plane. Всё состояние агентов, задачи, зависимости — в Git.

**Key concept — Nondeterministic Idempotence:** работа выражается как "molecules" — цепочки мелких задач (Beads) с чёткими acceptance criteria. Если агент падает — следующий подхватывает с того же места.

**Зависимости:** Git 2.25+, Dolt 1.82.4+, Beads (bd) 0.55.4+, SQLite3, tmux 3.0+, Claude Code CLI.

**Стоимость:** ~$100/hour при 12-30 параллельных агентах ([Cloud Native Now](https://cloudnativenow.com/features/gas-town-what-kubernetes-for-ai-coding-agents-actually-looks-like/)).

[GitHub — steveyegge/gastown](https://github.com/steveyegge/gastown)

### 3. Система репутации (Stamps)

Центральная идея Wasteland — переносимая, верифицированная репутация:

- **Stamps** — многомерные оценки за выполненную работу (quality, reliability, creativity)
- **Passbook** — портфолио stamps, привязанных к конкретным tasks. Append-only, auditable
- **Portable** — репутацию можно переносить между Wasteland'ами (команда, компания, университет)
- **Key difference vs LinkedIn:** репутация = что другие написали о твоей работе, не что ты сам заявляешь

**Механизм:**
1. Poster создаёт task на Wanted Board
2. Rig (contributor) берёт task, выполняет через своих агентов
3. Submits PR-style completion
4. Validator reviews → stamps the passbook
5. Stamps accumulate → trust level grows

**Критика (HN):** по сути это "freelance board с AI агентами". Вопросы о том, кто платит, как масштабируется, и не становится ли это luxury product для нердов. ([HN Discussion](https://news.ycombinator.com/item?id=47250133))

### 4. Dolt — SQL + Git semantics

**Dolt** — SQL database (MySQL-совместимая) с полноценным Git: branch, merge, diff, clone, push, pull на уровне таблиц и ячеек.

**Что даёт для federation:**
- Cell-based merge conflicts (точнее, чем line-based в Git)
- Version control queries — "кто изменил эту ячейку?"
- Clone/push/pull для structured data между инстансами
- SQL query performance + Git audit trail

**Текущий статус в Gas Town:** Dolt указан как зависимость (1.82.4+), но полная интеграция пока в процессе. Текущая архитектура использует SQLite + JSONL + Git. Dolt должен заменить этот dual-system approach.

**DoltHub** — GitHub для данных: public/private repos, PRs на данных, hosted Dolt.

[Dolt — Git for Data](https://github.com/dolthub/dolt) | [DoltHub Blog](https://www.dolthub.com/blog/2026-01-15-a-day-in-gas-town/)

### 5. Сравнение с Werma

| Aspect | Gas Town | Werma |
|--------|----------|-------|
| **Language** | Go (~189k LOC) | Rust (~6.5k LOC) |
| **Agent target** | Claude Code (+ Codex, Cursor, Gemini) | Claude Code |
| **State** | Beads (Git-backed) | SQLite WAL |
| **Task source** | Internal (Mayor, Wanted Board) | Linear issues |
| **Pipeline** | Implicit (Mayor decides) | YAML-driven, explicit stages |
| **Federation** | Wasteland (planned/early) | None |
| **Reputation** | Stamps (planned) | None |
| **Worktrees** | Yes (Hooks) | Yes (.trees/) |
| **Cost model** | ~$100/hr at scale | Pay per agent session |
| **Maturity** | 6k+ commits, 11.5k stars | ~200 commits, private |
| **Philosophy** | "Kubernetes for agents" — Mayor orchestrates | "Linear-driven pipeline" — YAML defines flow |

**Key architectural differences:**
- Gas Town = autonomous orchestration (Mayor decides what to do)
- Werma = pipeline orchestration (Linear → analyst → engineer → reviewer → done)
- Gas Town is 30x larger codebase, targeting enterprise-scale agent swarms
- Werma is lean, opinionated, integrated with Linear project management

### 6. Что можно взять для Werma / RigpaLabs

#### Directly applicable ideas

1. **Reputation/quality tracking для агентов.** Werma уже трекает task completion, но нет quality dimension. Можно добавить:
   - Reviewer verdict tracking (approve/reject/rework) — уже частично есть
   - Agent performance metrics (first-pass success rate, rework count)
   - Quality scores per agent type/model
   - Store in SQLite, expose via `werma stats`

2. **Portable work evidence.** Каждый completed task в werma = потенциальный "stamp". Linear issue + PR + review verdict — это уже auditable work record. Можно генерить structured reports:
   - "Agent X completed 47 tasks this month, 89% first-pass approval"
   - Export как portfolio для демонстрации возможностей RigpaLabs pipeline

3. **Nondeterministic Idempotence.** Gas Town's подход к crash recovery (molecules с acceptance criteria) — хорошая идея. Werma сейчас при crash просто mark failed. Можно:
   - Добавить checkpoint system (agent saves progress mid-task)
   - Allow `werma continue <id>` to resume from last checkpoint
   - Store intermediate outputs in task record

#### Worth monitoring (not yet actionable)

4. **Dolt for data versioning.** Если RigpaLabs начнёт шарить research data (backtest results, signal parameters) между проектами — Dolt > обычный PostgreSQL. Пока overkill для текущего масштаба.

5. **Federated task board.** Когда RigpaLabs вырастет за пределы одного человека — shared task board с reputation stamps может заменить Linear для cross-team coordination. Пока преждевременно.

6. **Multi-runtime support.** Gas Town поддерживает Codex, Cursor, Gemini через agent presets. Werma привязан к Claude Code. Если понадобится — можно добавить runner abstraction.

#### Not applicable

7. **Gas Town как замена werma.** Разные философии: Gas Town = autonomous Mayor, werma = Linear-driven pipeline. Gas Town требует $100+/hr при масштабе, 189k LOC codebase. Werma lean и достаточен для текущих нужд.

8. **Wasteland participation.** Текущий Wasteland — pre-product vision. Нет стабильного API, нет federation protocol spec. Участие = использование Gas Town полностью, что означает отказ от werma. Не оправдано.

## Recommendations

1. **Track agent quality metrics** — добавить в werma статистику по reviewer verdicts, first-pass success rate. Это low-effort (данные уже есть в SQLite) и даёт аналог stamps для внутреннего use
2. **Monitor Wasteland development** — если появится federation protocol spec, оценить возможность werma adapter. Пока слишком рано
3. **Consider Dolt** — когда/если RigpaLabs data sharing станет bottleneck (research data, signal parameters between projects). Не сейчас
4. **Steal the "molecules" pattern** — crash recovery через checkpoints + acceptance criteria. Хорошая идея для resilience werma pipeline tasks

## Sources

| # | Title | URL | Accessed | Trust |
|---|-------|-----|----------|-------|
| 1 | Welcome to the Wasteland (Steve Yegge, Medium) | https://steve-yegge.medium.com/welcome-to-the-wasteland-a-thousand-gas-towns-a5eb9bc8dc1f | 2026-03-11 | HIGH |
| 2 | GitHub — steveyegge/gastown | https://github.com/steveyegge/gastown | 2026-03-11 | HIGH |
| 3 | Gas Town Docs | https://docs.gastownhall.ai/ | 2026-03-11 | HIGH |
| 4 | HN Discussion — Wasteland | https://news.ycombinator.com/item?id=47250133 | 2026-03-11 | MEDIUM |
| 5 | A Day in Gas Town (DoltHub Blog) | https://www.dolthub.com/blog/2026-01-15-a-day-in-gas-town/ | 2026-03-11 | HIGH |
| 6 | Gas Town: What K8s for AI Agents Looks Like | https://cloudnativenow.com/features/gas-town-what-kubernetes-for-ai-coding-agents-actually-looks-like/ | 2026-03-11 | MEDIUM |
| 7 | Dolt — Git for Data (GitHub) | https://github.com/dolthub/dolt | 2026-03-11 | HIGH |
| 8 | Gas Town Hall | https://gastownhall.ai/ | 2026-03-11 | HIGH |
| 9 | Dev Interrupted Podcast — Agent Wasteland | https://linearb.io/dev-interrupted/podcast/agent-wasteland-openclaw-perplexity-computer-dev-interrupted | 2026-03-11 | MEDIUM |
| 10 | Steve Yegge on AI Agents (Pragmatic Engineer) | https://newsletter.pragmaticengineer.com/p/steve-yegge-on-ai-agents-and-the | 2026-03-11 | HIGH |
| 11 | Gas Town Work Management Docs | https://docs.gastownhall.ai/usage/work-management/ | 2026-03-11 | HIGH |
| 12 | Wasteland — gastownhall.ai | https://wasteland.gastownhall.ai/ | 2026-03-11 | LOW (empty page) |

## Methodology

- **Search angles:** product overview + Steve Yegge blog, architecture/stamps/Dolt, Gas Town CLI/setup, Dolt database specifics, HN community discussion
- **Tools:** WebSearch (5 queries) + WebFetch (8 URLs)
- **Known gaps:** Medium article returned 403 (paywall), Wasteland site itself was empty (likely JS-rendered SPA), no federation protocol spec found (may not exist yet), no API documentation for Wasteland integration
