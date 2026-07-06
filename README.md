# Atlas Team Rust Backend

The **Phase 2 coordination MVP** of the Neuradix Atlas Team backend: the always-on
counterpart to the local-first Flutter client. It provides identity (companies,
users, memberships, devices), the per-company **mutation-log sync** the client's
`CloudAdapter` contract expects, content-addressed **blob** storage for attachment
bytes, raw **webhook intake**, and a server-side **audit log**.

Design references (in the `mercantis.hub.flutter` repo):

- `docs/ATLAS_SOLO_TEAM_BACKEND_DECISION.md` — why this backend exists, the stack
  (§5), the authority model (§6), the sync model (§7), identity/audit (§8).
- `docs/ROADMAP_V2_SOLO_TEAM.md` — §6 lists the acceptance criteria this MVP
  implements (the [P2] items; see the table below).

**Not in this scaffold:** the posting engine. Official document submission,
gap-free numbering, immutability of submitted documents and the stock/COGS
fixture (roadmap §6 criteria 5–7 and 11, all tagged [P3]) are **Phase 3** —
this service deliberately contains no posting authority yet. Webhook signature
verification and processing are Phase 4; intake here is log-only.

## Architecture notes

- **Axum + Tokio + SQLx + PostgreSQL** (decision doc §5). All persistence sits
  behind an async `Store` trait (`src/store/mod.rs`) with two implementations:
  - `MemStore` — in-memory; used by the test suite and the `--mem` dev mode.
  - `PgStore` — SQLx/Postgres with runtime queries (no compile-time `query!`
    macros), so the project builds and tests **without any database**.
- The wire format of a mutation is the Dart client's `MutationRecord`
  (camelCase: `id, type, docType, documentId, payload, deviceId, userId,
  localTimestamp, syncVersion, status`). Sync versions are assigned
  server-side, monotonically increasing per company; `syncVersion` is returned
  as a string because the Dart field is `String?`.
- Bearer tokens (user tokens from bootstrap/invitation-accept, device tokens
  from device registration) are opaque random values; only SHA-256 hashes are
  stored. Every company-scoped route checks membership; roles gate sensitive
  routes server-side.
- Every state-changing authenticated endpoint writes an `audit_log` row
  (company, user, device, action, detail, timestamp). Webhook intake is itself
  the durable log (`webhook_events`).

## Running

### Dev mode, no database

```sh
cargo run -- --mem
curl localhost:8080/health   # {"status":"ok"}
```

State lives in memory and is lost on exit. `PORT` overrides the default 8080.

### Compose mode (Postgres 16)

```sh
docker compose up --build
```

Brings up `postgres:16` (volume `pgdata`, healthchecked) and the backend
(migrations in `migrations/` are applied automatically on startup). Set
`POSTGRES_PASSWORD` in the environment or an `.env` file for anything
non-local. A Caddy TLS proxy stub is provided (`Caddyfile` + the commented
`caddy` service) for exposing the webhook endpoints publicly.

Running against an existing Postgres instead: `DATABASE_URL=postgres://... cargo run`.

### Backup / restore

```sh
scripts/backup.sh                     # pg_dump -> backups/atlas-<UTC stamp>.sql
scripts/restore.sh backups/atlas-....sql atlas_drill   # drill into a scratch DB
scripts/restore.sh backups/atlas-....sql               # restore the live DB
```

Both scripts run the tools inside the compose `postgres` service. Drill the
restore on a copy regularly — that is part of the acceptance criterion, not an
afterthought.

## API surface

