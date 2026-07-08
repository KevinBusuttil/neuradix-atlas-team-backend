-- Phase 3 refinement — customer/supplier/tax subledger rows, matching the
-- Dart `ledger_derivation.dart` row shapes: `Customer Transaction`
-- (`CT-{doc}`), `Supplier Transaction` (`VT-{doc}`) and `Tax Transaction`
-- (`TT-{doc}-{i}`). One shared party_transactions table carries both party
-- subledgers (kind picks the doctype and the party field name on the wire).
-- conversion_rate/base_amount carry the multi-currency base stamping the Dart
-- `_stampBaseAmounts` applies; single-currency rows post rate 1 and
-- base_amount == amount.

create table party_transactions (
    company_id      uuid not null references companies (id),
    id              text not null,
    kind            text not null check (kind in ('Customer', 'Supplier')),
    trans_type      text not null,
    party           text not null,
    posting_date    text not null,
    due_date        text,
    amount          double precision not null,
    currency        text,
    conversion_rate double precision not null,
    base_amount     double precision not null,
    voucher_type    text not null,
    voucher_no      text not null,
    is_reversal     boolean not null,
    batch_id        text not null,
    primary key (company_id, id)
);

create index party_txn_voucher_idx on party_transactions (company_id, voucher_no);
create index party_txn_party_idx on party_transactions (company_id, kind, party);

create table tax_transactions (
    company_id   uuid not null references companies (id),
    id           text not null,
    tax_type     text not null,
    tax          text,
    posting_date text not null,
    base_amount  double precision not null,
    tax_amount   double precision not null,
    rate         double precision not null,
    party_type   text not null,
    party        text,
    voucher_type text not null,
    voucher_no   text not null,
    is_reversal  boolean not null,
    batch_id     text not null,
    primary key (company_id, id)
);

create index tax_txn_voucher_idx on tax_transactions (company_id, voucher_no);
