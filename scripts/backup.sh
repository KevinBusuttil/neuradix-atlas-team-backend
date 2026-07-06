#!/usr/bin/env bash
# Back up the Atlas Team Postgres database to a timestamped SQL file.
#
# Usage:
#   scripts/backup.sh [output-dir]
#
# Runs pg_dump inside the compose `postgres` service, so it needs no local
# Postgres tools. Output: <output-dir>/atlas-<UTC timestamp>.sql
# (default output dir: ./backups). Restore drill: see scripts/restore.sh —
# roadmap §6 criterion 10 expects backup AND restore to be practiced.

set -euo pipefail

cd "$(dirname "$0")/.."

OUT_DIR="${1:-backups}"
STAMP="$(date -u +%Y%m%d-%H%M%S)"
OUT_FILE="${OUT_DIR}/atlas-${STAMP}.sql"

mkdir -p "${OUT_DIR}"

echo "Dumping database 'atlas' to ${OUT_FILE} ..."
docker compose exec -T postgres pg_dump -U atlas --clean --if-exists atlas > "${OUT_FILE}"

echo "Done: $(wc -c < "${OUT_FILE}") bytes."
echo "Verify the drill regularly: restore this file into a scratch database"
echo "with scripts/restore.sh and check the service comes up against it."
