# Best CLI Tools & Patterns — Вдохновение для Werma

> Date: 2026-03-10 | Sources: 18 | Search queries: 8

## TL;DR

- **Werma уже делает многое правильно** — worktree isolation, pipeline-as-config, Linear integration, tmux orchestration. Это уровень agent-orchestrator/parallel-code
- **Главные gaps:** отсутствие цвета/progress indicators, примитивный dashboard (println), нет `--json` output, нет interactive mode, слабая error UX
- **Quick wins:** добавить `console` + `indicatif` для цветного output + спиннеров, `--json` flag глобально, улучшить error messages с actionable suggestions
- **Medium term:** ratatui-based live dashboard (`werma dash --live`), plugin architecture для agents/runtimes, `werma doctor` для self-diagnostics
- **Вдохновение:** Agent Orchestrator (plugin slots), clig.dev (human-first design), ripgrep (sensible defaults + speed)

---

## 1. Ландшафт: Rust CLI Ecosystem 2026

### Ключевые крейты

| Crate | Purpose | Stars | Relevance для werma |
|-------|---------|-------|---------------------|
| **clap** 4.5 | Argument parsing (derive macros) | 14k+ | Уже используем ✓ |
| **ratatui** | TUI framework (immediate-mode) | 18.9k | Dashboard upgrade |
| **indicatif** | Progress bars, spinners, multi-progress | 4k+ | Task execution feedback |
| **console** | Colors, styles, terminal detection | 2k+ | Colored output everywhere |
| **dialoguer** | Interactive prompts, confirmations | 2k+ | `werma add` interactive mode |
| **tabled** | Pretty tables | 1k+ | `werma list` / `werma status` |
| **anyhow** + **thiserror** | Error handling | 13k+ | Уже используем ✓ |
| **iocraft** | React-like TUI (новый) | ~500 | Experimental alternative to ratatui |

