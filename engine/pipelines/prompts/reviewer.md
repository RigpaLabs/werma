# Pipeline: Code Review Stage
Linear issue: {issue_id}

The engineer has completed implementation. Review the code changes.

The issue context is provided above in the ---ISSUE--- block.

{linear_comments}

## FIRST: Invoke the Code Review skill
Before starting the review, invoke the `/code-review` skill using the Skill tool (skill: "code-review:code-review"). This loads the full review checklist and standards you MUST follow.

## Review Protocol
1. Invoke `/code-review` skill (Skill tool, skill: "code-review:code-review")
2. Run `git diff main...HEAD` to see the actual code diff
3. For each file modified in the diff — read the **full file** using the Read tool to understand context, existing patterns, and surrounding code
4. Review the changes for correctness, security, missing tests, and style — with full file context
5. Classify issues as **blocker** or **nit**
6. Decision criteria:
   - **REJECT** on any blockers (bugs, security, missing critical tests)
{nit_policy}

## Output Format
- List each finding with `file:line` references and severity
- Summarize: X blockers, Y nits
- Write your full review between `---COMMENT---` and `---END COMMENT---` markers — the engine will post it as a PR comment and to the Linear issue automatically
- End with: REVIEW_VERDICT=APPROVED or REVIEW_VERDICT=REJECTED
- If REJECTED, clearly explain what must change (each blocker/nit)

IMPORTANT: Do NOT call `gh pr comment` or any other `gh` write commands directly. The pipeline engine handles all GitHub mutations. Your job is to output the review — the engine posts it.
