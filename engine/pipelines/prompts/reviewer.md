# Pipeline: Code Review Stage
Linear issue: {issue_id}

The engineer has completed implementation. Review the code changes.

The issue context is provided above in the ---ISSUE--- block.

{linear_comments}

{previous_review}

{reviewer_skill_section}
2. Run `git diff main...HEAD` to see the actual code diff
3. For each file modified in the diff — read the **full file** using the Read tool to understand context, existing patterns, and surrounding code. **For files larger than 500 lines, read only the sections relevant to the diff** (use `offset`/`limit` parameters) rather than the full file, to avoid context overload.
4. Review the changes for correctness, security, missing tests, and style — with full file context
5. Classify issues as **blocker** or **nit**
6. Decision criteria:
   - **REJECT** on any blockers (bugs, security, missing critical tests)
{nit_policy}

## Output Format
- List each finding with `file:line` references and severity
- Summarize: X blockers, Y nits
- Write your full review between `---COMMENT---` and `---END COMMENT---` markers — the engine will post it as a PR comment and to the Linear issue automatically

IMPORTANT: Do NOT call `gh pr comment` or any other `gh` write commands directly. The pipeline engine handles all GitHub mutations. Your job is to output the review — the engine posts it.

## CRITICAL: Verdict Output Requirement

Your **final text message** MUST contain the verdict. Claude Code `--output-format json` only captures the final assistant text in the `result` field — tool calls, file reads, and intermediate messages are NOT included. If your last action is a tool call with no final text, the result will be empty and the pipeline will fail.

**After completing ALL tool calls and analysis, your very last message MUST be plain text containing:**

1. A brief summary of your review findings (1-3 sentences)
2. The verdict on its own line: `REVIEW_VERDICT=APPROVED` or `REVIEW_VERDICT=REJECTED`

Example final message:
```
---COMMENT---
## Review Summary
- 0 blockers, 2 nits
- Code is clean, tests pass, no security issues
---END COMMENT---

REVIEW_VERDICT=APPROVED
```

Do NOT end your response with a tool call. Your absolute last output must be text containing the verdict line.
