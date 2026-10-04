#!/usr/bin/env bash
# Starts one single-use GitHub Actions runner on Cloudflare Containers via the
# rgha-runners Worker (a manual stand-in for the future rgha backend).
#   RGHA_CF_URL=https://rgha-runners.<acct>.workers.dev RGHA_CF_TOKEN_FILE=... \
#     scripts/start-runner.sh <owner/repo> <label> [instance]
set -euo pipefail
repo=$1 label=$2 instance=${3:-basic}
name="cf-$(date +%s)-$RANDOM"
jit=$(gh api -X POST "repos/$repo/actions/runners/generate-jitconfig" \
  -f name="$name" -F runner_group_id=1 -f "labels[]=$label" -f work_folder=_work --jq .encoded_jit_config)
jq -n --arg jit "$jit" '{jit: $jit}' |
  curl -sf -X POST -H "authorization: Bearer $(cat "$RGHA_CF_TOKEN_FILE")" -H 'content-type: application/json' \
    --data-binary @- "$RGHA_CF_URL/runners/$name?instance=$instance" >/dev/null
echo "$name"
