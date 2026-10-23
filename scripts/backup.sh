#!/usr/bin/env bash
# Back up the Atlas Team database: one consistent pg_dump of everything the
# service stores — identity, the mutation log, official postings, audit AND
# attachment bytes (blobs live in the `blobs` table, so there is no separate
# object store to snapshot).
#
# Usage:
#   scripts/backup.sh [output-dir]            # default output dir: ./backups
#
# Source database:
#   * default: the compose `postgres` service (pg_dump runs in the container);
#   * DATABASE_URL set: that database, with the local pg_dump (>= server major).
#
# Output: <output-dir>/atlas-<UTC stamp>.dump (pg_dump custom format, restored
# with pg_restore) plus <file>.sha256. Optional, by environment:
#
#   ATLAS_BACKUP_AGE_RECIPIENTS  path to an age recipients file (public keys).
#                                The dump is encrypted to <file>.age and the
#                                plaintext removed. Keep the matching identity
#                                (private key) OFF this server.
#   ATLAS_BACKUP_RCLONE_REMOTE   rclone destination (e.g. spaces:atlas-backups/prod);
#                                the (encrypted) file + checksum are copied there.
#                                Off-site copies should only ever be encrypted.
#   ATLAS_BACKUP_KEEP_DAYS       local retention in days (default 14). Remote
#                                retention is the bucket's lifecycle policy.
#
# A backup is only proven by a restore: run scripts/restore_drill.sh on a
# schedule (see docs/OPERATIONS.md).

set -euo pipefail
cd "$(dirname "$0")/.."

OUT_DIR="${1:-backups}"
STAMP="$(date -u +%Y%m%d-%H%M%S)"
OUT_FILE="${OUT_DIR}/atlas-${STAMP}.dump"
KEEP_DAYS="${ATLAS_BACKUP_KEEP_DAYS:-14}"

mkdir -p "${OUT_DIR}"
umask 077

echo "[$(date -u +%FT%TZ)] dumping to ${OUT_FILE}"
if [[ -n "${DATABASE_URL:-}" ]]; then
    pg_dump --format=custom --no-owner --dbname="${DATABASE_URL}" --file="${OUT_FILE}"
else
    docker compose exec -T postgres pg_dump -U atlas --format=custom --no-owner atlas > "${OUT_FILE}"
fi

# A dump that pg_restore cannot list is not a backup.
pg_restore --list "${OUT_FILE}" > /dev/null

FINAL="${OUT_FILE}"
if [[ -n "${ATLAS_BACKUP_AGE_RECIPIENTS:-}" ]]; then
    age --encrypt --recipients-file "${ATLAS_BACKUP_AGE_RECIPIENTS}" \
        --output "${OUT_FILE}.age" "${OUT_FILE}"
    rm -f "${OUT_FILE}"
    FINAL="${OUT_FILE}.age"
fi
( cd "$(dirname "${FINAL}")" && sha256sum "$(basename "${FINAL}")" > "$(basename "${FINAL}").sha256" )

if [[ -n "${ATLAS_BACKUP_RCLONE_REMOTE:-}" ]]; then
    if [[ "${FINAL}" != *.age ]]; then
        echo "refusing to copy an unencrypted backup off-site; set ATLAS_BACKUP_AGE_RECIPIENTS" >&2
        exit 1
    fi
    rclone copy "${FINAL}" "${ATLAS_BACKUP_RCLONE_REMOTE}"
    rclone copy "${FINAL}.sha256" "${ATLAS_BACKUP_RCLONE_REMOTE}"
    rclone check --one-way "$(dirname "${FINAL}")" "${ATLAS_BACKUP_RCLONE_REMOTE}" \
        --include "$(basename "${FINAL}")"
    echo "copied off-site to ${ATLAS_BACKUP_RCLONE_REMOTE}"
fi

find "${OUT_DIR}" -maxdepth 1 -name 'atlas-*' -mtime "+${KEEP_DAYS}" -print -delete

echo "[$(date -u +%FT%TZ)] done: ${FINAL} ($(wc -c < "${FINAL}") bytes)"
