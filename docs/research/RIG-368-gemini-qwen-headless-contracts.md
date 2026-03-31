# RIG-368: Gemini CLI + Qwen Code Headless Contracts

**Date:** 2026-04-01
**Status:** Verified on macOS (Apple Silicon)

---

## Summary

Both CLIs work in headless mode and share a nearly identical flag contract — Qwen Code is a fork of Gemini CLI. Integration into werma `AgentRuntime` is straightforward, following the existing Codex pattern.

---

## Gemini CLI

| Property | Value |
|----------|-------|
| Binary | `gemini` |
| Version tested | 0.35.3 |
| Install | `brew install gemini-cli` or `npm i -g @anthropic-ai/gemini-cli` |
| Auth | Google OAuth (cached in `~/.gemini/`) |
| Config dir | `~/.gemini/` |
| Default model | `gemini-3-flash-preview` (as of 2026-04-01) |

### Headless invocation

```bash
gemini -p "prompt text here"
```

- Stdout: plain text response (default)
- Stderr: "Loaded cached credentials." on first use
- Exit code: 0 on success

### Flags for werma integration

| Flag | Purpose | Example |
|------|---------|---------|
| `-p <prompt>` | Non-interactive (headless) mode | `-p "say hello"` |
| `-y` / `--yolo` | Auto-approve all tool calls | `gemini -y -p "..."` |
| `--approval-mode <mode>` | Fine-grained: `default`, `auto_edit`, `yolo`, `plan` | `--approval-mode yolo` |
| `-m <model>` | Model selection | `-m gemini-2.5-flash` |
| `-o <format>` | Output format: `text`, `json`, `stream-json` | `-o json` |
| `--raw-output` | Disable output sanitization | |
| `--include-directories` | Additional workspace dirs | |

### JSON output format (`-o json`)

```json
{
  "session_id": "uuid",
  "response": "agent text output",
  "stats": {
    "models": {
      "gemini-3-flash-preview": {
        "api": { "totalRequests": 1, "totalErrors": 0, "totalLatencyMs": 8524 },
        "tokens": { "input": 7830, "prompt": 7830, "candidates": 3, "total": 7867, "cached": 0, "thoughts": 34, "tool": 0 }
      }
    }
  }
}
```

Key differences from Claude Code JSON:
- `response` field (not `result`)
- Token stats nested under `stats.models.<model_name>.tokens`
- No `total_cost_usd` (free tier, no billing)
- No `num_turns` at top level
- No `subtype` field (no `error_max_turns` equivalent detected)
- No `is_error` field

### Stream JSON format (`-o stream-json`)

NDJSON with typed messages:
```jsonl
{"type":"init","timestamp":"...","session_id":"uuid","model":"gemini-3-flash-preview"}
{"type":"message","timestamp":"...","role":"user","content":"..."}
{"type":"message","timestamp":"...","role":"assistant","content":"...","delta":true}
{"type":"result","timestamp":"...","status":"success","stats":{...}}
```

### Rate limits (free tier)

- Google AI Studio free tier: varies by model
- 429 errors observed for `gemini-2.5-flash` during testing ("No capacity available")
- Retry with backoff recommended, same pattern as Claude rate-limit handling

### Privacy

- Google Cloud terms apply
- Gemini API may use prompts for model improvement unless opted out via Google AI Studio settings
- No explicit "we don't train on your code" claim like Qwen

---

## Qwen Code

| Property | Value |
|----------|-------|
| Binary | `qwen` (NOT `qwen-code`) |
| Version tested | 0.13.0 |
| Install | `brew install qwen-code` |
| Auth | Qwen OAuth (`qwen auth qwen-oauth`) or Alibaba Cloud Coding Plan |
| Config dir | `~/.qwen-code/` (created on first settings write) |
| Default model | `coder-model` (Qwen internal alias) |

### Headless invocation

```bash
qwen -p "prompt text here"
```

- Stdout: plain text response (default)
- Stderr: silent (no "loaded credentials" noise)
- Exit code: 0 on success

