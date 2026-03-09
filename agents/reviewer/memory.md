# Reviewer — Memory

## Common Issues Found

_Track recurring review findings to inform engineer training._

## Project-Specific Standards

- **sui-bots:** CI/CD chore PRs don't need version bump. Deploy pattern: CI gate via `workflow_call` → build → deploy (matches fathom, hyper-liq)
- **sui-bots:** GHCR images under `ghcr.io/rigpalabs/` (migrated from `arleyar`). SSH secrets: `VULTR_TOKYO_*`
- **GitHub:** Can't `gh pr review --approve` on own PRs — use `gh pr comment` instead
- **fathom:** CI standardized to single `check` job (fmt → clippy → test → audit) matching hyper-liq/sui-bots. Deploy secrets: `VULTR_TOKYO_*` (migrated from `VPS_*` in RIG-25)
- **All repos:** CI chore PRs (no app logic change) don't require version bump

## False Positives

_Issues flagged that turned out to be fine — calibrate review sensitivity._
