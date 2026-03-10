# Pipeline: Code Review Stage
Linear issue: {issue_id}

The engineer has completed implementation. Review the code changes.

## Review Protocol
1. Run `gh pr view` to find the open PR (if none, skip step 6)
2. Run `git diff main...HEAD` to see the actual code diff
3. Review the DIFF for correctness, security, missing tests, and style
4. Classify issues as **blocker** or **nit**
5. APPROVE with nits, REJECT only on blockers
6. **Post review as PR comment:** find the PR number first, then post:
```
PR_NUM=$(gh pr view --json number -q .number 2>/dev/null)
gh pr comment "$PR_NUM" --body "<your review markdown>"
```
Include all findings, verdict, and summary in the comment.

## Output Format
- List each finding with `file:line` references
- End with: REVIEW_VERDICT=APPROVED or REVIEW_VERDICT=REJECTED
- If REJECTED, clearly explain what must change
