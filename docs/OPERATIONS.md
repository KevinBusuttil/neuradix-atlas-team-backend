# Atlas Team backend — operations, backup and recovery

Companion to [DEPLOYMENT_DIGITALOCEAN.md](DEPLOYMENT_DIGITALOCEAN.md) (first
install). This is the runbook for keeping a Team deployment recoverable and
observable. Every procedure here is scripted, and the recovery path is
exercised by CI on every change (`restore-drill` job in `.github/workflows/ci.yml`).

## What has to be protected

Everything lives in one PostgreSQL database: identity, memberships, device
credentials (hashed), the mutation log, official documents and ledgers,
audit, webhook intake **and attachment bytes** (`blobs` table). There is no
separate object store, so one consistent `pg_dump` is a complete backup.
Secrets (`POSTGRES_PASSWORD`, `STRIPE_WEBHOOK_SECRET`, `ATLAS_BOOTSTRAP_TOKEN`)
live in `.env` on the host and are not in the dump; keep them in the
operator's password manager.

Targets for the single-server deployment:

| | Target | How |
|---|---|---|
| RPO (data loss on total host loss) | ≤ 24 h, ≤ 1 h once WAL archiving is added | nightly encrypted off-site dump |
| RTO (time to restored service) | ≤ 4 h | new droplet + `restore_drill.sh`-proven restore |
| Restore proven | every change (CI) + monthly on the production backup | `scripts/restore_drill.sh` |

A 24 h RPO means a day of postings could be lost on a total host loss. Client
devices still hold their local copy, but official postings are server-authored,
so they cannot simply be pushed back. Before the first paying Team customer,
either accept that in the service terms or add continuous WAL archiving
(`archive_command` to the same encrypted bucket) and record the new RPO here.

## Backups (`scripts/backup.sh`)

```bash
# one-time: an age key pair. The PRIVATE key never lives on the server.
age-keygen -o atlas-backup-identity.txt          # on the operator's machine
age-keygen -y atlas-backup-identity.txt > /opt/neuradix-atlas-team-backend/backup-recipients.txt

# rclone remote for DigitalOcean Spaces (or any S3), bucket with versioning
# and a lifecycle rule (e.g. 35 days) for off-site retention:
rclone config   # name it "spaces"
```

`/etc/cron.d/atlas-backup`:

```cron
SHELL=/bin/bash
ATLAS_BACKUP_AGE_RECIPIENTS=/opt/neuradix-atlas-team-backend/backup-recipients.txt
ATLAS_BACKUP_RCLONE_REMOTE=spaces:atlas-backups/prod
ATLAS_BACKUP_KEEP_DAYS=14
15 2 * * * root cd /opt/neuradix-atlas-team-backend && scripts/backup.sh >> /var/log/atlas-backup.log 2>&1
*/5 * * * * root cd /opt/neuradix-atlas-team-backend && ATLAS_ALERT_WEBHOOK_URL=https://hooks.example/... scripts/monitor.sh >> /var/log/atlas-monitor.log 2>&1
```

Each run writes `backups/atlas-<stamp>.dump.age` plus a `.sha256` file, checks that
`pg_restore` can list the archive, copies both off-site (it refuses to upload an
unencrypted dump), verifies the copy with `rclone check`, and prunes local
copies older than `ATLAS_BACKUP_KEEP_DAYS`.

## Restore drill (`scripts/restore_drill.sh`)

Proves a backup restores into a **clean, isolated** PostgreSQL instance (a
fresh `initdb` cluster, or `ATLAS_DRILL_MODE=docker` for a throwaway
`postgres:16` container) that shares nothing with production. The drill then:

1. verifies the checksum and decrypts (`ATLAS_BACKUP_AGE_IDENTITY`);
2. restores with `--exit-on-error`;
3. runs `scripts/verify_restore.sql`, which fails on any of these:
   - an unbalanced GL, per company or per voucher;
   - a bin that disagrees with its stock ledger;
   - a stock ledger pair without a bin;
   - a gap or a duplicate in official numbering;
   - a document without its posting batch;
   - a gap in the sync log or a drifted sync counter;
   - a blob whose bytes no longer match its SHA-256;
4. compares every table's row count with `SOURCE_DATABASE_URL` when given;
5. boots the backend on the restored data and requires `GET /ready`;
6. with `ATLAS_DRILL_COMPANY_ID` and `ATLAS_DRILL_DEVICE_TOKEN` (a device
   credential issued before the backup), pulls the full mutation log through
   the restored API: every mutation must arrive exactly once, which is how a
   replaced device rebuilds its books;
7. writes `drill-reports/restore-drill-<stamp>.md`.

Monthly on the production backup, from the operator's machine (which holds
the private key):

```bash
rclone copy spaces:atlas-backups/prod/atlas-<stamp>.dump.age ./ && \
rclone copy spaces:atlas-backups/prod/atlas-<stamp>.dump.age.sha256 ./
ATLAS_BACKUP_AGE_IDENTITY=atlas-backup-identity.txt scripts/restore_drill.sh atlas-<stamp>.dump.age
```

