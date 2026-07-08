# Atlas Team Rust Backend

The Neuradix Atlas Team backend: the always-on counterpart to the local-first
Flutter client. **Phase 2** provides identity (companies, users, memberships,
devices), the per-company **mutation-log sync** the client's `CloudAdapter`
contract expects, content-addressed **blob** storage for attachment bytes, raw
**webhook intake**, and a server-side **audit log**. **Phase 3** adds the
**posting authority**: backend-confirmed official document submission with
gap-free numbering, perpetual-inventory stock + COGS GL posting, reversals,
settlements and server-side immutability (see the Phase 3 section below).

Design references (in the `mercantis.hub.flutter` repo):

- `docs/ATLAS_SOLO_TEAM_BACKEND_DECISION.md` — why this backend exists, the stack
  (§5), the authority model (§6), the sync model (§7), identity/audit (§8).
- `docs/ROADMAP_V2_SOLO_TEAM.md` — §6 lists the acceptance criteria: the [P2]
  coordination items and the [P3] posting-authority items, both covered by the
  tables below.
- `docs/STOCK_COGS_IMPLEMENTATION_PLAN.md` — §2 target accounting behaviour,
  §4 Track B (this engine), §5 the shared fixture suite.

**Not in this service yet:** webhook signature verification and processing
(Phase 4; intake here is log-only), POS session close, and the client
preview-vs-official flow (client-side work).

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
| GET / PUT | `/companies/{id}/settings` | member / **owner, admin or accountant** | Company posting settings (merge-patch): `allow_negative_stock`, `books_lock_date`, default posting accounts |
| POST | `/companies/{id}/items` | **owner, admin or stock** | Item-registry upsert: `{id, item_type, valuation_method, *_account overrides}` |
| POST | `/companies/{id}/commands/submit-document` | **device token**, role per doctype | Official submission → `{document_id, number, docstatus, gl_entries, stock_ledger_entries, bins, settlements}` |
| POST | `/companies/{id}/commands/cancel-document` | **device token**, role per doctype | Reversal batch (negated legs, `-reversal` ids), docstatus 2 |

Roles: `owner, admin, sales, purchasing, stock, pos, accountant, advisor`.
Command-role gates: Sales Invoice → sales/owner/admin · Purchase
Invoice/Receipt → purchasing/owner/admin · Stock Entry → stock/owner/admin ·
Payment Entry → accountant/owner/admin (cancel requires the same role as
submit).

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

## Phase 3 — backend-authoritative postings

