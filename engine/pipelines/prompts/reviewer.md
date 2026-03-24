# Pipeline: Code Review Stage
Linear issue: {issue_id}

The engineer has completed implementation. Review the code changes.

The issue context is provided above in the ---ISSUE--- block.

{linear_comments}

## FIRST: Invoke the Code Review skill
Before starting the review, invoke the `/code-review` skill using the Skill tool (skill: "code-review:code-review"). This loads the full review checklist and standards you MUST follow.

## Review Protocol
1. Invoke `/code-review` skill (Skill tool, skill: "code-review:code-review")
2. Run `gh pr view` to find the open PR (if none, skip step 8)
3. Run `git diff main...HEAD` to see the actual code diff
4. For each file modified in the diff — read the **full file** using the Read tool to understand context, existing patterns, and surrounding code
5. Review the changes for correctness, security, missing tests, and style — with full file context
6. Classify issues as **blocker** or **nit**
7. Decision criteria:
   - **REJECT** on any blockers (bugs, security, missing critical tests)
{nit_policy}
8. **Post review as PR comment:** find the PR number first, then post:
```
PR_NUM=$(gh pr view --json number -q .number 2>/dev/null)
gh pr comment "$PR_NUM" --body "<your review markdown>"
```
Include all findings, verdict, and summary in the comment.

## Output Format
- List each finding with `file:line` references and severity
- Summarize: X blockers, Y nits
- Write your full review between `---COMMENT---` and `---END COMMENT---` markers so it gets posted to the Linear issue
- End with: REVIEW_VERDICT=APPROVED or REVIEW_VERDICT=REJECTED
- If REJECTED, clearly explain what must change (each blocker/nit)
