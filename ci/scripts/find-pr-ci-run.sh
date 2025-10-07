#!/bin/bash
set -euox pipefail

COMMIT_SHA="${1:-}"

if [[ -z "$COMMIT_SHA" ]]; then
    echo "Usage: $0 <commit_sha>"
    exit 1
fi

PR_DATA=$(gh api "/repos/${GITHUB_REPOSITORY}/commits/${COMMIT_SHA}/pulls" \
    --jq '.[0] | "\(.number) \(.head.sha)"')

if [[ -z "$PR_DATA" ]]; then
    echo "Error: No PR found for commit ${COMMIT_SHA}" >&2
    exit 1
fi

PR_NUMBER="${PR_DATA%% *}"
PR_HEAD_SHA="${PR_DATA#* }"

RUN_IDS=$(gh api "/repos/${GITHUB_REPOSITORY}/actions/runs?event=pull_request&per_page=100" \
    --jq "[.workflow_runs[] | select(.head_sha == \"${PR_HEAD_SHA}\" and .path == \".github/workflows/build-package.yml\")] | sort_by(.created_at) | reverse | .[].id")

if [[ -z "$RUN_IDS" ]]; then
    echo "Error: No CI runs found" >&2
    exit 1
fi

RUNS_WITH_ARTIFACTS=""
for run_id in $RUN_IDS; do
    artifact_count=$(gh api "/repos/${GITHUB_REPOSITORY}/actions/runs/${run_id}/artifacts" --jq '.total_count')
    if [[ "$artifact_count" -gt 0 ]]; then
        RUNS_WITH_ARTIFACTS="$RUNS_WITH_ARTIFACTS $run_id"
    fi
done

RUNS_WITH_ARTIFACTS=$(echo "$RUNS_WITH_ARTIFACTS" | xargs)

if [[ -z "$RUNS_WITH_ARTIFACTS" ]]; then
    echo "Error: No CI runs with artifacts found for PR #${PR_NUMBER}" >&2
    exit 1
fi

echo "$RUNS_WITH_ARTIFACTS"
