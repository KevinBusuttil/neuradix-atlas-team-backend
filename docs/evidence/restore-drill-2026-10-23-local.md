# Atlas Team restore drill — 20261023T050622Z

- Backup: `/tmp/claude-0/backups/atlas-20261023-050622.dump.age` (66649 bytes)
- Host: Claude Code cloud container (Ubuntu 24.04, PostgreSQL 16.15) · isolation: initdb PostgreSQL instance on port 26167

```
[drill] checksum OK
[drill] decrypted with the age identity
[drill] archive lists cleanly
[drill] fresh PostgreSQL 16.15 (Ubuntu 16.15-0ubuntu0.24.04.1) started (initdb)
[drill] restored in 1s
           check_name           | violations | detail 
--------------------------------+------------+--------
 bins_match_stock_ledger        |          0 | 
 blob_content_addresses_intact  |          0 | 
 documents_have_posting_batches |          0 | 
 gl_balanced_per_company        |          0 | 
 gl_balanced_per_voucher        |          0 | 
 memberships_reference_users    |          0 | 
 numbering_gap_free             |          0 | 
 official_numbers_unique        |          0 | 
 stock_ledger_pairs_have_bins   |          0 | 
 sync_log_gap_free              |          0 | 
(10 rows)

 kind |      table_name      | n  
------+----------------------+----
 rows | audit_log            | 28
 rows | bins                 |  1
 rows | blobs                |  1
 rows | companies            |  1
 rows | company_documents    | 42
 rows | company_settings     |  0
 rows | devices              |  6
 rows | documents            |  7
 rows | gl_entries           | 19
 rows | idempotency_keys     |  8
 rows | invitations          |  5
 rows | items                |  1
 rows | memberships          |  6
 rows | mutations            | 48
 rows | numbering_series     |  6
 rows | party_transactions   |  3
 rows | pay_links            |  0
 rows | portal_links         |  0
 rows | posting_batches      |  8
 rows | settlements          |  1
 rows | stock_ledger_entries |  5
 rows | sync_counters        |  1
 rows | tax_transactions     |  1
 rows | user_tokens          |  6
 rows | users                |  6
 rows | webhook_events       |  0
(26 rows)

[drill] invariants hold
[drill] row counts identical to the source in all 27 tables
[drill] backend ready on the restored database: {"schemaVersion":11,"status":"ready","version":"0.1.0"}
[drill] full resync through the restored API: 48/48 mutations, no duplicates
```

**Result: PASS**
