-- Portal plane: tokenized customer/accountant links and the materialized
-- document read model the portal renders from. `company_documents` is the
-- fold of the per-company mutation log (create/update/submit/cancel upsert,
-- delete removes; `__children` rows land in `children`), maintained inside
-- the same transaction as every log append and rebuildable from scratch.

create table company_documents (
    company_id  uuid not null references companies (id),
    doctype     text not null,
    document_id text not null,
    payload     jsonb not null,
    children    jsonb,
    docstatus   smallint not null default 0,
    updated_at  timestamptz not null default now(),
    primary key (company_id, doctype, document_id)
);

create index company_documents_doctype_idx
    on company_documents (company_id, doctype);

-- Portal links. The token is a distinct token kind: its SHA-256 hash lives
-- only here (never in user_tokens/devices), so a portal token cannot
-- authenticate member/device endpoints and vice versa.
create table portal_links (
    id         uuid primary key,
    company_id uuid not null references companies (id),
    kind       text not null check (kind in ('customer', 'accountant')),
    party      text,
    label      text,
    token_hash text not null unique,
    created_by uuid not null references users (id),
    created_at timestamptz not null default now(),
    expires_at timestamptz not null,
    revoked_at timestamptz
);

create index portal_links_company_idx on portal_links (company_id);
