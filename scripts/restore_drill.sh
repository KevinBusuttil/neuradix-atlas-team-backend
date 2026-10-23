#!/usr/bin/env bash
# Restore drill: prove a backup restores into a CLEAN, ISOLATED PostgreSQL
# instance and that the restored system is consistent and serviceable.
# A pg_dump file existing proves nothing; this script is the proof.
#
# Usage:
#   scripts/restore_drill.sh <backup.dump | backup.dump.age>
#
# What it does, failing loudly at the first problem:
#   1. checks the backup's .sha256 (when present) and decrypts .age backups
#      (ATLAS_BACKUP_AGE_IDENTITY = path to the age private key);
#   2. starts a brand-new throwaway PostgreSQL instance — `initdb` on a temp
#      directory (default), or `ATLAS_DRILL_MODE=docker` for a postgres:16
#      container — that shares nothing with production;
#   3. pg_restore's the backup into it (--exit-on-error);
#   4. runs scripts/verify_restore.sql: double entry per company and per
#      voucher, bins = stock ledger, gap-free official numbering, unique
#      official numbers, posting batches, gap-free sync log, intact blob
#      content addresses;
#   5. compares every table's row count with SOURCE_DATABASE_URL (optional —
#      the live database the backup was taken from, when it is quiescent);
#   6. boots the backend binary against the restored database and requires
#      GET /ready (database reachable, schema version reported);
#   7. optional client resync proof: with ATLAS_DRILL_COMPANY_ID and
#      ATLAS_DRILL_DEVICE_TOKEN (a device credential issued BEFORE the
#      backup), pulls the company's full mutation log page by page through
#      the restored API and requires every logged mutation to arrive — a
#      reinstalled device can rebuild its books from the restored server;
#   8. writes a timestamped report to drill-reports/ and tears everything down.
#
# Environment: PG_BIN (directory holding initdb/pg_ctl; default: autodetect),
# ATLAS_BACKEND_BIN (default target/release/atlas-team-backend, then
# target/debug/atlas-team-backend).

set -euo pipefail
cd "$(dirname "$0")/.."

