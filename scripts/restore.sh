#!/usr/bin/env bash
# Restore an Atlas Team Postgres backup produced by scripts/backup.sh.
#
# Usage:
#   scripts/restore.sh <backup-file.sql> [database-name]
#
# By default restores into the live `atlas` database (the dump is created
# with --clean --if-exists, so existing objects are dropped first). For a
# restore DRILL against a copy — strongly recommended before you need it —
# pass a scratch database name, e.g.:
#
#   scripts/restore.sh backups/atlas-20260706-120000.sql atlas_drill
#
# The scratch database is created if missing; point a spare backend at it with
# DATABASE_URL=postgres://atlas:...@localhost:5432/atlas_drill to verify.

set -euo pipefail

cd "$(dirname "$0")/.."

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <backup-file.sql> [database-name]" >&2
    exit 1
fi

BACKUP_FILE="$1"
DB_NAME="${2:-atlas}"

if [[ ! -f "${BACKUP_FILE}" ]]; then
    echo "error: backup file not found: ${BACKUP_FILE}" >&2
    exit 1
fi

if [[ "${DB_NAME}" != "atlas" ]]; then
    echo "Ensuring scratch database '${DB_NAME}' exists ..."
    docker compose exec -T postgres psql -U atlas -d postgres -tc \
        "select 1 from pg_database where datname = '${DB_NAME}'" | grep -q 1 ||
        docker compose exec -T postgres createdb -U atlas "${DB_NAME}"
fi

echo "Restoring ${BACKUP_FILE} into database '${DB_NAME}' ..."
docker compose exec -T postgres psql -U atlas -d "${DB_NAME}" -v ON_ERROR_STOP=1 \
    < "${BACKUP_FILE}" > /dev/null

echo "Done. Sanity-check row counts, e.g.:"
echo "  docker compose exec postgres psql -U atlas -d ${DB_NAME} -c 'select count(*) from mutations;'"
