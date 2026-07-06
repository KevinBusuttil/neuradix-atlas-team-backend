//! PostgreSQL [`Store`] implementation (SQLx, runtime queries only so the
//! build never needs a live database). Schema lives in `migrations/`.

use async_trait::async_trait;
use serde_json::{json, Value};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, MutationRecord, Role, TokenIdentity, User,
    WebhookEvent,
};
use crate::posting::model::{
    format_number, CommitOutcome, CompanySettings, GlEntry, Item, PostedDocument,
    PostingCommit, Settlement, StockLedgerEntry,
};

use super::{Store, StoreError};

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Connect and apply the embedded migrations.
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPool::connect(database_url).await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| StoreError::Internal(format!("migration failed: {e}")))?;
        Ok(Self::new(pool))
    }
}

fn parse_role(raw: &str) -> Result<Role, StoreError> {
    Role::parse(raw).ok_or_else(|| StoreError::Internal(format!("unknown role in db: {raw}")))
}

fn gl_from_row(row: &PgRow) -> Result<GlEntry, StoreError> {
    Ok(GlEntry {
        id: row.try_get("id")?,
        company_id: row.try_get("company_id")?,
        account: row.try_get("account")?,
        debit: row.try_get("debit")?,
        credit: row.try_get("credit")?,
        party_type: row.try_get("party_type")?,
        party: row.try_get("party")?,
        voucher_type: row.try_get("voucher_type")?,
        voucher_no: row.try_get("voucher_no")?,
        posting_date: row.try_get("posting_date")?,
        is_reversal: row.try_get("is_reversal")?,
        batch_id: row.try_get("batch_id")?,
    })
}

fn sle_from_row(row: &PgRow) -> Result<StockLedgerEntry, StoreError> {
    Ok(StockLedgerEntry {
        id: row.try_get("id")?,
        company_id: row.try_get("company_id")?,
        trans_type: row.try_get("trans_type")?,
        item: row.try_get("item")?,
        warehouse: row.try_get("warehouse")?,
        qty_change: row.try_get("qty_change")?,
        valuation_rate: row.try_get("valuation_rate")?,
        voucher_type: row.try_get("voucher_type")?,
        voucher_no: row.try_get("voucher_no")?,
        posting_date: row.try_get("posting_date")?,
        is_reversal: row.try_get("is_reversal")?,
        batch_id: row.try_get("batch_id")?,
        seq: row.try_get("seq")?,
    })
}

fn settlement_from_row(row: &PgRow) -> Result<Settlement, StoreError> {
    Ok(Settlement {
        id: row.try_get("id")?,
        company_id: row.try_get("company_id")?,
        payment_voucher_type: row.try_get("payment_voucher_type")?,
        payment_voucher_no: row.try_get("payment_voucher_no")?,
        invoice_voucher_type: row.try_get("invoice_voucher_type")?,
        invoice_voucher_no: row.try_get("invoice_voucher_no")?,
        party_type: row.try_get("party_type")?,
        party: row.try_get("party")?,
        allocated_amount: row.try_get("allocated_amount")?,
        posting_date: row.try_get("posting_date")?,
        is_reversal: row.try_get("is_reversal")?,
        batch_id: row.try_get("batch_id")?,
    })
}

fn document_from_row(row: &PgRow) -> Result<PostedDocument, StoreError> {
    Ok(PostedDocument {
        id: row.try_get("id")?,
        company_id: row.try_get("company_id")?,
        doctype: row.try_get("doctype")?,
        payload: row.try_get("payload")?,
        docstatus: row.try_get("docstatus")?,
        official_number: row.try_get("official_number")?,
        created_at: row.try_get("created_at")?,
    })
}

#[async_trait]
impl Store for PgStore {
    async fn create_company(&self, name: &str) -> Result<Company, StoreError> {
        let row = sqlx::query(
            "insert into companies (id, name) values ($1, $2) returning id, name, created_at",
        )
        .bind(Uuid::new_v4())
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok(Company {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            created_at: row.try_get("created_at")?,
        })
    }

