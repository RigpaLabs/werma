# CI/CD Systems: Inspiration for Werma

> Date: 2026-03-10 | Sources: 14 | Search queries: 8

## TL;DR

- **Temporal's workflow-as-code** — лучшая аналогия для werma: durable execution, signals, activities, child workflows. Werma уже делает многое из этого, но может заимствовать Signals (inter-agent communication) и Entity Workflows (issue = long-running workflow)
- **Dagger's модульность** — reusable pipeline modules через Daggerverse. Werma может сделать аналог: pipeline stages как shareable/composable модули, а не monolithic YAML
- **Plan → Execute → Verify (PEV) loop** — паттерн из agentic engineering, который werma уже реализует через pipeline stages. Можно формализовать: каждый stage = PEV цикл с explicit quality gates
- **Observability из CI/CD** — werma не хватает метрик: время выполнения stage, success rate по agent/model, cost per task. OpenTelemetry-style трейсинг pipeline runs
- **Event-driven triggers** — beyond Linear polling: реакция на git events, Slack commands, file changes, schedule + event combo triggers

---

## 1. Architectural Paradigms

### Task-Centric vs Data-Centric

Современные оркестраторы делятся на два лагеря ([State of Workflow Orchestration 2025](https://www.pracdata.io/p/state-of-workflow-orchestration-ecosystem-2025)):

| Paradigm | Systems | Core idea |
|----------|---------|-----------|
| **Task-Centric** | Airflow, Kestra, Luigi | DAG of tasks, scheduler manages control flow |
| **Data-Centric** | Dagster, Temporal, Flyte | Data objects (assets) as primary, native data passing |

**Werma сейчас:** task-centric (Linear issue → stages → verdict → transition). Но с элементами data-centric — `{previous_output}` передаётся между stages.

**Идея:** Формализовать "artifacts" между stages. Каждый stage produces typed output (analysis doc, code diff, test results), следующий stage получает structured input, а не raw text.

### Durable Execution (Temporal)

Temporal — самая близкая аналогия к werma ([Temporal + AI Agents](https://dev.to/akki907/temporal-workflow-orchestration-building-reliable-agentic-ai-systems-3bpm)):

| Temporal concept | Werma equivalent | Gap |
|------------------|-----------------|-----|
| Workflow | Pipeline run (issue lifecycle) | ✅ Есть |
| Activity | Stage execution (agent task) | ✅ Есть |
| Signal | — | ❌ Нет inter-agent signals |
| Child Workflow | — | ❌ Нет sub-pipelines |
| Query | `werma pipeline status` | ✅ Частично |
| Timer | Schedule/cron | ✅ Есть |
| Retry policy | — | ❌ Нет configurable retries |
| Event history | SQLite task log | ⚠️ Минимально |

**9 паттернов Temporal для AI-агентов:**
1. **Sequential** — werma stages (уже есть)
2. **Parallel** — wave execution (уже есть)
3. **Sub-workflows** — child pipelines (нет)
4. **Conditional** — verdict-based transitions (уже есть)
5. **Fan-Out/Fan-In** — distribute across agents, aggregate (нет)
6. **Saga** — compensating actions on failure (нет)
7. **Polling** — Linear polling (уже есть)
8. **Event-Driven** — signals from external (нет)
9. **Chained** — stage output → next input (уже есть)

### Pipeline-as-Code vs Pipeline-as-Config

Dagger доказывает преимущества code over YAML ([Dagger Pipeline-as-Code](https://www.youngju.dev/blog/devops/2026-03-03-dagger-cicd-pipeline-as-code.en)):

| Approach | Pros | Cons |
|----------|------|------|
| **YAML config** (werma сейчас) | Простота, декларативность, легко читать | Нет type safety, limited logic, hard to test |
| **Code** (Dagger) | Type safety, IDE support, composable, testable | Steeper learning curve, more boilerplate |
| **Hybrid** | Best of both | Complexity of two systems |

**Werma's sweet spot:** YAML для stage definitions (простота), но Rust types для validation (уже делается через `PipelineConfig` serde). Можно усилить: добавить `werma pipeline validate` с runtime type checking промптов и transitions.

---

## 2. Модульность и Composability

### Dagger Modules / Daggerverse

Dagger создал экосистему reusable pipeline components ([Dagger Modules](https://docs.dagger.io/features/modules/), [Daggerverse](https://daggerverse.dev/)):

- Modules = collections of Functions, packaged for sharing
- Decentralized registry (git-based, no central server)
- Featured modules earn community recognition
- Cross-language: Go, Python, TypeScript modules interoperable

**Идея для Werma: Pipeline Stage Modules**

```yaml
# Вместо inline prompts — reusable stage modules
stages:
  analyst:
    module: "rigpalabs/werma-stages/analyst@v1"  # git-based
    config:
      spec_format: "linear-description"

  engineer:
    module: "rigpalabs/werma-stages/engineer@v1"
    config:
      language: "rust"
      test_required: true
```

Это позволит:
- Шарить stage configurations между проектами
- Версионировать prompts отдельно от engine
- Community stages (e.g., "security-auditor", "docs-writer")

### Dagster: Software-Defined Assets

Dagster переосмыслил пайплайны: вместо "что делать" (tasks) — "что производить" (assets). Каждый asset — это declarative output с lineage tracking.

**Для Werma:** Каждый pipeline stage produces a named artifact:
- `analyst` → `spec.md`
- `engineer` → `branch + PR`
- `reviewer` → `review-verdict.json`
- `devops` → `deployment-status`

Это даёт трейсабельность: "почему deployment failed?" → trace back через artifacts к исходному spec.

---

## 3. Multi-Agent Orchestration Patterns

### Plan → Execute → Verify (PEV)

Из [Agentic Engineering Guide](https://www.nxcode.io/resources/news/agentic-engineering-complete-guide-vibe-coding-ai-agents-2026):

```
Task → Feature Author → Test Generator → Code Reviewer
→ Architecture Guardian → Security Scanner → Human Review → CI/CD
```

Werma's pipeline уже реализует PEV:
- **Plan:** analyst stage
- **Execute:** engineer stage
- **Verify:** reviewer stage + CI

**Что можно добавить:**
- **Architecture Guardian** — отдельный stage проверяющий structural compliance
- **Security Scanner** — automated SAST/DAST stage
- **Explicit quality gates** между stages с configurable thresholds

### Specialized Agent Roles

Cursor's experiment: "hundreds of specialized agents (planners, workers, judges)" ([AI Coding Agents 2026](https://codeagni.com/blog/ai-coding-agents-2026-the-new-frontier-of-intelligent-development-workflows)).

**Werma уже имеет role-based agents** (analyst, engineer, reviewer, devops). Можно расширить:

| Role | Current | Could add |
|------|---------|-----------|
| Analyst | ✅ | — |
| Engineer | ✅ | Split: architect + coder |
| Reviewer | ✅ | Split: code review + security |
| DevOps | ✅ | — |
| QA | ✅ | E2E test writer |
| Docs Writer | ❌ | Auto-update docs on feature merge |
| Dependency Auditor | ❌ | Check for CVEs, outdated deps |

### Inter-Agent Communication (Temporal Signals)

Temporal's **Signals** — fire-and-forget messages to running workflows. Werma не имеет аналога.

**Сценарии:**
- Engineer blocked → signal to analyst "spec unclear, need clarification"
- Reviewer rejects → signal to engineer with specific feedback (уже через `rejection_feedback`, но одностороннее)
- External event (deploy failed) → signal to devops stage
- Human override → signal to skip/retry stage

**Реализация:** SQLite-based message queue между stages. Уже есть `shared/signals.md` — можно формализовать в DB.

---

## 4. Event System и Triggers

### Beyond Linear Polling

Werma сейчас: poll Linear every 900s. CI/CD системы значительно богаче ([Harness Triggers](https://developer.harness.io/docs/platform/triggers/triggers-overview/)):

| Trigger type | CI/CD examples | Werma potential |
|-------------|---------------|-----------------|
| **Git events** | Push, PR, merge, tag | Watch repos → auto-create tasks |
| **Schedule** | Cron | ✅ Уже есть (`werma sched`) |
| **Webhook** | Generic HTTP POST | API endpoint for external triggers |
| **Artifact** | New release published | Watch GHCR → auto-deploy |
| **Status change** | Linear issue moved | ✅ Уже есть (pipeline poll) |
| **File change** | Config modified | Watch .env/.yaml → validate |
| **Slack** | Message/reaction | Slack command → create task |
| **Composite** | Schedule + condition | "Run if Linear has pending AND it's business hours" |

**Kestra's подход:** real-time HTTP triggers + Kafka/SQS event processing. Werma мог бы добавить webhook endpoint в daemon для real-time triggers вместо polling.

### Airflow 3.0: Data Assets

Airflow 3.0 (April 2025) ввёл **Data Assets** — event-driven scheduling based on data availability, не только time ([State of Workflow Orchestration 2025](https://www.pracdata.io/p/state-of-workflow-orchestration-ecosystem-2025)).

**Для Werma:** "Run engineer stage only when analyst output is ready AND CI is green" — condition-based triggers вместо linear flow.

---

## 5. Observability и DX

### Метрики для Agent Pipelines

Из CI/CD observability best practices ([InfoQ CI/CD Observability](https://www.infoq.com/articles/ci-cd-observability/)):

| Metric | CI/CD equivalent | Werma application |
|--------|-----------------|-------------------|
| Build time | Stage execution time | Time per stage, per agent model |
| Success rate | Pipeline pass rate | Stage verdict distribution (approve/reject/revise) |
| MTTR | Recovery time | Time from rejection to re-approval |
| Deployment frequency | Release cadence | Issues completed per week |
| Cost | Infra spend | Token usage per stage, per model |
| Flakiness | Test flakiness | Stage inconsistency (same input, different verdicts) |

**Werma уже трекает:** task status, duration, model used, daily usage costs. Можно добавить:
- **Pipeline-level metrics:** end-to-end time (issue created → merged)
- **Stage-level:** avg time, success rate, retry count
- **Agent-level:** model performance comparison (sonnet vs opus per stage)
- **Cost efficiency:** $/issue, $/stage, cost trends

### Dashboard

Werma имеет `werma dash`. Можно вдохновиться CI/CD dashboards:

```
┌─────────────────────────────────────────────┐
│ WERMA PIPELINE DASHBOARD                     │
├─────────────────┬───────────────────────────┤
│ Active Issues   │ ██████░░ 6/8 in pipeline  │
│ Today's Cost    │ $4.20 (budget: $10)       │
│ Avg Cycle Time  │ 2.3h (target: <4h)        │
│ Success Rate    │ 78% (last 7 days)         │
├─────────────────┴───────────────────────────┤
│ STAGE HEALTH                                 │
│ analyst   ████████████░ 92% pass  avg 8min  │
│ engineer  ████████░░░░░ 67% pass  avg 45min │
│ reviewer  ██████████░░░ 83% pass  avg 12min │
│ devops    █████████████ 100% pass avg 5min  │
├─────────────────────────────────────────────┤
│ RECENT PIPELINE RUNS                         │
│ RIG-98  ✅ analyst → engineer → review → done│
│ RIG-97  ⚠️ analyst → engineer → REJECTED    │
│ RIG-96  🔄 analyst → engineer (running)     │
└─────────────────────────────────────────────┘
```

### Dagger DX Innovations

Из [Dagger](https://www.youngju.dev/blog/devops/2026-03-03-dagger-cicd-pipeline-as-code.en):

- **Local execution identical to CI** — werma уже имеет это (local tmux = production)
- **Shell access during debugging** — werma может добавить `werma attach <id>` для live agent session
- **Content-based caching** — cache agent outputs, skip stage if input unchanged
- **Secret management** — dedicated secret handling (werma uses .env, good enough)

---

## 6. Конкретные Recommendations для Werma

### Priority 1: Quick Wins (1-3 SP each)

1. **Pipeline metrics в `werma dash`** — добавить success rate, avg time per stage, cost/issue
2. **Stage artifacts** — формализовать output каждого stage как named file, не только `{previous_output}` text
3. **Configurable retries** — `max_retries: 2` per stage в YAML, с exponential backoff
4. **`werma attach <id>`** — tmux attach к running agent session

### Priority 2: Medium Effort (5-8 SP each)

5. **Inter-stage signals** — DB-based message passing (engineer → analyst: "spec unclear")
6. **Webhook trigger** — HTTP endpoint в daemon для external events (GitHub webhook, Slack command)
7. **Conditional transitions** — "proceed only if CI green AND reviewer approved" (multi-condition gates)
8. **Pipeline tracing** — full execution trace per issue (every stage input/output/verdict/timing)

### Priority 3: Ambitious (13+ SP)

9. **Pipeline Stage Modules** — git-based reusable stage definitions (Daggerverse-inspired)
10. **Fan-Out/Fan-In** — distribute review across N agents, aggregate verdicts
11. **Child Pipelines** — sub-pipeline для complex issues (e.g., "refactor module" spawns per-file pipelines)
12. **Saga/Compensation** — rollback actions on failure (revert branch, close PR, update Linear)

---

## Comparison: Werma vs CI/CD Patterns

| Pattern | GitHub Actions | Temporal | Dagger | Werma (current) | Werma (potential) |
|---------|---------------|----------|--------|-----------------|-------------------|
| Pipeline definition | YAML | Code | Code (Go/Py/TS) | YAML | YAML + modules |
| Execution | Cloud containers | Workers | Containers | tmux + Claude | Same |
| State persistence | None (stateless) | Durable (event sourced) | Cache volumes | SQLite | SQLite + traces |
| Triggers | Git events, cron, API | Signals, timers | Any (code) | Linear poll, cron | + webhooks, events |
| Retries | Per-step | Per-activity (configurable) | Per-step | None | Per-stage |
| Parallelism | Matrix strategy | Fan-out/fan-in | Goroutines | Wave execution | + fan-out/fan-in |
| Observability | Actions UI | Temporal UI + history | Dagger Cloud | `werma dash` | + metrics, traces |
| Modularity | Actions Marketplace | — | Daggerverse | Inline YAML | Stage modules |
| Inter-process comm | Artifacts | Signals + Queries | Function calls | `{previous_output}` | Signals + artifacts |
| Human-in-the-loop | Environments + approvals | Signals | — | Manual label | + explicit gates |

---

## Sources

| # | Title | URL | Accessed | Trust |
|---|-------|-----|----------|-------|
| 1 | State of Workflow Orchestration 2025 | https://www.pracdata.io/p/state-of-workflow-orchestration-ecosystem-2025 | 2026-03-10 | HIGH |
| 2 | Temporal + AI Agents: Building Reliable Agentic AI Systems | https://dev.to/akki907/temporal-workflow-orchestration-building-reliable-agentic-ai-systems-3bpm | 2026-03-10 | HIGH |
| 3 | Agentic Engineering: Complete Guide 2026 | https://www.nxcode.io/resources/news/agentic-engineering-complete-guide-vibe-coding-ai-agents-2026 | 2026-03-10 | HIGH |
| 4 | Dagger: CI/CD Pipeline as Code (2026) | https://www.youngju.dev/blog/devops/2026-03-03-dagger-cicd-pipeline-as-code.en | 2026-03-10 | HIGH |
| 5 | AI Coding Agents 2026: New Frontier | https://codeagni.com/blog/ai-coding-agents-2026-the-new-frontier-of-intelligent-development-workflows | 2026-03-10 | MEDIUM |
| 6 | CI/CD Observability (InfoQ) | https://www.infoq.com/articles/ci-cd-observability/ | 2026-03-10 | HIGH |
| 7 | Dagger Reusable Modules | https://docs.dagger.io/features/modules/ | 2026-03-10 | HIGH |
| 8 | Daggerverse | https://daggerverse.dev/ | 2026-03-10 | HIGH |
| 9 | Orchestration Showdown: Airflow vs Dagster vs Temporal | https://medium.com/datumlabs/orchestration-showdown-airflow-vs-dagster-vs-temporal-in-the-age-of-llms-758a76876df0 | 2026-03-10 | MEDIUM |
| 10 | Temporal Child Workflows | https://docs.temporal.io/child-workflows | 2026-03-10 | HIGH |
| 11 | Tekton vs Argo Comparison | https://www.wallarm.com/cloud-native-products-101/cloud-native-ci-cd-pipelines-tekton-vs-argo | 2026-03-10 | MEDIUM |
| 12 | Harness Triggers Overview | https://developer.harness.io/docs/platform/triggers/triggers-overview/ | 2026-03-10 | HIGH |
| 13 | Prefect ControlFlow for LLM Workflows | https://www.prefect.io/compare/airflow | 2026-03-10 | MEDIUM |
| 14 | Dagger Module Catalog & Insights | https://dagger.io/blog/module-catalog-insights | 2026-03-10 | HIGH |

## Methodology

- **Search angles:** CI/CD architecture (Tekton/Argo/Dagger), DAG orchestration (Temporal/Prefect/Airflow), AI agent orchestration, pipeline-as-code vs config, CI/CD observability/DX, event-driven triggers
- **Tools:** WebSearch (8 queries) + WebFetch (6 deep dives)
- **Known gaps:** Не удалось получить Datadog blog (ECONNREFUSED) и Medium Datumlabs (403). Нет данных по Argo Events architecture details. Prefect ControlFlow для LLM workflows упоминается, но детали не найдены.
