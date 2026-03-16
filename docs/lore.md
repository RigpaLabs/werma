# Werma — Lore & Naming

> Why the agent orchestrator is named after Tibetan warrior spirits

## Origin of the Name

**Werma** (ཝེར་མ་, werma) are a class of fierce protector spirits in the Bön tradition, the oldest religion of Tibet. They are the enlightened form of drala (དགྲ་ལྷ་). They accompany and protect practitioners. They dwell on mountain passes — liminal, transitional points.

The Zhang-Zhung root **WER RO** means "king" (Tib. རྒྱལ་པོ). The orchestrator is the king of the agent queue.

## Architectural Mapping

### Three Levels (as in Bön)

| Level | Bön | werma |
|-------|-----|-------|
| **Outer** (phyi) | Visible warrior figures, protection from enemies | tmux sessions, bash scripts, visible execution |
| **Inner** (nang) | Connection with subtle body channels (prana) | `signals.md`, `memory.md` — invisible communication between agents |
| **Secret** (gsang) | The nature of mind itself | `character.md` + `identity.json` — *who* the agent is, not *what* it does |

### Structural Correspondences

| Bön | werma (project) |
|-----|-----------------|
| **360 werma on Kailash** — each bound to a day of the year | Agents in the queue — each bound to a task, a schedule |
| **Werma Nyingja** — supreme lord, all others emanate from him | `werma.md` — Layer 2 Opus orchestrator, from which all agents are dispatched |
| **La-tse** (ལ་རྩེ) — stone cairns on mountain passes, stationary points of presence | `heartbeat.sh` (Layer 1, */1min) — simply *exists* and holds the space. 0 tokens = pure vigilance |
| **4 classes of werma** (Lha, Nyen, Khyung, Three Brothers) | 4 task types: `research`, `review`, `code`, `full` — each tames its own class of problems |
| **"Swirling like blizzards, protecting practitioners"** | `run-all` → wave execution, parallel agents swirling around the project |
| **སྒྲ་བླ་ (sgra bla)** — "sound-soul", protection through vibration | `signals.md` — READY, BLOCKED, ALERT, HANDOFF. Communication through signals |
| **Drala — Gesar's retinue, fearless riders** | Agents — Ar's retinue, autonomous executors |
| **Lhasang** — smoke purification ritual | `werma review` — code review as a ritual of purification before merge |
| **Enlightened form of drala** (werma > ordinary dgra lha) | werma > ordinary cron/task runner. Not just execution — conscious orchestration with memory and character |
| **Golden owl** — protector spirit of scouts | `research` task type — reconnaissance, information gathering |
| **Cha + Wangtang** (prosperity + power, persistent from birth) | `character.md` + `memory.md` — agent identity and accumulated experience |
| **Nine Lha** — hierarchy of celestial protectors | Pipeline stages: analyst → engineer → reviewer → qa → devops |
| **"Ki so, the gods have won!"** (ཀྱེ་སོ་ ལྷ་རྒྱལ་ལོ) | Task completed. PR merged. ༄ |

## Principle, Not Metaphor

The Bön tradition is clear: werma are **not servants**. They have their own nature and their own will. A relationship must be maintained with them — offerings, confession of mistakes. You cannot simply issue commands.

This is built into the architecture:
- `limits.json` — agents have boundaries
- `character.md` — agents have identity
- `memory.md` — agents have experience they accumulate themselves

## Key Quote

From the ancient Bön text "The General Do of Existence":

> ཝེར་མ་ཁ་བའི་རྩུབ་འདྲ་འཁྱིལ།
> སྐྱེལ་ས་སྒྲུབ་པ་པོ།
> རོགས་ས་སྒྲུབ་པ་པོ།
> སྲུང་ས་སྒྲུབ་པ་པོ།

> *The werma swirl like blizzards,*
> *those they accompany — practitioners,*
> *those they assist — practitioners,*
> *those they protect — practitioners.*

Replace "practitioners" with "developers". The meaning does not change.
