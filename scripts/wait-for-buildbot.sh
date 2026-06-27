#!/usr/bin/env bash
# Poll a commit's buildbot check run until it finishes. Lets a normal,
# unprivileged CI job run only after nixbot reports success, without the
# secret/cache exposure of a status-event-triggered workflow.
#
# Usage: wait-for-buildbot.sh <sha> [check-name] [timeout-seconds]
# Requires gh authenticated via GH_TOKEN.
set -euo pipefail

sha="$1"
check="${2:-buildbot/nix-build}"
timeout="${3:-3600}"
deadline=$(($(date +%s) + timeout))

while :; do
  read -r status conclusion < <(
    gh api "repos/$GITHUB_REPOSITORY/commits/$sha/check-runs" \
      --jq "([.check_runs[] | select(.name==\"$check\")][0]) | \"\(.status // \"pending\") \(.conclusion // \"\")\""
  )
  echo "nixbot check ($check): status=$status conclusion=$conclusion"
  if [ "$status" = "completed" ]; then
    [ "$conclusion" = "success" ] && exit 0
    echo "nixbot concluded: $conclusion" >&2
    exit 1
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "timed out waiting for $check" >&2
    exit 1
  fi
  sleep 30
done
