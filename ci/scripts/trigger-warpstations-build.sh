#!/bin/sh
set -e

if [ -z "$GITHUB_TRIGGER_TOKEN" ]; then
  echo "ERROR: GITHUB_TRIGGER_TOKEN is not set"
  exit 1
fi

PAYLOAD=$(cat <<EOF
{
  "event_type": "zip_uploaded",
  "client_payload": {
    "ref": "$CI_COMMIT_REF_NAME",
    "pipeline_id": "$CI_PIPELINE_ID"
  }
}
EOF
)

echo "Triggering Warpstations build with payload:"
echo "$PAYLOAD"

RESPONSE=$(curl -s -w "\n%{http_code}" -X POST \
  -H "Accept: application/vnd.github.v3+json" \
  -H "Authorization: Bearer $GITHUB_TRIGGER_TOKEN" \
  -H "Content-Type: application/json" \
  https://api.github.com/repos/zetier/Warpstations-MacOS/dispatches \
  -d "$PAYLOAD")

HTTP_CODE=$(echo "$RESPONSE" | tail -n1)
RESPONSE_BODY=$(echo "$RESPONSE" | head -n -1)

echo "HTTP Status Code: $HTTP_CODE"
echo "Response Body: $RESPONSE_BODY"

if [ "$HTTP_CODE" -ge 200 ] && [ "$HTTP_CODE" -lt 300 ]; then
  echo "Successfully triggered Warpstations build"
else
  echo "ERROR: Failed to trigger Warpstations build"
  echo "HTTP Status: $HTTP_CODE"
  echo "Response: $RESPONSE_BODY"
  exit 1
fi