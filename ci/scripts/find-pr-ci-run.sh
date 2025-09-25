#!/bin/bash
set -euox pipefail

# Script to find the PR that was merged and get its CI run ID
# Usage: find-pr-ci-run.sh <commit_sha>

COMMIT_SHA="${1:-}"

if [[ -z "$COMMIT_SHA" ]]; then
    echo "Usage: $0 <commit_sha>"
    exit 1
fi

# Find PR associated with this commit
PR_NUMBER=$(gh api \
    -H "Accept: application/vnd.github+json" \
    "/repos/${GITHUB_REPOSITORY}/commits/${COMMIT_SHA}/pulls" \
    --jq '.[0].number')

if [[ -z "$PR_NUMBER" ]]; then
    echo "Error: No PR found for commit ${COMMIT_SHA}" >&2
    exit 1
fi

# Get the PR's head SHA (the last commit in the PR)
PR_HEAD_SHA=$(gh pr view "$PR_NUMBER" --json headRefOid -q '.headRefOid')

# Find the successful CI workflow run for this PR
RUN_ID=$(gh api \
    -H "Accept: application/vnd.github+json" \
    "/repos/${GITHUB_REPOSITORY}/actions/runs?event=pull_request&status=success" \
    --jq ".workflow_runs[] | select(.head_sha == \"${PR_HEAD_SHA}\" and .name == \"Continuous Integration\") | .id" \
    | head -1)

if [[ -z "$RUN_ID" ]]; then
    echo "Error: No successful CI run found for PR #${PR_NUMBER}" >&2
    exit 1
fi

echo "$RUN_ID"
