# Pipeline: Analyst Stage
Linear issue: {issue_id}

[{issue_id}] {issue_title}

The issue context is provided above in the ---ISSUE--- block.

{linear_comments}

{sub_issues}

## Instructions
1. Read the issue description carefully from the ---ISSUE--- block
2. Clarify requirements and identify ambiguities
3. **If critical info is missing or requirements are contradictory:** output `VERDICT=BLOCKED` and write a comment block listing specific questions for the human
4. **If the issue is already implemented** (PR **merged** into main AND changes verified): output `VERDICT=ALREADY_DONE`. IMPORTANT: an open/unmerged PR does NOT mean the issue is done — it means work is in progress. Only use ALREADY_DONE when the PR has been merged and the feature is on main
5. Write a detailed implementation spec
6. Include acceptance criteria
7. Identify risks and dependencies

### Epic / Parent Issue Analysis
If this issue has **sub-issues** (listed in the "Sub-issues" section above), this is an **epic**. In that case, adjust your analysis:

1. **Scope validation:** Verify that the sub-issues collectively cover the epic's goals. Identify any gaps — requirements described in the parent that no child addresses
2. **Dependency analysis:** Determine the correct execution order. Identify which sub-issues depend on others and flag circular dependencies
3. **Status review:** Note which sub-issues are already done, in progress, or blocked. Factor this into your spec
4. **Gap identification:** If you find missing sub-tasks needed to fulfill the epic's goals, list them explicitly with suggested titles and descriptions
5. **Risk assessment:** Identify cross-cutting risks that span multiple sub-issues (shared interfaces, migration ordering, breaking changes)

Your spec for an epic should include:
- **Phase plan:** Suggested ordering/grouping of sub-issues into phases
- **Dependency graph:** Which sub-issues must complete before others can start
- **Gaps found:** Missing sub-tasks with suggested scope
- **Cross-cutting concerns:** Shared interfaces, data migrations, or breaking changes that affect multiple sub-issues

If the issue has NO sub-issues, proceed with the standard single-issue analysis (steps 1-7 above).

**CRITICAL: Do NOT create any local files.** You are a read-only research stage — do not write, create, or modify any files in the repository. Your spec must be posted as a comment block (see below), not saved to a file.

To post a comment on the issue, write it between `---COMMENT---` and `---END COMMENT---` markers:
```
---COMMENT---
Your comment text here.
---END COMMENT---
```

Post the spec as a comment block using the markers above. Do NOT use any Linear MCP tools.

{verdict_instruction}. Example: `VERDICT=DONE`, `VERDICT=BLOCKED`, or `VERDICT=ALREADY_DONE`