Track B of `docs/STOCK_COGS_IMPLEMENTATION_PLAN.md`: the Rust engine is the
posting authority for Team companies, semantically ported from the Dart Solo
engine (`lib/ledger/ledger_derivation.dart`, `ledger_derivation_service.dart`,
`stock_costing.dart`, `stock_balance.dart`) — same deterministic row ids
(`GL-{id}-debit`, `SLE-{id}-{i}`, `{sleId}-gl-d`/`-gl-c`, `-reversal` suffix),
same moving-average/FIFO issue costing (FIFO shortfall falls back to moving
average), same perpetual-inventory GL mapping (sales issue → Dr COGS / Cr
Inventory at valuation cost; purchase receipt → Dr Inventory / Cr GRNI with the
Purchase Invoice's stock value split off the expense leg onto GRNI; stock-entry
movements → the stock-adjustment account; transfers value-neutral, no GL), same
return semantics (goods re-enter at the original voucher's issue cost).

Each command is **one atomic store transaction** (`Store::posting_commit`):
validate (per-doctype role, period lock via `books_lock_date`, JE-style balance
guard on the generated GL, negative-stock rejection unless
`allow_negative_stock`) → allocate the gap-free official number → insert
document + GL + SLE + settlements + posting batch → recompute the touched bins
→ write the audit row. Costing reads are protected by optimistic stock-ledger
expectations: if a concurrent commit moved a touched (item, warehouse) pair,
the commit fails `Stale` and the command recomputes. `idempotency_key` replays
return the originally committed response without posting again. Cancellation
mirrors the stored rows (reusing stored SLE rates — issues are never re-costed)
into a linked `PB-{id}-reversal` batch and sets docstatus 2. Official (posted)
documents are immutable on the sync plane: `sync/push` rejects any mutation
targeting them with 409.

Posted results also **replicate to client devices through the mutation log**:
the same atomic commit appends system-authored mutations (device id
`atlas-backend`, deterministic `postmut-…` ids, log-idempotent) for the
document (`submitDocument`/`cancelDocument`), every GL / stock ledger /
settlement row, every touched bin, and each referenced invoice's
outstanding-amount update — in the Dart sync engine's row-envelope wire shape
(`src/posting/replication.rs`), so a normal `GET …/sync/pull` delivers the
official state to every device (`tests/posting_replication.rs`).

### ROADMAP_V2 §6 — [P3] acceptance criteria coverage

| # | Criterion ([P3]) | Endpoint(s) | Test |
|---|---|---|---|
| 5 | Official document submission is backend-confirmed | `POST …/commands/submit-document`, `POST …/commands/cancel-document` | `tests/fixtures.rs` (all fixtures), `tests/posting.rs::commands_require_device_tokens_and_write_audit_rows` |
| 6 | Official numbers allocated safely (no duplicates/races; gap-free) | number allocation inside `posting_commit`; rejected submits burn no number | `tests/posting.rs::gap_free_numbering_under_20_concurrent_submits`, `…::idempotency_key_replay_returns_same_result_without_double_posting`, fixture `0006` |
| 7 | Submitted documents cannot be destructively edited | `sync/push` 409 guard; cancel-only state machine | `tests/posting.rs::submitted_documents_are_immutable_via_sync_push`, `…::cancel_requires_a_submitted_document_and_cannot_repeat` |
| 11 | Mandatory stock/COGS posting test passes on Rust | full command surface | `tests/fixtures.rs::fixture_0001_mandatory_perpetual_inventory` |

Phase 3 also extends criterion 8: every submit/cancel writes a
`command.submit-document` / `command.cancel-document` audit row (user + device
attributed) inside the same transaction as the posting.

### Shared fixture suite — one truth, two engines

`tests/fixtures/*.json` is the **language-neutral Dart ↔ Rust accounting
contract** (plan §5): each file is `{setup, actions, expect}`, driven here over
the command API + `MemStore` by `tests/fixtures.rs`. Fixture
`0001-mandatory-perpetual-inventory.json` is the mandatory acceptance scenario
verbatim; the Dart side runs the same scenario today as
`mercantis.hub.flutter/test/stock_cogs_acceptance_test.dart` (JSON-driving the
Dart engine from these files is future work). The rest: `0002` FIFO costing
incl. shortfall fallback, `0003` negative-stock rejection, `0004` adjustment
up/down, `0005` payment settlement + outstanding, `0006` period lock, `0007`
role rejection, `0008` value-neutral transfer.

Deliberate Phase 3 MVP bounds (deviations from the full Dart engine, all
flagged in the plan as later refinements): no UOM conversion at posting (line
qty is taken in stock units), no multi-currency base-amount stamping
(company-currency postings only), no customer/supplier/tax subledger rows
(GL + settlements carry party fields; the client derives its subledgers), no
receipt↔invoice line-linkage variance posting (the two-document flow clears
GRNI at matching values), and Delivery Note / POS Invoice doctypes arrive with
POS session close in Phase 6.

## Portal — customer / accountant links

The portal (`docs/NEURADIX_DOMAIN_AND_BRAND_ARCHITECTURE.md` §11) is served
under `portal.atlas.neuradix.app`; the paths below are host-agnostic and live
in this same binary (`src/portal.rs`).

### The `company_documents` read model

Drafts (quotations, unpaid-invoice metadata, customers, …) exist only as
mutations in the company log, so the portal renders from a **materialized
projection**: `(company_id, doctype, document_id)` → latest inner payload (the
row envelope's `payload`), child rows (`__children` → the `children` column),
docstatus and update time (`src/projection.rs`, table in `0003_portal.sql`).
Mutations fold in sync-version order with the client's semantics —
`createDocument`/`updateDocument`/`submitDocument`/`cancelDocument` upsert,
`deleteDocument` removes, a mutation without `__children` leaves stored
children intact. The projection is maintained **inside the same atomic step as
every log append** (client `sync/push` and the posting-commit replication path
share the stores' append), and `Store::rebuild_projection(company_id)` refolds
a company from scratch as a recovery/verification tool.

### Portal links (management plane, existing bearer auth)

Portal tokens are a **distinct token kind**: generated and stored hashed like
every other token but in their own `portal_links` table, so a portal token
never authenticates a member/device endpoint and member/device tokens 404 on
the portal plane.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/companies/{id}/portal-links` | **owner or admin** | `{kind: "customer"\|"accountant", party (required for customer), label, expires_days (default 90)}` → `{token, url_path, expiresAt}` |
| GET | `/companies/{id}/portal-links` | **owner or admin** | Link metadata + revoked state (tokens are never returned) |
| DELETE | `/companies/{id}/portal-links/{link_id}` | **owner or admin** | Revoke |

### Portal plane (the token in the path is the credential)

Unknown, expired and revoked tokens all read as **404**. Customer links are
scoped **strictly** to their customer: a document whose `customer` payload
field differs is a 404, never a 403 leak. `GET` endpoints on customer links
content-negotiate — `Accept: text/html` returns minimal server-rendered pages
(inline styles, no external assets, every interpolated value HTML-escaped);
JSON is the default.

| Method | Path | Link kind | Purpose |
|---|---|---|---|
| GET | `/portal/{token}` | customer | Summary: company name, customer id, open quotations (docstatus 0), unpaid invoices (submitted Sales Invoices with `outstanding_amount > 0`) |
| GET | `/portal/{token}/documents/{doctype}/{id}` | customer | Document payload + children (Quotation and Sales Invoice only) |
| POST | `/portal/{token}/quotations/{id}/accept` | customer | Appends a system mutation (device id `atlas-portal`, `updateDocument` row envelope) setting `accepted_on` to today; audit `portal.quote.accept`. Idempotent (repeat → 200, no new mutation); reject-after-accept → 409. HTML form posts redirect back to the document page |
| POST | `/portal/{token}/quotations/{id}/reject` | customer | Same, setting `rejected_on`; audit `portal.quote.reject`; accept-after-reject → 409 |
| GET | `/portal/{token}` | accountant | Summary: posted-document counts by doctype + GL entry count |
| GET | `/portal/{token}/gl.csv` | accountant | GL entries as CSV (`posting_date, voucher_type, voucher_no, account, debit, credit, party_type, party, is_reversal`), ordered by posting date then voucher |
| GET | `/portal/{token}/audit?limit=n` | accountant | Recent audit rows |

Payments are **not** part of the portal (that is the payment-links item;
invoice payment hands off to `pay.`, below).

## Payments — invoice pay links + Stripe webhook processing

Payment links (`docs/NEURADIX_DOMAIN_AND_BRAND_ARCHITECTURE.md` §12) are
served under `pay.atlas.neuradix.app`; the paths are host-agnostic and live in
this same binary (`src/pay.rs`).

### Pay links (management plane, existing bearer auth)

Pay tokens are a **distinct token kind** (their own `pay_links` table, hashed
at rest): a pay token never authenticates a member/device/portal endpoint and
other tokens never resolve on the pay plane.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| POST | `/companies/{id}/pay-links` | **owner, admin, sales or accountant** | `{invoice_id, expires_days (default 60)}` → `{token, url_path: "/pay/{token}", expiresAt}`. The invoice must exist as a **submitted Sales Invoice** (posted document or read model), 404 otherwise |
| GET | `/companies/{id}/pay-links` | same roles | Link metadata + revoked state (tokens are never returned) |
| DELETE | `/companies/{id}/pay-links/{link_id}` | same roles | Revoke |

### Pay page (the token in the path is the credential)

`GET /pay/{token}` — no other auth; unknown, expired and revoked tokens all
read as **404**. JSON by default; `Accept: text/html` returns a minimal
server-rendered page (portal renderer pattern: inline styles, no external
assets, every interpolated value HTML-escaped). Both views carry the company
name, invoice id + official number, posting date, line items, grand total, the
**live outstanding amount** (from the posted document's payload, maintained by
the posting engine on every settlement) and the payment state — outstanding 0
renders "Paid — thank you".

### Card payments without outbound HTTP

The backend never calls Stripe. The owner creates a **Stripe Payment Link**
in the Stripe dashboard and stores its URL in the company settings
(`stripe_payment_link_url`, on the settings GET/PUT whitelist). The pay page's
"Pay by card" button links there with `?client_reference_id={token}` appended;
Stripe echoes `client_reference_id` back in its webhook, which is how a
payment finds its invoice — payment state flows exclusively through the
already-public webhook intake, so the backend needs no Stripe API key and no
egress. When `stripe_payment_link_url` is unset the page renders the company's
`payment_instructions` settings text (bank transfer details etc., also
whitelisted) instead.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test          # 49 tests over MemStore (unit + API + fixtures + posting + replication + portal + payments); no DB required
```

Schema lives in `migrations/` (applied by `PgStore::connect` via embedded SQLx
migrations): `0001_init.sql` for the coordination plane (includes the
`mutations(company_id, sync_version)` index the pull path relies on),
`0002_postings.sql` for the posting authority (documents, numbering_series,
gl_entries, stock_ledger_entries, bins, settlements, posting_batches, items,
company_settings, idempotency_keys), `0003_portal.sql` for the portal
(portal_links, the company_documents read model) and `0004_payments.sql` for
the payment plane (pay_links).
