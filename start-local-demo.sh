#!/usr/bin/env bash
# Local kontext-pando demo: FQS on :8787 with limits, JWT, and activity log.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

set -a
# shellcheck disable=SC1091
source "$ROOT/.env.local"
set +a

: "${FQS_SECRET:?set FQS_SECRET in .env.local}"
: "${FQS_LIMITS:?set FQS_LIMITS in .env.local}"
: "${FQS_ACTIVITY_LOG:=/tmp/fqs-logs/activity.jsonl}"
: "${FQS_ACTIVITY_EVENTS:=all}"
: "${FQS_ACTIVITY_STATE_SECS:=60}"

mkdir -p "$(dirname "$FQS_ACTIVITY_LOG")" /tmp/fqs-logs

args=(
  serve
  --host 0.0.0.0
  --port 8787
  --db "$ROOT/fqs.db"
  --log-file /tmp/fqs-logs/fqs.log
  --server-name kontext-pando-local
  --limits "$FQS_LIMITS"
  --jwt-secret "$FQS_SECRET"
  --activity-log "$FQS_ACTIVITY_LOG"
  --activity-events "$FQS_ACTIVITY_EVENTS"
  --activity-state-secs "$FQS_ACTIVITY_STATE_SECS"
  --restart
)
if [[ -n "${FQS_ACTIVITY_SALT:-}" ]]; then
  args+=(--activity-salt "$FQS_ACTIVITY_SALT")
fi

exec "$ROOT/target/release/fqs" "${args[@]}"
