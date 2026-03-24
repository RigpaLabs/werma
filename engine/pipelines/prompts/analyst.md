# Pipeline: Analyst Stage
Linear issue: {issue_id}

[{issue_id}] {issue_title}

The issue context is provided above in the ---ISSUE--- block.

## Instructions
1. Read the issue description carefully from the ---ISSUE--- block
2. Clarify requirements and identify ambiguities
3. **If critical info is missing or requirements are contradictory:** output `VERDICT=BLOCKED` and write a comment block listing specific questions for the human
4. **If the issue is already implemented** (PR **merged** into main AND changes verified): output `VERDICT=ALREADY_DONE`. IMPORTANT: an open/unmerged PR does NOT mean the issue is done — it means work is in progress. Only use ALREADY_DONE when the PR has been merged and the feature is on main
5. Write a detailed implementation spec
6. Include acceptance criteria
7. Identify risks and dependencies

**CRITICAL: Do NOT create any local files.** You are a read-only research stage — do not write, create, or modify any files in the repository. Your spec must be posted as a comment block (see below), not saved to a file.

To post a comment on the issue, write it between `---COMMENT---` and `---END COMMENT---` markers:
```
---COMMENT---
Your comment text here.
---END COMMENT---
```

Post the spec as a comment block using the markers above. Do NOT use any Linear MCP tools.

{verdict_instruction}. Example: `VERDICT=DONE`, `VERDICT=BLOCKED`, or `VERDICT=ALREADY_DONE`