File the report with the date. A month without a passing drill means the
backup is unproven. An example passing report is in
[evidence/restore-drill-2026-10-23-local.md](evidence/restore-drill-2026-10-23-local.md).

`scripts/verify_restore.sql` is read-only and also safe to run against
production at any time. It is the first check after any incident.

## Full recovery (host lost)

1. New droplet per DEPLOYMENT_DIGITALOCEAN.md §0–4; restore `.env` from the
   password manager; `docker compose up -d postgres`.
2. Fetch the newest backup + checksum; on a machine with the identity:
   `age --decrypt -i identity.txt -o atlas.dump atlas-<stamp>.dump.age`; copy
   `atlas.dump` to the host.
3. `scripts/restore.sh atlas.dump` (it verifies with `verify_restore.sql`), then
   `docker compose up -d backend` and `curl -s 127.0.0.1:8080/ready`.
4. Point DNS / nginx at the new host. Devices reconnect with their existing
   tokens (the hashes are in the dump).
5. **Devices that synced after the backup's time are ahead of the server.**
   Every pull returns the log `head`; a client whose cursor is beyond it
   detects the restored server and re-pulls from 0 automatically (the hub's
   `TeamSyncRunner`; applying is idempotent). Drafts a device pushed after the
   backup are no longer on the server: they are still on that device but
   marked synced, so re-save them (or re-enter them) to queue them again.
   Official postings made after the backup are lost with the RPO window and
   must be re-submitted from the source paperwork. Record which in the
   incident log.

## Migrations and rollback

- Migrations are embedded and applied on start (`PgStore::connect`).
- **Policy: additive only** ("expand/contract"): new tables, nullable or
  defaulted columns, new indexes. A destructive change ships in a later
  release, after no running binary reads the old shape.
- Because of that policy the backend starts against a database migrated by a
  *newer* release (`set_ignore_missing(true)`), so **binary rollback is safe**:

```bash
scripts/backup.sh                                # always, before an upgrade
git fetch --tags && git checkout vX.Y.Z          # the release to deploy
docker compose up -d --build backend
curl -fsS 127.0.0.1:8080/ready                   # schemaVersion = newest migration
# rollback:
git checkout vX.Y.(Z-1) && docker compose up -d --build backend
```

- If a release ever must break the policy, its notes say so, and rolling back
  past it means restoring the pre-upgrade backup.

## Monitoring and alerting

| Signal | Source | Alert |
|---|---|---|
| Process up | `GET /health`, compose healthcheck | compose restarts the container |
| Database reachable, schema version | `GET /ready` (503 when the DB fails) | external uptime monitor (e.g. UptimeRobot, Better Stack) on `https://team.neuradix.app/ready`, plus `scripts/monitor.sh` |
| Backup freshness | `scripts/monitor.sh` (newest backup age > 26 h) | webhook (`ATLAS_ALERT_WEBHOOK_URL`) |
| Backup job failure | `/var/log/atlas-backup.log`, non-zero exit | cron MAILTO or the freshness alert above |
| Request errors | one structured line per request (`request_id`, method, redacted path, status, latency); 5xx at ERROR, 4xx at WARN; `ATLAS_LOG_FORMAT=json` for log shippers | ship `docker compose logs` to the log service of choice and alert on `level=ERROR` |
| Correlation | `x-request-id` response header (client-supplied ids are honoured) | quote it in support tickets |

Credential path segments (portal, pay and invitation tokens) are redacted
from request logs.

## Secret rotation

| Secret | Rotation |
|---|---|
| `POSTGRES_PASSWORD` | `docker compose exec postgres psql -U atlas -c "alter user atlas password '…'"`, update `.env`, `docker compose up -d backend` |
| `STRIPE_WEBHOOK_SECRET` | roll in the Stripe dashboard (it keeps the old secret valid for a grace period), update `.env`, restart backend |
| `ATLAS_BOOTSTRAP_TOKEN` | change `.env`, restart backend (only gates company creation) |
| User tokens | 30-day absolute expiry; `POST /companies/{id}/tokens/rotate` swaps the caller's |
| Device tokens | no expiry by design (offline-first); revoke per device (`…/devices/{id}/revoke`) or by removing the member; a replaced device registers anew |
| Backup age key | generate a new pair, switch `backup-recipients.txt`; keep the old identity until every backup encrypted to it has aged out |

## Incident response

1. **Contain.** Suspected credential compromise: revoke the device / remove
   the member (takes effect on the next request), and rotate the affected
   secret. Suspected data corruption: stop the backend
   (`docker compose stop backend`) so devices stop syncing; their local
   queues hold their changes.
2. **Assess.** Run `scripts/verify_restore.sql` against production, then read
   `GET /companies/{id}/audit` and the request logs around the event. Find the
   affected requests by `request_id`.
3. **Recover.** Pick the newest backup that predates the incident. Drill it
   with `restore_drill.sh`, then restore it (see Full recovery). Corrections to
   official documents go through cancel/re-submit, never through SQL edits.
4. **Communicate.** Tell affected companies what happened, the time window,
   which postings must be re-entered and which devices must resync.
5. **Record.** Write a dated incident note with the timeline, cause, data
   impact, the drill report used, and follow-up actions.
