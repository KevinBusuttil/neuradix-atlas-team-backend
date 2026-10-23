#!/usr/bin/env bash
# Minimal self-hosted alerting: probe the backend's readiness and the age of
# the newest backup, and POST an alert when either is unhealthy. Run it from
# cron every few minutes (docs/OPERATIONS.md). An external uptime monitor on
# GET /ready is still recommended — this script cannot report the host
# itself being down.
#
#   ATLAS_READY_URL          default http://127.0.0.1:8080/ready
#   ATLAS_BACKUP_DIR         default ./backups
#   ATLAS_BACKUP_MAX_AGE_H   default 26 (nightly backups + slack)
#   ATLAS_ALERT_WEBHOOK_URL  where alerts go: any endpoint accepting
#                            {"text": "..."} (Slack/Mattermost/Discord-style
#                            incoming webhook). Unset = print to stderr only.
#
# Exit status is non-zero when something is wrong, so it also works as a
# systemd/cron health unit.

set -uo pipefail
cd "$(dirname "$0")/.."

READY_URL="${ATLAS_READY_URL:-http://127.0.0.1:8080/ready}"
BACKUP_DIR="${ATLAS_BACKUP_DIR:-backups}"
MAX_AGE_H="${ATLAS_BACKUP_MAX_AGE_H:-26}"
HOST="$(hostname)"
problems=()

if ! body="$(curl -fsS --max-time 10 "${READY_URL}" 2>&1)"; then
    problems+=("backend not ready at ${READY_URL}: ${body}")
fi

newest="$(find "${BACKUP_DIR}" -maxdepth 1 -name 'atlas-*.dump*' ! -name '*.sha256' -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -1)"
if [[ -z "${newest}" ]]; then
    problems+=("no backup found in ${BACKUP_DIR}")
else
    age_h=$(( ( $(date +%s) - ${newest%%.*} ) / 3600 ))
    if (( age_h > MAX_AGE_H )); then
        problems+=("newest backup is ${age_h}h old: ${newest#* }")
    fi
fi

if (( ${#problems[@]} == 0 )); then
    exit 0
fi
message="Atlas Team ALERT on ${HOST}: $(printf '%s; ' "${problems[@]}")"
echo "${message}" >&2
if [[ -n "${ATLAS_ALERT_WEBHOOK_URL:-}" ]]; then
    payload="$(python3 -c 'import json,sys; print(json.dumps({"text": sys.argv[1]}))' "${message}")"
    curl -fsS --max-time 10 -H 'Content-Type: application/json' -d "${payload}" \
        "${ATLAS_ALERT_WEBHOOK_URL}" > /dev/null || echo "alert delivery failed" >&2
fi
exit 1
