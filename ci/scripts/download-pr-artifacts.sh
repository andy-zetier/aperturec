#!/bin/bash
set -euox pipefail

RUN_IDS="${1:-}"
OUTPUT_DIR="${2:-./artifacts}"

if [[ -z "$RUN_IDS" ]]; then
    echo "Usage: $0 <run_ids> [output_dir]"
    exit 1
fi

mkdir -p "$OUTPUT_DIR"

for run_id in $RUN_IDS; do
    artifacts=$(gh api "/repos/${GITHUB_REPOSITORY}/actions/runs/${run_id}/artifacts" --jq '.artifacts')

    if [[ "$artifacts" == "[]" ]] || [[ -z "$artifacts" ]]; then
        continue
    fi

    while IFS= read -r artifact_json; do
        artifact_id=$(echo "$artifact_json" | jq -r '.id')
        artifact_name=$(echo "$artifact_json" | jq -r '.name')

        gh api "/repos/${GITHUB_REPOSITORY}/actions/artifacts/${artifact_id}/zip" \
            > "artifact_${artifact_id}.zip" || exit 1

        mkdir -p "$OUTPUT_DIR/${artifact_name}"
        unzip -q -o "artifact_${artifact_id}.zip" -d "$OUTPUT_DIR/${artifact_name}"
        rm "artifact_${artifact_id}.zip"
    done < <(echo "$artifacts" | jq -c '.[]')
done