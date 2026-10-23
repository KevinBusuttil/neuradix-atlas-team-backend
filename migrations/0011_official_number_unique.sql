-- Official numbers are unique per (company, doctype). Series allocation
-- already guarantees it for server-numbered documents; offline-fiscal POS
-- confirmations bring their own (till receipt) number, so the invariant is
-- now enforced by the schema as well as checked inside the posting commit.
create unique index documents_official_number_unique
    on documents (company_id, doctype, official_number)
    where official_number is not null;

-- Lookup for the one-device-per-till-series binding check.
create index documents_pos_till_series
    on documents (company_id, (payload->>'till_series'))
    where doctype = 'POS Invoice';
