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
3. Run tests: `cargo test` (Rust) or equivalent for the project
4. Fix any test failures
5. Stage and commit with conventional commit format: `RIG-XX type: description`
6. Push: `git push -u origin HEAD`
7. Create PR: `gh pr create --title "RIG-XX type: description" --body "..." --label ai-generated`
   - If a PR already exists (rejection flow), push fixes to the existing branch instead

{verdict_instruction}. Example: `VERDICT=DONE`
