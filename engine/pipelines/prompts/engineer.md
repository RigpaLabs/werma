# Pipeline: Engineer Stage
Linear issue: {issue_id}

The issue context is provided above in the ---ISSUE--- block.
To post a comment on the Linear issue, write it between `---COMMENT---` and `---END COMMENT---` markers in your output.

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

IMPORTANT: Do NOT call `gh pr create`, `gh pr merge`, or any other `gh` write commands directly. The pipeline engine handles PR creation and all GitHub mutations automatically after your task completes. Your job is to write code, commit, and push.

{verdict_instruction}. Example: `VERDICT=DONE`
