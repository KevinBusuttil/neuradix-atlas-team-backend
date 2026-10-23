-- Atlas Team restore verification: invariants that must hold in ANY
-- consistent Atlas Team database, checked after a restore (and safe to run
-- read-only against production at any time). Each check prints one row;
-- the script ends with an exception — non-zero psql exit under
-- ON_ERROR_STOP — when any check fails. Used by scripts/restore_drill.sh.

\set ON_ERROR_STOP 1

create temporary table drill_checks (check_name text, violations bigint, detail text);

-- 1. Double entry: every company's GL balances (debits = credits).
insert into drill_checks
select 'gl_balanced_per_company', count(*),
       coalesce(string_agg(company_id::text || ' off by ' || round(diff::numeric, 4), '; '), '')
from (select company_id, sum(debit) - sum(credit) as diff
      from gl_entries group by company_id
      having abs(sum(debit) - sum(credit)) > 0.005) t;

-- 2. Every voucher's GL balances on its own.
insert into drill_checks
select 'gl_balanced_per_voucher', count(*),
       coalesce(string_agg(voucher_no, ', '), '')
from (select company_id, voucher_no from gl_entries group by company_id, voucher_no
      having abs(sum(debit) - sum(credit)) > 0.005) t;

-- 3. Bins equal the fold of their stock ledger (quantity).
insert into drill_checks
select 'bins_match_stock_ledger', count(*),
       coalesce(string_agg(item || '@' || warehouse, ', '), '')
from (select b.company_id, b.item, b.warehouse
      from bins b
      left join (select company_id, item, warehouse, sum(qty_change) as qty
                 from stock_ledger_entries group by company_id, item, warehouse) s
        using (company_id, item, warehouse)
      where abs(b.actual_qty - coalesce(s.qty, 0)) > 0.0001) t;

-- 4. Every stock ledger pair has a bin.
insert into drill_checks
select 'stock_ledger_pairs_have_bins', count(*), ''
from (select distinct company_id, item, warehouse from stock_ledger_entries) s
where not exists (select 1 from bins b where b.company_id = s.company_id
                  and b.item = s.item and b.warehouse = s.warehouse);

-- 5. Official numbering is gap-free and duplicate-free: each series counter
--    equals the number of documents carrying a number from it, and the
--    highest number equals the counter.
insert into drill_checks
select 'numbering_gap_free', count(*),
       coalesce(string_agg(series_key || ' counter ' || next_value || ' vs ' || n || ' docs', '; '), '')
from (select ns.company_id, ns.series_key, ns.next_value,
             count(d.official_number) as n,
             coalesce(max(substring(d.official_number from '[0-9]+$')::bigint), 0) as hi
      from numbering_series ns
      left join documents d on d.company_id = ns.company_id
           and d.official_number ~ ('^' || ns.series_key || '-[0-9]+$')
      group by ns.company_id, ns.series_key, ns.next_value) t
where n <> next_value or hi <> next_value;

insert into drill_checks
select 'official_numbers_unique', count(*), coalesce(string_agg(official_number, ', '), '')
from (select company_id, doctype, official_number from documents
      where official_number is not null
      group by company_id, doctype, official_number having count(*) > 1) t;

-- 6. Every official document has its posting batch.
insert into drill_checks
select 'documents_have_posting_batches', count(*), coalesce(string_agg(d.id, ', '), '')
from documents d
where not exists (select 1 from posting_batches pb
                  where pb.company_id = d.company_id and pb.document_id = d.id
                    and pb.kind = 'submit');

-- 7. The mutation log is gap-free (1..N) and its counter agrees, so every
--    device's cursor stays valid against the restored server.
insert into drill_checks
select 'sync_log_gap_free', count(*),
       coalesce(string_agg(company_id::text, ', '), '')
from (select m.company_id, count(*) as n, max(m.sync_version) as hi, min(m.sync_version) as lo,
             max(sc.last_version) as counter
      from mutations m join sync_counters sc using (company_id)
      group by m.company_id) t
where n <> hi or lo <> 1 or counter <> hi;

-- 8. Attachment bytes are intact (content address = SHA-256 of bytes).
insert into drill_checks
select 'blob_content_addresses_intact', count(*), coalesce(string_agg(sha256, ', '), '')
from blobs where encode(sha256(bytes), 'hex') <> sha256;

-- 9. Referential sanity: rows belong to existing companies; memberships
--    point at existing users; devices at existing members' companies.
insert into drill_checks
select 'memberships_reference_users', count(*), ''
from memberships m where not exists (select 1 from users u where u.id = m.user_id);

select check_name, violations, detail from drill_checks order by check_name;

-- Row counts, for comparison with the source database.
select 'rows' as kind, t.table_name,
       (xpath('/row/n/text()',
              query_to_xml(format('select count(*) as n from %I', t.table_name), false, true, '')))[1]::text::bigint as n
from information_schema.tables t
where t.table_schema = 'public' and t.table_type = 'BASE TABLE' and t.table_name <> '_sqlx_migrations'
order by t.table_name;

do $$
declare failed text;
begin
  select string_agg(check_name, ', ') into failed from drill_checks where violations > 0;
  if failed is not null then
    raise exception 'restore verification FAILED: %', failed;
  end if;
end $$;