**Note:** `-p` is deprecated in Qwen — they prefer positional prompt. But `-p` still works and is the reliable headless trigger.

### Flags for werma integration

| Flag | Purpose | Example |
|------|---------|---------|
| `-p <prompt>` | Headless mode (deprecated but works) | `-p "say hello"` |
| Positional prompt | Preferred headless mode | `qwen "say hello"` |
| `-y` / `--yolo` | Auto-approve all tool calls | `qwen -y -p "..."` |
| `--approval-mode <mode>` | Fine-grained: `plan`, `default`, `auto-edit`, `yolo` | `--approval-mode yolo` |
| `-m <model>` | Model selection | `-m qwen3-coder` |
| `-o <format>` | Output format: `text`, `json`, `stream-json` | `-o json` |
| `--system-prompt` | Override system prompt | |
| `--append-system-prompt` | Append to system prompt | |
| `--max-session-turns` | Limit turns | `--max-session-turns 20` |
| `--include-directories` | Additional workspace dirs | |
| `--input-format` | stdin format: `text`, `stream-json` | |

### JSON output format (`-o json`)

Array of NDJSON objects (NOT a single JSON object like Gemini):
```json
[
  {"type":"system","subtype":"init","session_id":"uuid","cwd":"...","tools":[...],"model":"coder-model","qwen_code_version":"0.13.0"},
  {"type":"assistant","message":{"content":[{"type":"thinking","thinking":"..."}]}},
  {"type":"assistant","message":{"content":[{"type":"text","text":"actual response"}],"usage":{"input_tokens":12706,"output_tokens":44}}},
  {"type":"result","subtype":"success","is_error":false,"duration_ms":8629,"result":"actual response","usage":{...},"stats":{...}}
]
```