[[ $# -ge 1 ]] || { echo "usage: $0 <backup.dump|backup.dump.age>" >&2; exit 2; }
BACKUP="$1"
[[ -f "${BACKUP}" ]] || { echo "backup not found: ${BACKUP}" >&2; exit 2; }

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT_DIR="${ATLAS_DRILL_REPORT_DIR:-drill-reports}"
mkdir -p "${REPORT_DIR}"
REPORT="${REPORT_DIR}/restore-drill-${STAMP}.md"
WORK="$(mktemp -d)"
PORT="${ATLAS_DRILL_PG_PORT:-$(( 20000 + RANDOM % 20000 ))}"
API_PORT="${ATLAS_DRILL_API_PORT:-$(( 40000 + RANDOM % 20000 ))}"
MODE="${ATLAS_DRILL_MODE:-initdb}"
BACKEND_PID=""
CONTAINER=""
PG_RUN=()

log() { echo "[drill] $*" | tee -a "${REPORT}"; }
cleanup() {
    [[ -n "${BACKEND_PID}" ]] && kill "${BACKEND_PID}" 2>/dev/null || true
    if [[ "${MODE}" == "docker" && -n "${CONTAINER}" ]]; then
        docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true
    elif [[ -n "${PG_BIN:-}" && -d "${WORK}/pg" ]]; then
        "${PG_RUN[@]}" "${PG_BIN}/pg_ctl" -D "${WORK}/pg" -m immediate stop >/dev/null 2>&1 || true
    fi
    rm -rf "${WORK}"
}
trap cleanup EXIT
trap 'echo "[drill] FAIL at line ${LINENO}" | tee -a "${REPORT}"; printf "\x60\x60\x60\n\n**Result: FAIL**\n" >> "${REPORT}"' ERR

{
    echo "# Atlas Team restore drill — ${STAMP}"
    echo
    echo "- Backup: \`${BACKUP}\` ($(wc -c < "${BACKUP}") bytes)"
    echo "- Host: $(hostname) · isolation: ${MODE} PostgreSQL instance on port ${PORT}"
    echo
    echo '```'
} > "${REPORT}"

# 1. Integrity + decryption.
if [[ -f "${BACKUP}.sha256" ]]; then
    ( cd "$(dirname "${BACKUP}")" && sha256sum --check --quiet "$(basename "${BACKUP}").sha256" )
    log "checksum OK"
else
    log "WARNING: no ${BACKUP}.sha256 next to the backup; integrity not checked"
fi
DUMP="${BACKUP}"
if [[ "${BACKUP}" == *.age ]]; then
    : "${ATLAS_BACKUP_AGE_IDENTITY:?set ATLAS_BACKUP_AGE_IDENTITY to decrypt the backup}"
    DUMP="${WORK}/backup.dump"
    age --decrypt --identity "${ATLAS_BACKUP_AGE_IDENTITY}" --output "${DUMP}" "${BACKUP}"
    log "decrypted with the age identity"
fi
pg_restore --list "${DUMP}" > /dev/null
log "archive lists cleanly"

# 2. A clean, isolated PostgreSQL instance.
if [[ "${MODE}" == "docker" ]]; then
    CONTAINER="atlas-drill-${STAMP,,}"
    docker run -d --rm --name "${CONTAINER}" -e POSTGRES_USER=atlas -e POSTGRES_PASSWORD=drill \
        -e POSTGRES_DB=postgres -p "127.0.0.1:${PORT}:5432" postgres:16 > /dev/null
    ADMIN_URL="postgres://atlas:drill@127.0.0.1:${PORT}/postgres"
    for _ in $(seq 60); do pg_isready -q -d "${ADMIN_URL}" && break; sleep 1; done
else
    PG_BIN="${PG_BIN:-$(dirname "$(command -v initdb 2>/dev/null || ls /usr/lib/postgresql/*/bin/initdb | sort -V | tail -1)")}"
    # PostgreSQL refuses to run as root; under sudo the instance runs as the
    # `postgres` system user (the server package creates it).
    PG_RUN=()
    if [[ "$(id -u)" == 0 ]]; then
        PG_RUN=(runuser -u postgres --)
        chmod 755 "${WORK}"
        install -d -o postgres -m 700 "${WORK}/pg" "${WORK}/sock"
    else
        mkdir -p "${WORK}/sock"
    fi
    "${PG_RUN[@]}" "${PG_BIN}/initdb" -D "${WORK}/pg" -U atlas --auth=trust > /dev/null
    "${PG_RUN[@]}" "${PG_BIN}/pg_ctl" -D "${WORK}/pg" -l "${WORK}/pg/drill.log" -w \
        -o "-p ${PORT} -c listen_addresses=127.0.0.1 -k ${WORK}/sock" start > /dev/null
    ADMIN_URL="postgres://atlas@127.0.0.1:${PORT}/postgres"
fi
DRILL_URL="${ADMIN_URL%/postgres}/atlas_drill"
psql -q -d "${ADMIN_URL}" -c "create database atlas_drill" 
log "fresh PostgreSQL $(psql -tA -d "${ADMIN_URL}" -c 'show server_version') started (${MODE})"

# 3. Restore.
START=$(date +%s)
pg_restore --no-owner --exit-on-error --dbname="${DRILL_URL}" "${DUMP}"
log "restored in $(( $(date +%s) - START ))s"

# 4. Invariants.
psql -X -q -d "${DRILL_URL}" -v ON_ERROR_STOP=1 -f scripts/verify_restore.sql \
    -P pager=off > "${WORK}/verify.txt" 2>&1 || { cat "${WORK}/verify.txt" | tee -a "${REPORT}"; log "FAIL: invariants"; exit 1; }
cat "${WORK}/verify.txt" >> "${REPORT}"
log "invariants hold"

# 5. Row counts against the source.
if [[ -n "${SOURCE_DATABASE_URL:-}" ]]; then
    COUNTS_SQL="select table_name, (xpath('/row/n/text()', query_to_xml(format('select count(*) as n from %I', table_name), false, true, '')))[1]::text from information_schema.tables where table_schema='public' and table_type='BASE TABLE' order by 1"
    psql -X -tA -d "${SOURCE_DATABASE_URL}" -c "${COUNTS_SQL}" > "${WORK}/source.counts"
    psql -X -tA -d "${DRILL_URL}" -c "${COUNTS_SQL}" > "${WORK}/drill.counts"
    if diff -u "${WORK}/source.counts" "${WORK}/drill.counts" >> "${REPORT}"; then
        log "row counts identical to the source in all $(wc -l < "${WORK}/source.counts") tables"
    else
        log "FAIL: row counts differ from the source (diff above)"; exit 1
    fi
fi

# 6. The service runs on the restored data.
BIN="${ATLAS_BACKEND_BIN:-}"
if [[ -z "${BIN}" ]]; then
    for candidate in target/release/atlas-team-backend target/debug/atlas-team-backend; do
        [[ -x "${candidate}" ]] && BIN="${candidate}" && break
    done
fi
[[ -x "${BIN}" ]] || { log "FAIL: no backend binary (cargo build --release, or set ATLAS_BACKEND_BIN)"; exit 1; }
DATABASE_URL="${DRILL_URL}" PORT="${API_PORT}" RUST_LOG=warn ATLAS_BOOTSTRAP_TOKEN=drill-only \
    "${BIN}" > "${WORK}/backend.log" 2>&1 &
BACKEND_PID=$!
READY=""
for _ in $(seq 60); do
    READY="$(curl -fsS "http://127.0.0.1:${API_PORT}/ready" 2>/dev/null || true)"
    [[ -n "${READY}" ]] && break
    sleep 0.5
done
[[ -n "${READY}" ]] || { cat "${WORK}/backend.log" >> "${REPORT}"; log "FAIL: backend never became ready"; exit 1; }
log "backend ready on the restored database: ${READY}"

# 7. A device rebuilds its books from the restored server.
if [[ -n "${ATLAS_DRILL_COMPANY_ID:-}" && -n "${ATLAS_DRILL_DEVICE_TOKEN:-}" ]]; then
    EXPECTED="$(psql -X -tA -d "${DRILL_URL}" -c \
        "select count(*) from mutations where company_id = '${ATLAS_DRILL_COMPANY_ID}'")"
    PULLED="$(python3 - "${API_PORT}" "${ATLAS_DRILL_COMPANY_ID}" "${ATLAS_DRILL_DEVICE_TOKEN}" <<'PY'
import json, sys, urllib.request
port, company, token = sys.argv[1:4]
after, total, seen = 0, 0, set()
while True:
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}/companies/{company}/sync/pull?after={after}",
        headers={"Authorization": f"Bearer {token}"})
    page = json.load(urllib.request.urlopen(req))
    for m in page["mutations"]:
        seen.add(m["id"]); after = max(after, int(m["syncVersion"]))
    total += len(page["mutations"])
    if not page["hasMore"]:
        break
print(len(seen) if len(seen) == total else -1)
PY
)"
    if [[ "${PULLED}" == "${EXPECTED}" ]]; then
        log "full resync through the restored API: ${PULLED}/${EXPECTED} mutations, no duplicates"
    else
        log "FAIL: resync pulled ${PULLED}, log holds ${EXPECTED}"; exit 1
    fi
fi

echo '```' >> "${REPORT}"
echo >> "${REPORT}"
echo "**Result: PASS**" >> "${REPORT}"
echo "[drill] PASS — report: ${REPORT}"
