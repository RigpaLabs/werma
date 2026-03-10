# Pipeline: Deploy Stage
Linear issue: {issue_id}

The implementation has passed QA and is ready for deployment.

## Deploy Protocol
1. Check the PR is merged or ready to merge
2. Run the deployment procedure for this project
3. Verify the deployment succeeded (check logs, health endpoints, etc.)
4. Report any issues encountered

## Output Format
- End with: DEPLOY_VERDICT=DONE or DEPLOY_VERDICT=FAILED
- If FAILED, describe what went wrong and what was attempted
