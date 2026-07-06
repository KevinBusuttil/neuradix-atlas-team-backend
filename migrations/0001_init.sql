-- Atlas Team coordination backend — initial schema (Phase 2).
-- Identity, invitations, devices, tokens, mutation log, blobs, audit,
-- webhook intake. Posting-authority tables arrive with Phase 3.

create table companies (
    id          uuid primary key,
    name        text not null,
    created_at  timestamptz not null default now()
);

create table users (
    id           uuid primary key,
    email        text not null unique,
    display_name text not null,
    created_at   timestamptz not null default now()
);

create table memberships (
    user_id    uuid not null references users (id),
    company_id uuid not null references companies (id),
    role       text not null check (role in
        ('owner', 'admin', 'sales', 'purchasing', 'stock', 'pos', 'accountant', 'advisor')),
    created_at timestamptz not null default now(),
    primary key (user_id, company_id)
);

create table invitations (
    token       text primary key,
    company_id  uuid not null references companies (id),
    email       text not null,
    role        text not null check (role in
        ('owner', 'admin', 'sales', 'purchasing', 'stock', 'pos', 'accountant', 'advisor')),
    created_by  uuid not null references users (id),
    accepted_by uuid references users (id),
    created_at  timestamptz not null default now(),
    expires_at  timestamptz not null
);

create table devices (
    id         uuid primary key,
    company_id uuid not null references companies (id),
    user_id    uuid not null references users (id),
    name       text not null,
    token_hash text not null unique,
    created_at timestamptz not null default now(),
    revoked_at timestamptz
);

-- User (non-device) bearer tokens, e.g. the bootstrap owner token and the
-- token returned on invitation accept. Only SHA-256 hashes are stored.
create table user_tokens (
    token_hash text primary key,
    user_id    uuid not null references users (id),
    company_id uuid not null references companies (id),
    created_at timestamptz not null default now()
);

-- Per-company mutation log (replication plane). sync_version is assigned
-- server-side, monotonically increasing per company; `record` is the full
-- camelCase MutationRecord JSON as the client sent it.
create table mutations (
    company_id   uuid not null references companies (id),
    mutation_id  text not null,
    sync_version bigint not null,
    record       jsonb not null,
    acknowledged boolean not null default false,
    received_at  timestamptz not null default now(),
    primary key (company_id, mutation_id),
    unique (company_id, sync_version)
);

create index mutations_company_sync_version_idx
    on mutations (company_id, sync_version);

-- Row-per-company version counter, locked (select ... for update) during a
-- push so concurrent pushes serialize.
create table sync_counters (
    company_id   uuid primary key references companies (id),
    last_version bigint not null default 0
);

-- Content-addressed attachment bytes (lower-case hex SHA-256 of `bytes`).
create table blobs (
    company_id uuid not null references companies (id),
    sha256     text not null,
    bytes      bytea not null,
    created_at timestamptz not null default now(),
    primary key (company_id, sha256)
);

create table audit_log (
    id         uuid primary key,
    company_id uuid not null references companies (id),
    user_id    uuid references users (id),
    device_id  uuid references devices (id),
    action     text not null,
    detail     jsonb not null default '{}'::jsonb,
    at         timestamptz not null default now()
);

create index audit_log_company_at_idx on audit_log (company_id, at desc);

-- Raw webhook intake (payments / channel connectors). Verification and
-- processing are later phases; this table is the durable inbox.
create table webhook_events (
    id          uuid primary key,
    kind        text not null check (kind in ('payment', 'channel')),
    provider    text not null,
    headers     jsonb not null default '{}'::jsonb,
    body        bytea not null,
    received_at timestamptz not null default now()
);

create index webhook_events_received_at_idx on webhook_events (received_at desc);