All request/response bodies are JSON unless noted. Authenticated routes take
`Authorization: Bearer <token>`; every `/companies/{id}/...` route requires the
caller to be a member of that company (403 otherwise; 401 when the token is
missing or unknown).

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/health` | none | Liveness: `{"status":"ok"}` |
| POST | `/companies` | none (bootstrap) | `{name, owner_email, owner_name}` → company + owner + owner membership; returns a user token |
| POST | `/companies/{id}/invitations` | user/device token, **owner or admin** | `{email, role}` → `{token, expiresAt}` (7-day expiry) |
| POST | `/invitations/{token}/accept` | none (token is the credential) | `{display_name}` → creates/joins user + membership; returns a user token |
| POST | `/companies/{id}/devices` | user token | `{name}` → `{deviceId, deviceToken}` |
| POST | `/companies/{id}/sync/push` | **device token** | `{mutations: [MutationRecord…]}` → `{versions: {id: version}}`; idempotent on mutation id |
| GET | `/companies/{id}/sync/pull?after={v}` | **device token** | `{mutations: […]}` with `syncVersion` set, ordered by version; `after` omitted = from 0 |
| POST | `/companies/{id}/sync/ack` | **device token** | `{ids: […]}` → `{acknowledged: n}` |
| PUT | `/companies/{id}/blobs/{sha256}` | member | Raw bytes body; 201, or **422** if the body's SHA-256 ≠ path |
| GET / HEAD | `/companies/{id}/blobs/{sha256}` | member | Bytes / existence; 404 when absent |
| POST | `/webhooks/payments/{provider}` | none (Phase 4 verifies signatures) | Logs raw headers+body to `webhook_events` → `{"logged": true}` |
| POST | `/webhooks/channels/{connector}` | none (Phase 4 verifies signatures) | Same, `kind = channel` |
| GET | `/companies/{id}/audit?limit=n` | **owner, admin or accountant** | Recent audit rows (newest first) |

Roles: `owner, admin, sales, purchasing, stock, pos, accountant, advisor`.

## ROADMAP_V2 §6 — [P2] acceptance criteria coverage

| # | Criterion ([P2]) | Endpoint(s) | Test (`tests/api.rs`) |
|---|---|---|---|
| 1 | Owner can create a Team company | `POST /companies` | `bootstrap_creates_company_owner_and_token` |
| 2 | Owner can invite another user | `POST /companies/{id}/invitations` | `invitation_flow_lets_second_user_join_and_register_devices`, `only_owner_or_admin_can_invite` |
| 3 | Second user can join and register a device | `POST /invitations/{token}/accept`, `POST /companies/{id}/devices` | `invitation_flow_lets_second_user_join_and_register_devices`, `invitation_cannot_be_accepted_twice` |
| 4 | Two devices sync masters/drafts through the backend | `POST …/sync/push`, `GET …/sync/pull`, `POST …/sync/ack`, blob routes | `push_assigns_monotonic_versions_and_is_idempotent`, `pull_returns_camelcase_records_in_version_order`, `ack_marks_mutations_acknowledged`, `blob_put_get_head_roundtrip_and_hash_check` |
| 8 | Audit log records user, device, company, action, timestamp for every backend action | all mutating endpoints → `GET …/audit` | `every_mutating_action_writes_an_audit_row` |
| 9 | Roles restrict access server-side | role checks on invitations + audit; device-only sync | `only_owner_or_admin_can_invite`, `audit_feed_is_role_restricted`, `sync_requires_a_device_token`, `non_members_get_403` |
| 10 | Backup and restore are possible and drilled | `scripts/backup.sh`, `scripts/restore.sh` (documented drill) | n/a (operational scripts) |
| 12 | Payment link events can be received and logged | `POST /webhooks/payments/{provider}` | `webhooks_are_logged_without_auth` |
| 13 | Online store webhook events can be received and logged | `POST /webhooks/channels/{connector}` | `webhooks_are_logged_without_auth` |

**Posting authority — criteria 5, 6, 7 and 11 ([P3]) — is Phase 3 and is not
part of this scaffold.** Nothing here allocates official numbers, submits
documents or posts GL/stock entries.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test          # 15 integration tests over MemStore; no DB required
```

Schema lives in `migrations/0001_init.sql` (applied by `PgStore::connect` via
embedded SQLx migrations; includes the `mutations(company_id, sync_version)`
index the pull path relies on).
