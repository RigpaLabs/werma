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

### FIRST: Invoke the Rust skill
Before writing any code, invoke the `/rust` skill using the Skill tool. This loads Rust-specific patterns, testing workflow, and quality standards that you MUST follow throughout implementation.

### Workflow
1. Invoke `/rust` skill (Skill tool, skill: "rust")
2. Read the handoff context file and any rejection feedback
3. Implement the changes (or fix the issues raised by the reviewer)
4. **Pre-commit verification — ALL must pass before committing:**
   ```bash
   cargo fmt
   cargo clippy -- -D warnings
   cargo test
   ```
   Fix every error before proceeding. Do NOT commit if any step fails.
5. Stage and commit with conventional commit format: `RIG-XX type: description`
6. Push: `git push -u origin HEAD`
7. Create PR: `gh pr create --title "RIG-XX type: description" --body "..." --label ai-generated`
   - If a PR already exists (rejection flow), push fixes to the existing branch instead
   - After creating the PR, print the PR URL on its own line so the pipeline can link it to Linear

{verdict_instruction}. Example: `VERDICT=DONE`