    async fn upsert_user(&self, email: &str, display_name: &str) -> Result<User, StoreError> {
        // `do update set email = excluded.email` is a no-op write that makes
        // RETURNING yield the existing row (keeping its display name).
        let row = sqlx::query(
            "insert into users (id, email, display_name) values ($1, $2, $3) \
             on conflict (email) do update set email = excluded.email \
             returning id, email, display_name, created_at",
        )
        .bind(Uuid::new_v4())
        .bind(email)
        .bind(display_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(User {
            id: row.try_get("id")?,
            email: row.try_get("email")?,
            display_name: row.try_get("display_name")?,
            created_at: row.try_get("created_at")?,
        })
    }

    async fn upsert_membership(
        &self,
        user_id: Uuid,
        company_id: Uuid,
        role: Role,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into memberships (user_id, company_id, role) values ($1, $2, $3) \
             on conflict (user_id, company_id) do nothing",
        )
        .bind(user_id)
        .bind(company_id)
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn membership_role(
        &self,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<Option<Role>, StoreError> {
        let row =
            sqlx::query("select role from memberships where user_id = $1 and company_id = $2")
                .bind(user_id)
                .bind(company_id)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some(row) => Ok(Some(parse_role(
                row.try_get::<String, _>("role")?.as_str(),
            )?)),
            None => Ok(None),
        }
    }

    async fn insert_user_token(
        &self,
        token_hash: &str,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into user_tokens (token_hash, user_id, company_id) values ($1, $2, $3)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(company_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn resolve_token(&self, token_hash: &str) -> Result<Option<TokenIdentity>, StoreError> {
        let device = sqlx::query(
            "select id, user_id, company_id from devices \
             where token_hash = $1 and revoked_at is null",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = device {
            return Ok(Some(TokenIdentity {
                user_id: row.try_get("user_id")?,
                company_id: row.try_get("company_id")?,
                device_id: Some(row.try_get("id")?),
            }));
        }
        let user = sqlx::query("select user_id, company_id from user_tokens where token_hash = $1")
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match user {
            Some(row) => Some(TokenIdentity {
                user_id: row.try_get("user_id")?,
                company_id: row.try_get("company_id")?,
                device_id: None,
            }),
            None => None,
        })
    }

    async fn create_invitation(&self, invitation: Invitation) -> Result<(), StoreError> {
        sqlx::query(
            "insert into invitations \
             (token, company_id, email, role, created_by, accepted_by, created_at, expires_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&invitation.token)
        .bind(invitation.company_id)
        .bind(&invitation.email)
        .bind(invitation.role.as_str())
        .bind(invitation.created_by)
        .bind(invitation.accepted_by)
        .bind(invitation.created_at)
        .bind(invitation.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn invitation(&self, token: &str) -> Result<Option<Invitation>, StoreError> {
        let row = sqlx::query(
            "select token, company_id, email, role, created_by, accepted_by, created_at, \
             expires_at from invitations where token = $1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(Invitation {
                token: row.try_get("token")?,
                company_id: row.try_get("company_id")?,
                email: row.try_get("email")?,
                role: parse_role(row.try_get::<String, _>("role")?.as_str())?,
                created_by: row.try_get("created_by")?,
                accepted_by: row.try_get("accepted_by")?,
                created_at: row.try_get("created_at")?,
                expires_at: row.try_get("expires_at")?,
            })),
            None => Ok(None),
        }
    }

    async fn mark_invitation_accepted(&self, token: &str, user_id: Uuid) -> Result<(), StoreError> {
        sqlx::query("update invitations set accepted_by = $2 where token = $1")
            .bind(token)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn create_device(&self, device: Device) -> Result<(), StoreError> {
        sqlx::query(
            "insert into devices \
             (id, company_id, user_id, name, token_hash, created_at, revoked_at) \
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(device.id)
        .bind(device.company_id)
        .bind(device.user_id)
        .bind(&device.name)
        .bind(&device.token_hash)
        .bind(device.created_at)
        .bind(device.revoked_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn push_mutations(
        &self,
        company_id: Uuid,
        mutations: Vec<MutationRecord>,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let mut tx = self.pool.begin().await?;
        // Per-company counter row, locked for the duration of the push so
        // concurrent pushes serialize and versions stay gap-free-monotonic.
        sqlx::query(
            "insert into sync_counters (company_id, last_version) values ($1, 0) \
             on conflict (company_id) do nothing",
        )
        .bind(company_id)
        .execute(&mut *tx)
        .await?;
        let row =
            sqlx::query("select last_version from sync_counters where company_id = $1 for update")
                .bind(company_id)
                .fetch_one(&mut *tx)
                .await?;
        let mut last: i64 = row.try_get("last_version")?;

        let mut versions = Vec::with_capacity(mutations.len());
        for record in mutations {
            let existing = sqlx::query(
                "select sync_version from mutations where company_id = $1 and mutation_id = $2",
            )
            .bind(company_id)
            .bind(&record.id)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(row) = existing {
                versions.push((record.id, row.try_get("sync_version")?));
                continue;
            }
            last += 1;
            let json = serde_json::to_value(&record)?;
            sqlx::query(
                "insert into mutations (company_id, mutation_id, sync_version, record) \
                 values ($1, $2, $3, $4)",
            )
            .bind(company_id)
            .bind(&record.id)
            .bind(last)
            .bind(json)
            .execute(&mut *tx)
            .await?;
            versions.push((record.id, last));
        }
        sqlx::query("update sync_counters set last_version = $2 where company_id = $1")
            .bind(company_id)
            .bind(last)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(versions)
    }

    async fn pull_mutations(
        &self,
        company_id: Uuid,
        after: i64,
    ) -> Result<Vec<MutationRecord>, StoreError> {
        let rows = sqlx::query(
            "select sync_version, record from mutations \
             where company_id = $1 and sync_version > $2 order by sync_version",
        )
        .bind(company_id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let version: i64 = row.try_get("sync_version")?;
            let json: serde_json::Value = row.try_get("record")?;
            let mut record: MutationRecord = serde_json::from_value(json)?;
            record.sync_version = Some(version.to_string());
            out.push(record);
        }
        Ok(out)
    }

    async fn ack_mutations(&self, company_id: Uuid, ids: &[String]) -> Result<u64, StoreError> {
        let result = sqlx::query(
            "update mutations set acknowledged = true \
             where company_id = $1 and mutation_id = any($2) and not acknowledged",
        )
        .bind(company_id)
        .bind(ids)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn put_blob(
        &self,
        company_id: Uuid,
        sha256: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into blobs (company_id, sha256, bytes) values ($1, $2, $3) \
             on conflict (company_id, sha256) do nothing",
        )
        .bind(company_id)
        .bind(sha256)
        .bind(bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_blob(
        &self,
        company_id: Uuid,
        sha256: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let row = sqlx::query("select bytes from blobs where company_id = $1 and sha256 = $2")
            .bind(company_id)
            .bind(sha256)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(row) => Some(row.try_get("bytes")?),
            None => None,
        })
    }

    async fn has_blob(&self, company_id: Uuid, sha256: &str) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "select exists(select 1 from blobs where company_id = $1 and sha256 = $2) as present",
        )
        .bind(company_id)
        .bind(sha256)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("present")?)
    }

    async fn append_audit(&self, entry: AuditEntry) -> Result<(), StoreError> {
        sqlx::query(
            "insert into audit_log (id, company_id, user_id, device_id, action, detail, at) \
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(entry.id)
        .bind(entry.company_id)
        .bind(entry.user_id)
        .bind(entry.device_id)
        .bind(&entry.action)
        .bind(&entry.detail)
        .bind(entry.at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn recent_audit(
        &self,
        company_id: Uuid,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        let rows = sqlx::query(
            "select id, company_id, user_id, device_id, action, detail, at \
             from audit_log where company_id = $1 order by at desc, id limit $2",
        )
        .bind(company_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(AuditEntry {
                id: row.try_get("id")?,
                company_id: row.try_get("company_id")?,
                user_id: row.try_get("user_id")?,
                device_id: row.try_get("device_id")?,
                action: row.try_get("action")?,
                detail: row.try_get("detail")?,
                at: row.try_get("at")?,
            });
        }
        Ok(out)
    }

    async fn insert_webhook_event(&self, event: WebhookEvent) -> Result<(), StoreError> {
        sqlx::query(
            "insert into webhook_events (id, kind, provider, headers, body, received_at) \
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(event.id)
        .bind(event.kind.as_str())
        .bind(&event.provider)
        .bind(&event.headers)
        .bind(&event.body)
        .bind(event.received_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Posting authority (Phase 3)
    // ------------------------------------------------------------------

    async fn company_settings(&self, company_id: Uuid) -> Result<CompanySettings, StoreError> {
        let row = sqlx::query("select settings from company_settings where company_id = $1")
            .bind(company_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => {
                let value: Value = row.try_get("settings")?;
                Ok(serde_json::from_value(value)?)
            }
            None => Ok(CompanySettings::default()),
        }
    }

    async fn put_company_settings(
        &self,
        company_id: Uuid,
        settings: CompanySettings,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "insert into company_settings (company_id, settings, updated_at) \
             values ($1, $2, now()) \
             on conflict (company_id) do update \
             set settings = excluded.settings, updated_at = now()",
        )
        .bind(company_id)
        .bind(serde_json::to_value(&settings)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn upsert_item(&self, company_id: Uuid, item: Item) -> Result<(), StoreError> {
        sqlx::query(
            "insert into items (company_id, id, item, updated_at) values ($1, $2, $3, now()) \
             on conflict (company_id, id) do update \
             set item = excluded.item, updated_at = now()",
        )
        .bind(company_id)
        .bind(&item.id)
        .bind(serde_json::to_value(&item)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn items(&self, company_id: Uuid, ids: &[String]) -> Result<Vec<Item>, StoreError> {
        let rows = sqlx::query("select item from items where company_id = $1 and id = any($2)")
            .bind(company_id)
            .bind(ids)
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let value: Value = row.try_get("item")?;
            out.push(serde_json::from_value(value)?);
        }
        Ok(out)
    }

    async fn posted_document(
        &self,
        company_id: Uuid,
        doctype: &str,
        id: &str,
    ) -> Result<Option<PostedDocument>, StoreError> {
        let row = sqlx::query(
            "select company_id, doctype, id, payload, docstatus, official_number, created_at \
             from documents where company_id = $1 and doctype = $2 and id = $3",
        )
        .bind(company_id)
        .bind(doctype)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| document_from_row(&row)).transpose()
    }

    async fn sles_for_pair(
        &self,
        company_id: Uuid,
        item: &str,
        warehouse: &str,
    ) -> Result<Vec<StockLedgerEntry>, StoreError> {
        let rows = sqlx::query(
            "select seq, company_id, id, trans_type, item, warehouse, qty_change, \
             valuation_rate, voucher_type, voucher_no, posting_date, is_reversal, batch_id \
             from stock_ledger_entries \
             where company_id = $1 and item = $2 and warehouse = $3 order by seq",
        )
        .bind(company_id)
        .bind(item)
        .bind(warehouse)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(sle_from_row).collect()
    }

    async fn sles_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<StockLedgerEntry>, StoreError> {
        let rows = sqlx::query(
            "select seq, company_id, id, trans_type, item, warehouse, qty_change, \
             valuation_rate, voucher_type, voucher_no, posting_date, is_reversal, batch_id \
             from stock_ledger_entries \
             where company_id = $1 and voucher_no = $2 order by seq",
        )
        .bind(company_id)
        .bind(voucher_no)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(sle_from_row).collect()
    }

    async fn gl_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<GlEntry>, StoreError> {
        let rows = sqlx::query(
            "select company_id, id, account, debit, credit, party_type, party, voucher_type, \
             voucher_no, posting_date, is_reversal, batch_id \
             from gl_entries where company_id = $1 and voucher_no = $2 order by id",
        )
        .bind(company_id)
        .bind(voucher_no)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(gl_from_row).collect()
    }

    async fn settlements_for_invoice(
        &self,
        company_id: Uuid,
        invoice_doctype: &str,
        invoice_no: &str,
    ) -> Result<Vec<Settlement>, StoreError> {
        let rows = sqlx::query(
            "select company_id, id, payment_voucher_type, payment_voucher_no, \
             invoice_voucher_type, invoice_voucher_no, party_type, party, allocated_amount, \
             posting_date, is_reversal, batch_id \
             from settlements \
             where company_id = $1 and invoice_voucher_type = $2 and invoice_voucher_no = $3",
        )
        .bind(company_id)
        .bind(invoice_doctype)
        .bind(invoice_no)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(settlement_from_row).collect()
    }

    async fn settlements_for_payment(
        &self,
        company_id: Uuid,
        payment_no: &str,
    ) -> Result<Vec<Settlement>, StoreError> {
        let rows = sqlx::query(
            "select company_id, id, payment_voucher_type, payment_voucher_no, \
             invoice_voucher_type, invoice_voucher_no, party_type, party, allocated_amount, \
             posting_date, is_reversal, batch_id \
             from settlements where company_id = $1 and payment_voucher_no = $2",
        )
        .bind(company_id)
        .bind(payment_no)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(settlement_from_row).collect()
    }

    async fn idempotent_response(
        &self,
        company_id: Uuid,
        key: &str,
    ) -> Result<Option<Value>, StoreError> {
        let row =
            sqlx::query("select response from idempotency_keys where company_id = $1 and key = $2")
                .bind(company_id)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match row {
            Some(row) => Some(row.try_get("response")?),
            None => None,
        })
    }

    async fn posting_commit(&self, commit: PostingCommit) -> Result<CommitOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        let company = commit.company_id;

        // Serialize all posting commits per company (the MemStore analogue is
        // its single mutex): numbering stays gap-free and the stock-ledger
        // expectation counts below can't race a concurrent commit.
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1::text, 42))")
            .bind(company)
            .execute(&mut *tx)
            .await?;

        // Idempotency replay.
        if let Some(key) = &commit.idempotency_key {
            let row = sqlx::query(
                "select response from idempotency_keys where company_id = $1 and key = $2",
            )
            .bind(company)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(row) = row {
                return Ok(CommitOutcome {
                    response: row.try_get("response")?,
                    replayed: true,
                });
            }
        }

        // Optimistic stock-ledger expectations.
        for (item, warehouse, expected) in &commit.sle_expectations {
            let row = sqlx::query(
                "select count(*) as n from stock_ledger_entries \
                 where company_id = $1 and item = $2 and warehouse = $3",
            )
            .bind(company)
            .bind(item)
            .bind(warehouse)
            .fetch_one(&mut *tx)
            .await?;
            let actual: i64 = row.try_get("n")?;
            if actual != *expected as i64 {
                return Err(StoreError::Stale(format!(
                    "stock ledger for {item}/{warehouse} moved ({expected} -> {actual})"
                )));
            }
        }

        let mut document = commit.document;
        let existing = sqlx::query(
            "select docstatus from documents where company_id = $1 and doctype = $2 and id = $3",
        )
        .bind(company)
        .bind(&document.doctype)
        .bind(&document.id)
        .fetch_optional(&mut *tx)
        .await?;
        if commit.document_is_new {
            if existing.is_some() {
                return Err(StoreError::Conflict(format!(
                    "document {} {} already exists",
                    document.doctype, document.id
                )));
            }
        } else {
            let Some(row) = existing else {
                return Err(StoreError::Conflict(format!(
                    "document {} {} does not exist",
                    document.doctype, document.id
                )));
            };
            let docstatus: i16 = row.try_get("docstatus")?;
            if docstatus != 1 {
                return Err(StoreError::Conflict(format!(
                    "document {} {} is not submitted",
                    document.doctype, document.id
                )));
            }
        }

        // Gap-free number allocation inside the transaction.
        let mut response = commit.response;
        if let Some(series) = &commit.series_key {
            sqlx::query(
                "insert into numbering_series (company_id, series_key, next_value) \
                 values ($1, $2, 0) on conflict (company_id, series_key) do nothing",
            )
            .bind(company)
            .bind(series)
            .execute(&mut *tx)
            .await?;
            let row = sqlx::query(
                "update numbering_series set next_value = next_value + 1 \
                 where company_id = $1 and series_key = $2 returning next_value",
            )
            .bind(company)
            .bind(series)
            .fetch_one(&mut *tx)
            .await?;
            let value: i64 = row.try_get("next_value")?;
            let number = format_number(series, value);
            document.official_number = Some(number.clone());
            response["number"] = json!(number);
        }

        if commit.document_is_new {
            sqlx::query(
                "insert into documents \
                 (company_id, doctype, id, payload, docstatus, official_number, created_at) \
                 values ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(company)
            .bind(&document.doctype)
            .bind(&document.id)
            .bind(&document.payload)
            .bind(document.docstatus)
            .bind(&document.official_number)
            .bind(document.created_at)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "update documents set payload = $4, docstatus = $5 \
                 where company_id = $1 and doctype = $2 and id = $3",
            )
            .bind(company)
            .bind(&document.doctype)
            .bind(&document.id)
            .bind(&document.payload)
            .bind(document.docstatus)
            .execute(&mut *tx)
            .await?;
        }

        for entry in &commit.gl_entries {
            sqlx::query(
                "insert into gl_entries \
                 (company_id, id, account, debit, credit, party_type, party, voucher_type, \
                  voucher_no, posting_date, is_reversal, batch_id) \
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(company)
            .bind(&entry.id)
            .bind(&entry.account)
            .bind(entry.debit)
            .bind(entry.credit)
            .bind(&entry.party_type)
            .bind(&entry.party)
            .bind(&entry.voucher_type)
            .bind(&entry.voucher_no)
            .bind(&entry.posting_date)
            .bind(entry.is_reversal)
            .bind(&entry.batch_id)
            .execute(&mut *tx)
            .await?;
        }

        for sle in &commit.stock_ledger_entries {
            sqlx::query(
                "insert into stock_ledger_entries \
                 (company_id, id, trans_type, item, warehouse, qty_change, valuation_rate, \
                  voucher_type, voucher_no, posting_date, is_reversal, batch_id) \
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(company)
            .bind(&sle.id)
            .bind(&sle.trans_type)
            .bind(&sle.item)
            .bind(&sle.warehouse)
            .bind(sle.qty_change)
            .bind(sle.valuation_rate)
            .bind(&sle.voucher_type)
            .bind(&sle.voucher_no)
            .bind(&sle.posting_date)
            .bind(sle.is_reversal)
            .bind(&sle.batch_id)
            .execute(&mut *tx)
            .await?;
        }

        for settlement in &commit.settlements {
            sqlx::query(
                "insert into settlements \
                 (company_id, id, payment_voucher_type, payment_voucher_no, \
                  invoice_voucher_type, invoice_voucher_no, party_type, party, \
                  allocated_amount, posting_date, is_reversal, batch_id) \
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            )
            .bind(company)
            .bind(&settlement.id)
            .bind(&settlement.payment_voucher_type)
            .bind(&settlement.payment_voucher_no)
            .bind(&settlement.invoice_voucher_type)
            .bind(&settlement.invoice_voucher_no)
            .bind(&settlement.party_type)
            .bind(&settlement.party)
            .bind(settlement.allocated_amount)
            .bind(&settlement.posting_date)
            .bind(settlement.is_reversal)
            .bind(&settlement.batch_id)
            .execute(&mut *tx)
            .await?;
        }

        for bin in &commit.bins {
            sqlx::query(
                "insert into bins \
                 (company_id, item, warehouse, actual_qty, valuation_rate, stock_value) \
                 values ($1, $2, $3, $4, $5, $6) \
                 on conflict (company_id, item, warehouse) do update set \
                 actual_qty = excluded.actual_qty, valuation_rate = excluded.valuation_rate, \
                 stock_value = excluded.stock_value",
            )
            .bind(company)
            .bind(&bin.item)
            .bind(&bin.warehouse)
            .bind(bin.actual_qty)
            .bind(bin.valuation_rate)
            .bind(bin.stock_value)
            .execute(&mut *tx)
            .await?;
        }

        for (doctype, id, outstanding) in &commit.outstanding_updates {
            sqlx::query(
                "update documents \
                 set payload = jsonb_set(payload, '{outstanding_amount}', to_jsonb($4::float8)) \
                 where company_id = $1 and doctype = $2 and id = $3",
            )
            .bind(company)
            .bind(doctype)
            .bind(id)
            .bind(outstanding)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "insert into posting_batches \
             (company_id, id, document_id, doctype, kind, reversal_of, created_at) \
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(company)
        .bind(&commit.batch.id)
        .bind(&commit.batch.document_id)
        .bind(&commit.batch.doctype)
        .bind(&commit.batch.kind)
        .bind(&commit.batch.reversal_of)
        .bind(commit.batch.created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "insert into audit_log (id, company_id, user_id, device_id, action, detail, at) \
             values ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(commit.audit.id)
        .bind(commit.audit.company_id)
        .bind(commit.audit.user_id)
        .bind(commit.audit.device_id)
        .bind(&commit.audit.action)
        .bind(&commit.audit.detail)
        .bind(commit.audit.at)
        .execute(&mut *tx)
        .await?;

        if let Some(key) = &commit.idempotency_key {
            sqlx::query(
                "insert into idempotency_keys (company_id, key, response) values ($1, $2, $3)",
            )
            .bind(company)
            .bind(key)
            .bind(&response)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(CommitOutcome {
            response,
            replayed: false,
        })
    }
}
