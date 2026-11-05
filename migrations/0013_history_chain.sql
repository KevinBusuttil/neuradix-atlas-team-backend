-- C4: server-history identity. Each log row carries its hash-chain link
-- (see src/history.rs); sync_counters keeps the newest link so an append
-- extends the chain without rereading the log. Both are filled lazily for
-- rows written before this migration.
alter table mutations add column chain_hash bytea;
alter table sync_counters add column last_hash bytea;

-- What recovering devices reported missing after a restore. One row per
-- (company, recovery id): a retried report replays the stored response.
create table history_recoveries (
    company_id  uuid not null references companies (id),
    recovery_id text not null,
    device_id   uuid,
    report      jsonb not null,
    response    jsonb not null,
    reported_at timestamptz not null default now(),
    primary key (company_id, recovery_id)
);
