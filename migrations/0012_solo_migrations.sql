-- C3 — one-time Solo → Team take-on. A company receives at most one Solo
-- opening position, and only while it holds no official document. The row
-- is written inside the same posting transaction that posts the opening
-- entry, so a crash leaves either both or neither.
create table solo_migrations (
    company_id     uuid primary key references companies (id),
    migration_id   text not null,
    document_id    text not null,
    package_sha256 text not null,
    completed_at   timestamptz not null default now()
);
