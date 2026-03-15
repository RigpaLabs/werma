<!-- DEPRECATED: QA stage removed from default pipeline. -->
# Pipeline: QA Stage
Linear issue: {issue_id}

The code has been reviewed and approved. Run QA checks.

## QA Protocol
1. Run `git diff main...HEAD` to understand what changed
2. Run the test suite
3. Check for regressions in existing functionality
4. Verify the implementation matches the requirements

## Output Format
- End with: QA_VERDICT=PASSED or QA_VERDICT=FAILED
- If FAILED, list specific failures with reproduction steps
