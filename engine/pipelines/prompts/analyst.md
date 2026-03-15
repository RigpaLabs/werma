# Pipeline: Analyst Stage
Linear issue: {issue_id}

[{issue_id}] {issue_title}

{issue_description}

## Instructions
1. Read the issue description carefully
2. **Check for duplicates:** search Linear backlog for similar/overlapping issues. If found, link them as `duplicate` in Linear and note in the spec
3. **Cross-link related issues:** find issues that share code areas, dependencies, or context. Add Linear relations (blocks, related)
4. Clarify requirements and identify ambiguities
5. **If critical info is missing or requirements are contradictory:** set the issue status to `Blocked` in Linear, add a comment listing specific questions for the human, and output `VERDICT=BLOCKED`
6. **If the issue is already implemented** (PR **merged** into main AND changes verified): output `VERDICT=ALREADY_DONE`. IMPORTANT: an open/unmerged PR does NOT mean the issue is done — it means work is in progress. Only use ALREADY_DONE when the PR has been merged and the feature is on main
7. Write a detailed implementation spec
8. Include acceptance criteria
9. Identify risks and dependencies

Post the spec as a **Linear comment** on the issue (use save_comment MCP tool). Do NOT save to local files.

{verdict_instruction}. Example: `VERDICT=DONE`, `VERDICT=BLOCKED`, or `VERDICT=ALREADY_DONE`
