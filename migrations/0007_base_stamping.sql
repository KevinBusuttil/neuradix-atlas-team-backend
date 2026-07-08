-- Phase 3 refinement — multi-currency base-amount stamping (the Dart
-- `_stampBaseAmounts`). Base-stamped vouchers (Sales/Purchase Invoice,
-- Payment Entry) carry `currency` + `conversion_rate` and every GL leg gets
-- base_debit/base_credit = amount x rate (full precision, so the base ledger
-- balances). Stock valuation legs always post conversion_rate 1 with base ==
-- amount. Null columns = a voucher posted before stamping / a non-stamped
-- doctype.

alter table gl_entries add column currency text;
alter table gl_entries add column conversion_rate double precision;
alter table gl_entries add column base_debit double precision;
alter table gl_entries add column base_credit double precision;
