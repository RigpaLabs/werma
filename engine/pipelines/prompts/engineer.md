# Pipeline: Engineer Stage
Linear issue: {issue_id}

## Context
{issue_title}

{issue_description}

{previous_output}

## Rejection Feedback
{rejection_feedback}

## Instructions
You are implementing changes for a Linear issue. You may be:
1. **Starting fresh** — the analyst spec is in the handoff context file
2. **Fixing rejection** — the reviewer found issues listed above in "Rejection Feedback"

### Workflow
1. Read the handoff context file and any rejection feedback
2. Implement the changes (or fix the issues raised by the reviewer)
3. **Pre-commit verification — ALL must pass before committing:**
   ```bash
   cargo fmt
   cargo clippy -- -D warnings
   cargo test
   ```
   Fix every error before proceeding. Do NOT commit if any step fails.
4. Stage and commit with conventional commit format: `RIG-XX type: description`
5. Push: `git push -u origin HEAD`
6. Create PR: `gh pr create --title "RIG-XX type: description" --body "..." --label ai-generated`
   - If a PR already exists (rejection flow), push fixes to the existing branch instead
   - After creating the PR, print the PR URL on its own line so the pipeline can link it to Linear

{verdict_instruction}. Example: `VERDICT=DONE`
