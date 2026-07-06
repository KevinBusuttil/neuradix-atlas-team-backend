-- Phase 3 — posting authority: official documents, gap-free numbering,
-- GL entries, stock ledger entries, derived bins, settlements, posting
-- batches, the per-company item registry, company posting settings and
-- idempotency keys. Monetary/qty columns are double precision, mirroring the
-- Dart engine's num semantics; posting dates are ISO YYYY-MM-DD strings.

create table company_settings (
    company_id uuid primary key references companies (id),
    settings   jsonb not null,
    updated_at timestamptz not null default now()
);

-- Minimal item registry: the posting engine needs the stock/service
-- distinction, valuation method and account overrides. Everything else about
-- an item lives on the sync plane.
create table items (
    company_id uuid not null references companies (id),
    id         text not null,
    item       jsonb not null,
    updated_at timestamptz not null default now(),
    primary key (company_id, id)
);

-- Official documents. docstatus: 1 submitted, 2 cancelled (drafts never land
-- here). official_number is allocated from numbering_series at submit.
create table documents (
    company_id      uuid not null references companies (id),
    doctype         text not null,
    id              text not null,
    payload         jsonb not null,
    docstatus       smallint not null check (docstatus in (1, 2)),
    official_number text,
    created_at      timestamptz not null default now(),
    primary key (company_id, doctype, id)
);

-- Strictly sequential, gap-free official-number series: next_value is the
-- last allocated value, bumped only inside a successful posting commit.
create table numbering_series (
    company_id uuid not null references companies (id),
    series_key text not null,
    next_value bigint not null default 0,
    primary key (company_id, series_key)
);

create table gl_entries (
    company_id   uuid not null references companies (id),
    id           text not null,
    account      text not null,
    debit        double precision not null,
    credit       double precision not null,
    party_type   text,
    party        text,
    voucher_type text not null,
    voucher_no   text not null,
    posting_date text not null,
    is_reversal  boolean not null,
    batch_id     text not null,
    primary key (company_id, id)
);

create index gl_entries_voucher_idx on gl_entries (company_id, voucher_no);
create index gl_entries_account_idx on gl_entries (company_id, account);

-- Append-only stock ledger. seq is the global insertion sequence; per-company
-- it is strictly increasing and is the chronological order the FIFO costing
-- replay depends on.
create table stock_ledger_entries (
    seq            bigint generated always as identity,
    company_id     uuid not null references companies (id),
    id             text not null,
    trans_type     text not null,
    item           text not null,
    warehouse      text not null,
    qty_change     double precision not null,
    valuation_rate double precision not null,
    voucher_type   text not null,
    voucher_no     text not null,
    posting_date   text not null,
    is_reversal    boolean not null,
    batch_id       text not null,
    primary key (company_id, id)
);

create index sle_pair_idx on stock_ledger_entries (company_id, item, warehouse, seq);
create index sle_voucher_idx on stock_ledger_entries (company_id, voucher_no);

-- Derived (item, warehouse) balances, recomputed and upserted inside every
-- posting commit that moves stock.
create table bins (
    company_id     uuid not null references companies (id),
    item           text not null,
    warehouse      text not null,
    actual_qty     double precision not null,
    valuation_rate double precision not null,
    stock_value    double precision not null,
    primary key (company_id, item, warehouse)
);

create table settlements (
    company_id           uuid not null references companies (id),
    id                   text not null,
    payment_voucher_type text not null,
    payment_voucher_no   text not null,
    invoice_voucher_type text not null,
    invoice_voucher_no   text not null,
    party_type           text not null,
    party                text,
    allocated_amount     double precision not null,
    posting_date         text not null,
    is_reversal          boolean not null,
    batch_id             text not null,
    primary key (company_id, id)
);

create index settlements_invoice_idx
    on settlements (company_id, invoice_voucher_type, invoice_voucher_no);
create index settlements_payment_idx on settlements (company_id, payment_voucher_no);

-- One row per atomic posting: PB-{document} on submit, PB-{document}-reversal
-- on cancel with reversal_of linking back.
create table posting_batches (
    company_id  uuid not null references companies (id),
    id          text not null,
    document_id text not null,
    doctype     text not null,
    kind        text not null check (kind in ('submit', 'cancel')),
    reversal_of text,
    created_at  timestamptz not null default now(),
    primary key (company_id, id)
);

-- Committed command responses, replayed verbatim when a client retries with
-- the same idempotency key.
create table idempotency_keys (
    company_id uuid not null references companies (id),
    key        text not null,
    response   jsonb not null,
    created_at timestamptz not null default now(),
    primary key (company_id, key)
);
