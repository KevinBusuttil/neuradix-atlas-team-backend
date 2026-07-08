//! Persistence boundary. Everything the HTTP layer needs is behind the
//! [`Store`] trait so the service runs identically over the in-memory store
//! (`--mem` dev mode, test suite) and Postgres (production, `DATABASE_URL`).

pub mod mem;
pub mod pg;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, MutationRecord, PayLink, PortalLink, Role,
    TokenIdentity, User, WebhookEvent,
};
use crate::posting::model::{
    CommitOutcome, CompanySettings, GlEntry, Item, PostedDocument, PostingCommit, Settlement,
    StockLedgerEntry,
};
use crate::projection::CompanyDocument;

pub use mem::MemStore;
pub use pg::PgStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A uniqueness / state conflict the caller may surface as 409.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Optimistic-concurrency failure inside a posting commit: the state the
    /// engine computed from moved underneath it. The command layer retries.
    #[error("stale: {0}")]
    Stale(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Internal(String),
}

#[async_trait]
pub trait Store: Send + Sync {
    // Identity
    async fn create_company(&self, name: &str) -> Result<Company, StoreError>;
    async fn company(&self, company_id: Uuid) -> Result<Option<Company>, StoreError>;
    /// Get-or-create by unique email. An existing user's display name is kept.
    async fn upsert_user(&self, email: &str, display_name: &str) -> Result<User, StoreError>;
    /// Idempotent: an existing membership keeps its original role.
    async fn upsert_membership(
        &self,
        user_id: Uuid,
        company_id: Uuid,
        role: Role,
    ) -> Result<(), StoreError>;
    async fn membership_role(
        &self,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<Option<Role>, StoreError>;

    // Tokens (opaque bearer tokens; only SHA-256 hashes are stored)
    async fn insert_user_token(
        &self,
        token_hash: &str,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), StoreError>;
    async fn resolve_token(&self, token_hash: &str) -> Result<Option<TokenIdentity>, StoreError>;

    // Invitations
    async fn create_invitation(&self, invitation: Invitation) -> Result<(), StoreError>;
    async fn invitation(&self, token: &str) -> Result<Option<Invitation>, StoreError>;
    async fn mark_invitation_accepted(&self, token: &str, user_id: Uuid) -> Result<(), StoreError>;

    // Devices
    async fn create_device(&self, device: Device) -> Result<(), StoreError>;

    // Mutation log (per-company, server-assigned monotonic sync versions)
    /// Assigns the next sync versions; idempotent on mutation id — a re-pushed
    /// id gets its previously assigned version back. Returns `(id, version)`
    /// pairs in input order.
    async fn push_mutations(
        &self,
        company_id: Uuid,
        mutations: Vec<MutationRecord>,
    ) -> Result<Vec<(String, i64)>, StoreError>;
    /// Mutations with version > `after`, ordered by version ascending, with
    /// `sync_version` set on each returned record.
    async fn pull_mutations(
        &self,
        company_id: Uuid,
        after: i64,
    ) -> Result<Vec<MutationRecord>, StoreError>;
    /// Marks the given mutation ids acknowledged; returns how many matched.
    async fn ack_mutations(&self, company_id: Uuid, ids: &[String]) -> Result<u64, StoreError>;

    // Blobs (content-addressed by lower-case hex SHA-256, per company)
    async fn put_blob(
        &self,
        company_id: Uuid,
        sha256: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StoreError>;
    async fn get_blob(&self, company_id: Uuid, sha256: &str)
        -> Result<Option<Vec<u8>>, StoreError>;
    async fn has_blob(&self, company_id: Uuid, sha256: &str) -> Result<bool, StoreError>;

    // Audit
    async fn append_audit(&self, entry: AuditEntry) -> Result<(), StoreError>;
    /// Most recent entries first.
    async fn recent_audit(
        &self,
        company_id: Uuid,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, StoreError>;

    // Webhook intake
    async fn insert_webhook_event(&self, event: WebhookEvent) -> Result<(), StoreError>;

    // ------------------------------------------------------------------
    // Posting authority (Phase 3)
    // ------------------------------------------------------------------

    /// Company posting settings; defaults when the company never stored any.
    async fn company_settings(&self, company_id: Uuid) -> Result<CompanySettings, StoreError>;
    async fn put_company_settings(
        &self,
        company_id: Uuid,
        settings: CompanySettings,
    ) -> Result<(), StoreError>;

    /// Item registry upsert (last write wins on the whole record).
    async fn upsert_item(&self, company_id: Uuid, item: Item) -> Result<(), StoreError>;
    /// Registry entries for the given ids; unknown ids are simply absent
    /// (the engine treats unregistered items as stock items, like Dart).
    async fn items(&self, company_id: Uuid, ids: &[String]) -> Result<Vec<Item>, StoreError>;

    /// An official document by (doctype, id), if it was ever submitted.
    async fn posted_document(
        &self,
        company_id: Uuid,
        doctype: &str,
        id: &str,
    ) -> Result<Option<PostedDocument>, StoreError>;

    /// Full stock ledger for an (item, warehouse) pair, oldest first (by
    /// insertion sequence — the FIFO replay depends on this order).
    async fn sles_for_pair(
        &self,
        company_id: Uuid,
        item: &str,
        warehouse: &str,
    ) -> Result<Vec<StockLedgerEntry>, StoreError>;
    /// All stock ledger rows a voucher produced (submit and reversal rows).
    async fn sles_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<StockLedgerEntry>, StoreError>;
    /// All GL rows a voucher produced (submit and reversal rows).
    async fn gl_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<GlEntry>, StoreError>;
    /// All settlements allocated against an invoice (signed; reversals net).
    async fn settlements_for_invoice(
        &self,
        company_id: Uuid,
        invoice_doctype: &str,
        invoice_no: &str,
    ) -> Result<Vec<Settlement>, StoreError>;
    /// All settlements a payment voucher produced.
    async fn settlements_for_payment(
        &self,
        company_id: Uuid,
        payment_no: &str,
    ) -> Result<Vec<Settlement>, StoreError>;

    /// A previously committed response for this idempotency key, if any.
    async fn idempotent_response(
        &self,
        company_id: Uuid,
        key: &str,
    ) -> Result<Option<Value>, StoreError>;

    /// Applies one posting command atomically: idempotency-key replay check,
    /// stock-ledger expectations (`StoreError::Stale` on mismatch), gap-free
    /// official-number allocation, document insert/update, GL + SLE +
    /// settlement appends, bin upserts, invoice outstanding maintenance, the
    /// posting batch and the audit row — all or nothing.
    async fn posting_commit(&self, commit: PostingCommit) -> Result<CommitOutcome, StoreError>;

    // ------------------------------------------------------------------
    // Portal (links + materialized document read model)
    // ------------------------------------------------------------------

    async fn create_portal_link(&self, link: PortalLink) -> Result<(), StoreError>;
    /// All links of a company (metadata; only token hashes are stored).
    async fn portal_links(&self, company_id: Uuid) -> Result<Vec<PortalLink>, StoreError>;
    /// Marks a link revoked; idempotent. Returns false when the link does not
    /// belong to the company.
    async fn revoke_portal_link(&self, company_id: Uuid, link_id: Uuid)
        -> Result<bool, StoreError>;
    /// Resolves a portal token hash — and only a portal token hash: member /
    /// device tokens live in different tables and never match here.
    async fn portal_link_by_hash(&self, token_hash: &str)
        -> Result<Option<PortalLink>, StoreError>;

    // ------------------------------------------------------------------
    // Pay links (invoice payment plane)
    // ------------------------------------------------------------------

    async fn create_pay_link(&self, link: PayLink) -> Result<(), StoreError>;
    /// All pay links of a company (metadata; only token hashes are stored).
    async fn pay_links(&self, company_id: Uuid) -> Result<Vec<PayLink>, StoreError>;
    /// Marks a pay link revoked; idempotent. Returns false when the link does
    /// not belong to the company.
    async fn revoke_pay_link(&self, company_id: Uuid, link_id: Uuid) -> Result<bool, StoreError>;
    /// Resolves a pay token hash — and only a pay token hash: member /
    /// device / portal tokens live in different tables and never match here.
    async fn pay_link_by_hash(&self, token_hash: &str) -> Result<Option<PayLink>, StoreError>;

    /// One row of the materialized document read model.
    async fn company_document(
        &self,
        company_id: Uuid,
        doctype: &str,
        document_id: &str,
    ) -> Result<Option<CompanyDocument>, StoreError>;
    /// All read-model rows of a doctype, ordered by document id.
    async fn company_documents(
        &self,
        company_id: Uuid,
        doctype: &str,
    ) -> Result<Vec<CompanyDocument>, StoreError>;
    /// Recovery / verification tool: drops the company's projection and
    /// refolds it from the mutation log in sync-version order.
    async fn rebuild_projection(&self, company_id: Uuid) -> Result<(), StoreError>;

    /// Posted (official) document counts per doctype, for the accountant
    /// portal summary.
    async fn posted_document_counts(
        &self,
        company_id: Uuid,
    ) -> Result<Vec<(String, i64)>, StoreError>;
    /// Every GL entry of the company, ordered by posting date then voucher
    /// (then row id) — the accountant portal's GL export order.
    async fn gl_entries_ordered(&self, company_id: Uuid) -> Result<Vec<GlEntry>, StoreError>;
}