Sources: [Rust CLI Patterns (dasroot.net)](https://dasroot.net/posts/2026/02/rust-cli-patterns-clap-cargo-configuration/), [Ratatui](https://ratatui.rs/), [indicatif](https://github.com/console-rs/indicatif)

### Эталонные Rust CLI проекты

| Tool | What to learn |
|------|---------------|
| **ripgrep** | Blazing defaults, smart .gitignore, colored output, `--json` mode |
| **bat** | Syntax highlighting, git integration, automatic paging, cat-compatible |
| **starship** | Config-driven (TOML), module system, cross-shell |
| **eza** | Git status integration, icons, tree view |
| **fd** | Opinionated sensible defaults > flexibility |
| **zoxide** | Learning from usage patterns (frecency algorithm) |
| **bottom** | ratatui dashboard, real-time system monitoring |
| **kdash** | Kubernetes dashboard on ratatui — closest to what `werma dash` should be |

Sources: [Curated Rust CLI list](https://gist.github.com/sts10/daadbc2f403bdffad1b6d33aff016c0a), [15 Rust CLI tools](https://dev.to/dev_tips/15-rust-cli-tools-that-will-make-you-abandon-bash-scripts-forever-4mgi)

---

## 2. Agent Orchestration: Конкуренты и Вдохновение

### Agent Orchestrator (ComposioHQ) — главный конкурент

**Архитектура:** Plugin-based с 8 swappable slots:
- **Runtime**: tmux (default), docker, k8s, process
- **Agent**: Claude Code, Codex, Aider
- **Workspace**: worktree, clone
- **Tracker**: GitHub, Linear
- **Notifier**: desktop, Slack, webhook
- **Terminal**: iTerm2, web

**Ключевые паттерны:**
1. **Autonomous CI/CD loop** — при фейле CI агент автоматически получает логи и фиксит (с retry limit)
2. **YAML-driven reactions** — `ci-failed: {auto: true, action: send-to-agent, retries: 2}`
3. **Human decision points** — нотификации только когда нужно human judgment
4. **Plugin interfaces** — каждый slot заменяем без переписки orchestration logic

**Что взять:** Plugin architecture для agents/runtimes. Сейчас werma hardcoded на Claude Code + tmux. Если завтра появится лучший agent — придётся переписывать runner.

Source: [Agent Orchestrator](https://github.com/ComposioHQ/agent-orchestrator)

### Parallel Code — GUI альтернатива

- Electron + SolidJS GUI с тiled agent panels
- QR code для mobile monitoring (!)
- Keyboard-first design: Ctrl+N (task), Ctrl+Shift+M (merge)
- Persistent state across restarts
- Supports Claude Code + Codex + Gemini одновременно

**Что взять:** Remote monitoring idea (webhook/API endpoint для мобильного мониторинга), persistent layout state.

Source: [Parallel Code](https://github.com/johannesjo/parallel-code)

### ccswarm — Academic approach

- Channel-based orchestration, zero shared state
- Specialized agents (Frontend, Backend, DevOps, QA) — похоже на наш pipeline
- Actor model для task delegation
- **НО:** Mostly unfinished, coordination loop not working

**Что взять:** Type-state pattern для task lifecycle (compile-time guarantees на transitions).

Source: [ccswarm](https://github.com/nwiizo/ccswarm)

### Werma vs конкуренты

| Feature | Werma | Agent Orchestrator | Parallel Code | ccswarm |
|---------|-------|--------------------|---------------|---------|
| Language | Rust | TypeScript | TypeScript/Electron | Rust |
| Runtime | tmux | tmux/docker/k8s | electron terminals | PTY |
| Pipeline config | YAML ✓ | YAML ✓ | — | Templates |
| Linear integration | ✓ Deep | — (GitHub) | — | — |
| Worktree isolation | ✓ | ✓ | ✓ | ✓ |
| Auto CI retry | — | ✓ | — | — |
| TUI dashboard | Stub | ✓ | GUI | — |
| Colored output | — | ✓ | ✓ | — |
| Multi-agent support | Claude only | Claude/Codex/Aider | Claude/Codex/Gemini | Claude |
| Cron scheduling | ✓ | — | — | — |
| Self-update | ✓ | — | ✓ | — |

**Werma's advantages:** Native Rust performance, deep Linear integration, cron scheduling, pipeline-as-config с stages/transitions. **Gaps:** UI polish, multi-agent support, CI reaction loop.

---

## 3. CLI UX Principles (clig.dev + Evil Martians + Lucas Costa)

### Human-First Design

> "If a command is going to be used primarily by humans, it should be designed for humans first."

**Принципы:**
1. **Respond within 100ms** — если операция долгая, сразу показать спиннер
2. **Show state changes explicitly** — "Created task #42", не молча вернуть 0
3. **Progressive disclosure** — basic help по `-h`, full docs по `--help`
4. **Suggest on error** — typo correction (Levenshtein distance), "did you mean?"

Source: [Command Line Interface Guidelines](https://clig.dev/)

### Output Modes (dual-mode output)

```
# Human mode (default, when TTY)
werma status
╔═══════════════════════════════╗
║ 3 pending  2 running  5 done ║
╚═══════════════════════════════╝

# Machine mode (piping or --json)
werma status --json
{"pending": 3, "running": 2, "completed": 5, "failed": 0}
```

**Правило:** stdout для data, stderr для messaging. Detect TTY → colored/formatted, pipe → plain.

Source: [clig.dev](https://clig.dev/), [Rust CLI book](https://rust-cli.github.io/book/tutorial/output.html)

### Progress Indicators (Evil Martians)

3 паттерна:
1. **Spinner** — для unknown duration (`werma run` — ждём агента)
2. **X of Y** — для countable steps (`werma run-all` — 3/7 tasks launched)
3. **Progress bar** — для parallel tasks с measurable progress

**Рекомендация:** X of Y для `run-all`, spinner для single `run`, multi-progress для pipeline polling.

Source: [Evil Martians CLI UX](https://evilmartians.com/chronicles/cli-ux-best-practices-3-patterns-for-improving-progress-displays)

### Error Messages

**Bad:**
```
Error: query returned no rows
```

**Good:**
```
Error: Task #42 not found

  The task may have been archived. Try:
    werma list --all     # show archived tasks
    werma list failed    # show failed tasks
```

Source: [UX Patterns for CLI Tools](https://www.lucasfcosta.com/blog/ux-patterns-cli-tools)

---

## 4. Конкретные Рекомендации для Werma

### Tier 1: Quick Wins (1-3 story points each)

#### 1.1 Colored Output (`console` crate)
```rust
use console::{style, Emoji};

println!("{} Task {} created", Emoji("✨", "+"), style("#42").cyan().bold());
println!("{} Running in {}", Emoji("🚀", ">"), style("werma-42").green());
println!("{} Failed: {}", Emoji("❌", "X"), style("timeout after 30m").red());
```
- Detect `NO_COLOR` env var и pipe mode автоматически
- Цветовая схема: green=success, yellow=pending, red=failed, cyan=info

#### 1.2 `--json` Global Flag
```rust
#[derive(Parser)]
pub struct Cli {
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}
```
- Все команды поддерживают `--json` для scripting/piping
- `werma status --json | jq .running`

#### 1.3 Spinners для Long Operations (`indicatif`)
```rust
let pb = ProgressBar::new_spinner();
pb.set_style(ProgressStyle::default_spinner()
    .template("{spinner:.green} {msg}")
    .tick_strings(&["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]));
pb.set_message("Polling Linear for new issues...");
pb.enable_steady_tick(Duration::from_millis(80));
// ... work ...
pb.finish_with_message("Found 3 new issues");
```

#### 1.4 Better Error Messages
Обернуть anyhow errors в human-friendly suggestions:
```rust
// Instead of raw DB errors
fn task_not_found(id: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "Task '{}' not found\n\n  Try:\n    werma list        # show active tasks\n    werma list --all  # include archived",
        id
    )
}
```

#### 1.5 Table Output (`tabled` или `comfy-table`)
```
werma list
┌────┬──────────┬─────────┬────────────┬─────────────────────────────────┐
│ ID │ Status   │ Type    │ Linear     │ Prompt                          │
├────┼──────────┼─────────┼────────────┼─────────────────────────────────┤
│ 42 │ 🟢 done  │ code    │ RIG-97     │ simplify pipeline to 3 stages   │
│ 43 │ 🔵 run   │ review  │ RIG-98     │ code review PR #37              │
│ 44 │ ⚪ pend  │ code    │ RIG-99     │ add colored output              │
└────┴──────────┴─────────┴────────────┴─────────────────────────────────┘
```

### Tier 2: Medium Term (5-8 story points each)

#### 2.1 Live TUI Dashboard (`ratatui`)

`werma dash --live` → полноценный ratatui TUI:

```
╔══════════════════════════════════════════════════════════════╗
║  WERMA DASHBOARD                              ◉ daemon: up  ║
╠══════════════════════════════════════════════════════════════╣
║                                                              ║
║  Tasks:  3 pending │ 2 running │ 47 done │ 1 failed         ║
║                                                              ║
║  ┌─ Running ────────────────────────────────────────────┐    ║
║  │ #43 [RIG-98] review  │ 12m ago │ werma-43 (tmux)    │    ║
║  │ #44 [RIG-99] code    │  3m ago │ werma-44 (tmux)    │    ║
║  └──────────────────────────────────────────────────────┘    ║
║                                                              ║
║  ┌─ Pipeline ───────────────────────────────────────────┐    ║
║  │ RIG-100  analyst → engineer → review ← (current)     │    ║
║  │ RIG-101  analyst ← (current)                         │    ║
║  └──────────────────────────────────────────────────────┘    ║
║                                                              ║
║  ┌─ Schedules (3/5 enabled) ────────────────────────────┐    ║
║  │ morning-review  │ 07:30 daily │ next: 2h 15m         │    ║
║  │ data-refresh    │ */6h        │ next: 4h 30m         │    ║
║  └──────────────────────────────────────────────────────┘    ║
║                                                              ║
║  [q]uit  [r]efresh  [k]ill task  [v]iew logs  [enter] tmux  ║
╚══════════════════════════════════════════════════════════════╝
```

Архитектура:
- Main thread: ratatui render loop (16fps)
- Background: mpsc channel для DB polling (каждые 5s)
- Keyboard: crossterm event handling
- Actions: kill task, attach to tmux, view logs — всё из TUI

Референсы: [statui](https://github.com/Mohamed-Badry/statui), [bottom](https://github.com/ClementTsang/bottom), [kdash](https://github.com/kdash-rs/kdash)

#### 2.2 Interactive Task Creation
```
$ werma add -i
? Prompt: > implement rate limiting for pipeline polling
? Type: (use arrows)
  ❯ code
    research
    review
    full
? Model: opus
? Linear issue: RIG-100 (auto-detected from branch)
? Working dir: ~/projects/rigpa/werma [Enter to confirm]
? Priority: 2

✨ Task #45 created [RIG-100, code, opus]
   Run now? (Y/n)
```
Используем `dialoguer` / `inquire` crate.

#### 2.3 `werma doctor` — Self-Diagnostics
```
$ werma doctor
Checking werma health...

  ✓ SQLite database          ~/.werma/werma.db (2.1 MB)
  ✓ Daemon                   running (pid 1234, uptime 3d)
  ✓ tmux                     installed (3.4)
  ✓ claude                   installed (1.0.x)
  ✓ Linear API key           configured
  ✓ GitHub CLI               authenticated (arleyar)
  ✗ Disk space               ~/.werma/logs/ 890 MB (consider: werma clean --logs)
  ⚠ Stale tasks              2 tasks running >24h (#41, #43)

  6/8 checks passed. 1 warning, 1 action needed.
```

#### 2.4 Webhook/API for Remote Monitoring
HTTP endpoint (tiny axum server) для:
- `GET /status` — JSON task summary
- `GET /tasks` — list active tasks
- `POST /tasks/:id/kill` — kill task remotely
- WebSocket для live updates

Это позволит мониторить с мобильника через простой web UI или Telegram webhook.

### Tier 3: Long Term (13+ story points)

#### 3.1 Plugin Architecture

По образцу Agent Orchestrator — заменяемые slot interfaces:

```rust
trait AgentRuntime: Send + Sync {
    async fn spawn(&self, task: &Task, prompt: &str) -> Result<SessionHandle>;
    async fn status(&self, handle: &SessionHandle) -> RuntimeStatus;
    async fn kill(&self, handle: &SessionHandle) -> Result<()>;
}

trait IssueTracker: Send + Sync {
    async fn poll_issues(&self, config: &StageConfig) -> Result<Vec<Issue>>;
    async fn update_status(&self, issue_id: &str, status: &str) -> Result<()>;
    async fn post_comment(&self, issue_id: &str, body: &str) -> Result<()>;
}
```

Implementations: `TmuxRuntime`, `DockerRuntime`, `ProcessRuntime`, `LinearTracker`, `GitHubTracker`.

#### 3.2 CI Reaction Loop
```yaml
# pipeline config extension
reactions:
  ci-failed:
    auto: true
    action: send-to-agent
    retries: 2
    prompt: "CI failed. Logs: {ci_logs}. Fix the issues."
  changes-requested:
    auto: true
    action: send-to-agent
    escalate_after: 30m
  approved-and-green:
    auto: false
    action: notify
```

#### 3.3 Task Templates
```bash
werma add --template bugfix --var issue=RIG-100 --var file=src/runner.rs
# Templates stored in ~/.werma/templates/ or engine/templates/
```

---

## 5. Архитектурные Паттерны для Заимствования

### Pattern 1: Dual-Mode Output (ripgrep)
Каждая команда выводит human-readable по умолчанию, `--json` для machines. Detect `isatty(stdout)` для авто-переключения.

### Pattern 2: Configuration Hierarchy (clig.dev)
```
CLI flags > env vars > project config (.werma.toml) > user config (~/.werma/config.toml) > defaults
```
Werma сейчас использует только env vars + CLI flags. Добавить `.werma.toml` per-project config.

### Pattern 3: Sensible Defaults (fd)
Команда без аргументов должна делать что-то полезное:
- `werma` (no args) → `werma status` (не help)
- `werma run` → запустить следующую pending задачу (уже есть ✓)
- `werma list` → только активные (не все 500 архивных)

### Pattern 4: Crash-Only Design (clig.dev)
Минимальный cleanup при Ctrl+C. Idempotent operations. `werma run` можно прервать и перезапустить без corruption.

### Pattern 5: Frecency (zoxide)
Запоминать часто используемые working directories, models, task types — и предлагать как defaults. `werma add "fix bug"` → автоматически подставить last used dir, type=code, model=opus.

### Pattern 6: Sub-millisecond First Paint (starship)
Первый вывод < 100ms. Lazy load тяжёлые операции (SSH, Linear API) — показать кешированные данные сразу, обновить асинхронно.

---

## 6. Приоритизированный Roadmap

| Priority | Item | SP | Impact |
|----------|------|----|--------|
| **P0** | Colored output (console crate) | 2 | Весь CLI выглядит профессиональнее |
| **P0** | `--json` global flag | 2 | Scriptability, automation |
| **P0** | Better error messages | 3 | Снижение confusion при ошибках |
| **P1** | Table output (tabled/comfy-table) | 2 | `list` и `status` читаемее |
| **P1** | Spinners (indicatif) | 2 | Feedback для long operations |
| **P1** | `werma doctor` | 3 | Self-diagnostics, onboarding |
| **P2** | Interactive mode (`-i` flag) | 5 | Discoverability для новых пользователей |
| **P2** | Live TUI dashboard (ratatui) | 8 | Killer feature, real-time monitoring |
| **P2** | Project-level config (.werma.toml) | 3 | Per-repo настройки |
| **P3** | Plugin architecture | 13 | Multi-agent, multi-runtime |
| **P3** | CI reaction loop | 8 | Autonomous CI fixing |
| **P3** | Webhook/API endpoint | 5 | Remote monitoring |
| **P3** | Task templates | 3 | Repeatability |

**Recommended first sprint:** P0 items (colored output + --json + error messages) = 7 SP total. Максимальный visual impact за минимальные усилия.

---

## Sources

| # | Title | URL | Trust |
|---|-------|-----|-------|
| 1 | Command Line Interface Guidelines | https://clig.dev/ | HIGH |
| 2 | CLI UX: 3 Patterns for Progress Displays (Evil Martians) | https://evilmartians.com/chronicles/cli-ux-best-practices-3-patterns-for-improving-progress-displays | HIGH |
| 3 | UX Patterns for CLI Tools (Lucas Costa) | https://www.lucasfcosta.com/blog/ux-patterns-cli-tools | HIGH |
| 4 | Rust CLI Patterns (dasroot.net) | https://dasroot.net/posts/2026/02/rust-cli-patterns-clap-cargo-configuration/ | MEDIUM |
| 5 | Agent Orchestrator (ComposioHQ) | https://github.com/ComposioHQ/agent-orchestrator | HIGH |
| 6 | Parallel Code | https://github.com/johannesjo/parallel-code | MEDIUM |
| 7 | ccswarm | https://github.com/nwiizo/ccswarm | MEDIUM |
| 8 | Ratatui | https://ratatui.rs/ | HIGH |
| 9 | indicatif | https://github.com/console-rs/indicatif | HIGH |
| 10 | Curated Rust CLI utilities | https://gist.github.com/sts10/daadbc2f403bdffad1b6d33aff016c0a | MEDIUM |
| 11 | 15 Rust CLI tools (dev.to) | https://dev.to/dev_tips/15-rust-cli-tools-that-will-make-you-abandon-bash-scripts-forever-4mgi | MEDIUM |
| 12 | Rust CLI book — Output | https://rust-cli.github.io/book/tutorial/output.html | HIGH |
| 13 | awesome-ratatui | https://github.com/ratatui/awesome-ratatui | MEDIUM |
| 14 | statui (ratatui dashboard) | https://github.com/Mohamed-Badry/statui | MEDIUM |
| 15 | Boris Cherny — Claude Code worktree support | https://www.threads.com/@boris_cherny/post/DVAAnexgRUj | HIGH |
| 16 | Shipyard — Multi-agent orchestration | https://shipyard.build/blog/claude-code-multi-agent/ | MEDIUM |
| 17 | CLI Agent Manager | https://mcpmarket.com/tools/skills/cli-agent-manager | MEDIUM |
| 18 | Building CLI Tools with Clap (dasroot.net) | https://dasroot.net/posts/2026/01/building-cli-tools-clap-rust/ | MEDIUM |

## Methodology

- **Search angles:** Rust CLI frameworks, best-designed Rust CLIs, CLI UX principles, agent orchestration tools, ratatui dashboards, progress indicators, competitor analysis
- **Tools:** WebSearch (8 queries) + WebFetch (8 pages) + codebase Read (2 files)
- **Known gaps:** Не нашёл benchmarks werma vs competitors (нет публичных). Iocraft (React-like TUI) — слишком новый, мало примеров. Не исследовал Windows/Linux compatibility patterns (werma = macOS only пока).