Key fields in the `result` object:
- `result`: plain text response (same semantics as Claude's `.result`)
- `is_error`: boolean
- `duration_ms`, `duration_api_ms`: timing
- `usage.input_tokens`, `usage.output_tokens`: token counts
- `stats.models.<model>.tokens`: detailed breakdown
- `permission_denials`: array of denied tool calls

**Critical:** Qwen's `-o json` returns an **array**, not a single object. Parse with `jq '.[-1].result'` (last element is the result).

### Stream JSON format (`-o stream-json`)

Same content as JSON but as NDJSON (one object per line). Each line is parseable independently.

### Auth flow

Two methods:
1. **Qwen OAuth** (`qwen auth qwen-oauth`): Opens browser, OAuth flow, stores token locally
2. **Alibaba Cloud Coding Plan** (`qwen auth coding-plan`): Enterprise auth via Alibaba Cloud

Check status: `qwen auth status`

### Rate limits (free tier)

- Auth status reports: **1,000 requests/day** (not 2,000 as some docs claim)
- Models available: "Qwen latest models" (aliased as `coder-model`)

### Privacy

- Qwen claims CLI does not train on user code
- Telemetry is opt-in (disabled by default, configurable in `settings.json`)
- No data stored server-side beyond API call processing

---

## Comparison Matrix

| Feature | Claude Code | Codex | Gemini CLI | Qwen Code |
|---------|------------|-------|------------|-----------|
| Binary | `claude` | `codex` | `gemini` | `qwen` |
| Headless flag | `-p` | `exec` subcommand | `-p` | `-p` (deprecated) / positional |
| Auto-approve | `--dangerously-skip-permissions` | `--full-auto` | `-y` / `--approval-mode yolo` | `-y` / `--approval-mode yolo` |
| Model flag | `--model` | `--model` | `-m` | `-m` |
| JSON output | `--output-format json` | `-o <file>` (file, not stdout) | `-o json` (stdout) | `-o json` (stdout) |
| Output file | N/A (stdout) | `-o <path>` | N/A (stdout) | N/A (stdout) |
| Max turns | `--max-turns` | N/A | N/A | `--max-session-turns` |
| Allowed tools | `--allowedTools` | N/A | `--allowed-tools` (deprecated) | `--allowed-tools` |
| System prompt override | N/A | N/A | N/A | `--system-prompt` |
| Sandbox modes | N/A | `--sandbox read-only\|full-auto` | `-s` (boolean) | `-s` (boolean) |
| Result field (JSON) | `.result` | file content | `.response` | `[-1].result` |
| Cost tracking | `.total_cost_usd` | N/A | N/A | N/A |
| Session ID | `.session_id` | N/A | `.session_id` | `.session_id` |
| Rate limit signal | exit 429 + stderr | N/A | exit + 429 JSON error | TBD |
| MCP support | Built-in | N/A | `gemini mcp` | `qwen mcp` |
| Free tier | No | No | Yes (Google AI Studio) | Yes (1000 req/day) |

---

## Werma Integration Recommendations

### 1. AgentRuntime enum extension

```rust
pub enum AgentRuntime {
    #[default]
    ClaudeCode,
    Codex,
    Gemini,  // new
    Qwen,    // new
}
```

### 2. Exec script pattern

Both Gemini and Qwen can share a single script generator (they're forks):

```bash
# Gemini
gemini -y -m "$MODEL" -o json -p "$PROMPT"

# Qwen
qwen -y -m "$MODEL" -o json -p "$PROMPT"
```

Key differences to handle per-runtime:
- **Binary name:** `gemini` vs `qwen`
- **JSON parsing:** Gemini returns single object (`.response`), Qwen returns array (`[-1].result`)
- **Sandbox:** Neither has Codex-style `--sandbox read-only|full-auto`. Use `--approval-mode` instead:
  - `plan` → read-only (research/review tasks)
  - `yolo` → full auto (code/full tasks)
- **Max turns:** Only Qwen supports `--max-session-turns`
- **Cost tracking:** Neither provides cost data — `werma complete` without `--cost`
- **Output:** Both write to stdout (not file like Codex) — capture and write to result file

### 3. Approval mode mapping

| werma task_type | Gemini/Qwen approval_mode |
|-----------------|--------------------------|
| research | `plan` |
| review/analyze | `plan` |
| code | `auto_edit` (Gemini) / `auto-edit` (Qwen) |
| full | `yolo` |

**Note:** Gemini uses `auto_edit` (underscore), Qwen uses `auto-edit` (hyphen).

### 4. Model mapping

| werma model | Gemini | Qwen |
|-------------|--------|------|
| (default) | `gemini-3-flash-preview` | `coder-model` |
| explicit | pass through | pass through |
| Claude shorthands (opus/sonnet) | empty (use default) | empty (use default) |

### 5. Result extraction (bash)

```bash
# Gemini
RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.response // empty')

# Qwen
RESULT_TEXT=$(echo "$RESULT_JSON" | jq -r '.[-1].result // empty')
```

### 6. Limitations

- **No cost tracking** — neither CLI reports token costs in USD
- **No `--allowedTools` equivalent** — Gemini has it (deprecated), Qwen has it. But tool restriction is weaker than Claude Code's granular control
- **No worktree awareness** — neither has `--skip-git-repo-check` like Codex. May need testing in `.trees/` worktrees
- **Rate limits on free tier** — both CLIs will hit 429s under heavy wave execution. Need backoff + fallback strategy
- **Session continuity** — Qwen supports `--continue`/`--resume`, Gemini supports `--resume`. Could enable multi-turn task continuation

---

## Open Questions for Implementation

1. **Worktree compatibility:** Do Gemini/Qwen respect `.trees/` working directories correctly? (Need to test with actual file edits)
2. **CLAUDE.md equivalent:** Gemini uses `GEMINI.md`, Qwen uses `QWEN.md` — pipeline prompts need to be written to the correct file per runtime
3. **Tool availability:** Do both CLIs provide equivalent file editing tools (Read, Edit, Write, Bash)?
4. **Verdict parsing:** Will agents on these runtimes reliably output `VERDICT=DONE` / `PR_URL=...` as last line?
5. **Error recovery:** What does a failed Gemini/Qwen run look like? Need to map exit codes and error JSON shapes for `werma fail` handling
