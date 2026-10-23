#!/usr/bin/env bash
# Restore an Atlas Team backup produced by scripts/backup.sh into a database
# of the compose `postgres` service.
#
# Usage:
#   scripts/restore.sh <backup.dump|backup.dump.age|backup.sql> [database-name]
#
# Default target is the LIVE `atlas` database — stop the backend first
# (`docker compose stop backend`). Prefer proving the backup with
# scripts/restore_drill.sh, which restores into a separate, throwaway
# PostgreSQL instance and verifies the result, before you ever need this.
#
# Encrypted backups need ATLAS_BACKUP_AGE_IDENTITY (path to the age private
# key). Legacy plain .sql dumps (pre-custom-format) are still accepted.

set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <backup> [database-name]" >&2
    exit 1
fi
BACKUP_FILE="$1"
DB_NAME="${2:-atlas}"
[[ -f "${BACKUP_FILE}" ]] || { echo "error: backup file not found: ${BACKUP_FILE}" >&2; exit 1; }

if [[ -f "${BACKUP_FILE}.sha256" ]]; then
    ( cd "$(dirname "${BACKUP_FILE}")" && sha256sum --check --quiet "$(basename "${BACKUP_FILE}").sha256" )
fi

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
SOURCE="${BACKUP_FILE}"
if [[ "${BACKUP_FILE}" == *.age ]]; then
    : "${ATLAS_BACKUP_AGE_IDENTITY:?set ATLAS_BACKUP_AGE_IDENTITY to decrypt}"
    SOURCE="${WORK}/backup.dump"
    age --decrypt --identity "${ATLAS_BACKUP_AGE_IDENTITY}" --output "${SOURCE}" "${BACKUP_FILE}"
fi

docker compose exec -T postgres psql -U atlas -d postgres -tc \
    "select 1 from pg_database where datname = '${DB_NAME}'" | grep -q 1 ||
    docker compose exec -T postgres createdb -U atlas "${DB_NAME}"

echo "Restoring ${BACKUP_FILE} into database '${DB_NAME}' ..."
if [[ "${SOURCE}" == *.sql ]]; then
    docker compose exec -T postgres psql -U atlas -d "${DB_NAME}" -v ON_ERROR_STOP=1 \
        < "${SOURCE}" > /dev/null
else
    docker compose exec -T postgres pg_restore -U atlas -d "${DB_NAME}" \
        --clean --if-exists --no-owner --exit-on-error < "${SOURCE}"
fi

echo "Verifying ..."
docker compose exec -T postgres psql -U atlas -d "${DB_NAME}" -v ON_ERROR_STOP=1 \
    < scripts/verify_restore.sql
echo "Done."
