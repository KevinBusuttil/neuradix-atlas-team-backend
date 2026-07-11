//! Persistence boundary. Everything the HTTP layer needs is behind the
//! [`Store`] trait so the service runs identically over the in-memory store
//! (`--mem` dev mode, test suite) and Postgres (production, `DATABASE_URL`).

pub mod mem;
pub mod pg;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, Member, MutationRecord, PayLink, PortalLink, Role,
    TokenIdentity, User, WebhookEvent,
};
use crate::posting::model::{
    Bin, CommitOutcome, CompanySettings, GlEntry, Item, PartyTransaction, PostedDocument,
    PostingCommit, Settlement, StockLedgerEntry, TaxTransaction,
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

/// One page of a paginated mutation pull.
#[derive(Debug)]
pub struct MutationPage {
    /// At most the requested `limit` records, sync-version ascending, with
    /// `sync_version` set on each.
    pub mutations: Vec<MutationRecord>,
    /// Whether mutations exist past the last returned version. Exact — never
    /// a heuristic — so clients can stop looping the moment it is `false`.
    pub has_more: bool,
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
    /// All members of a company joined with their user records, oldest
    /// membership first.
    async fn company_members(&self, company_id: Uuid) -> Result<Vec<Member>, StoreError>;
    /// Deletes a membership. Returns `false` when the user was not a member.
    async fn remove_membership(&self, user_id: Uuid, company_id: Uuid) -> Result<bool, StoreError>;
    /// Changes a member's role. Returns `false` when the user is not a
    /// member. (Unlike `upsert_membership`, this overwrites.)
    async fn set_membership_role(
        &self,
        user_id: Uuid,
        company_id: Uuid,
        role: Role,
    ) -> Result<bool, StoreError>;
    /// Revokes every live device of a user in a company (member removal —
    /// device tokens carry company access). Returns how many were revoked.
    async fn revoke_user_devices(&self, company_id: Uuid, user_id: Uuid)
        -> Result<u64, StoreError>;

    // Tokens (opaque bearer tokens; only SHA-256 hashes are stored)
    /// `expires_at = None` means non-expiring (legacy tokens issued before
    /// expiry existed); the API layer always passes an expiry for new tokens.
    async fn insert_user_token(
        &self,
        token_hash: &str,
        user_id: Uuid,
        company_id: Uuid,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), StoreError>;
    /// Resolves a member/device bearer token. Revoked device tokens and
    /// expired user tokens do not resolve.
    async fn resolve_token(&self, token_hash: &str) -> Result<Option<TokenIdentity>, StoreError>;

    // Invitations (tokens stored hashed, like every other credential)
    async fn create_invitation(&self, invitation: Invitation) -> Result<(), StoreError>;
    /// Looks an invitation up by the SHA-256 hex hash of its token.
    async fn invitation_by_hash(&self, token_hash: &str) -> Result<Option<Invitation>, StoreError>;
    async fn mark_invitation_accepted(
        &self,
        invitation_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), StoreError>;

    // Devices
    async fn create_device(&self, device: Device) -> Result<(), StoreError>;
    /// All devices of a company (including revoked ones), oldest first.
    async fn devices(&self, company_id: Uuid) -> Result<Vec<Device>, StoreError>;
    /// One device, scoped to the company — `None` when the id exists but
    /// belongs to another company (callers surface that as 404, never 403).
    async fn device(&self, company_id: Uuid, device_id: Uuid)
        -> Result<Option<Device>, StoreError>;
    /// Marks a device revoked; idempotent (an already-revoked device keeps
    /// its original `revoked_at`). Returns the post-update device, or `None`
    /// when the device is not in the company.
    async fn revoke_device(
        &self,
        company_id: Uuid,
        device_id: Uuid,
    ) -> Result<Option<Device>, StoreError>;
    /// Stamps `last_seen_at = seen_at`, but only when the current value is
    /// null or older than `stale_before` — the write throttle that keeps
    /// sync polling from hammering the devices table.
    async fn touch_device_seen(
        &self,
        device_id: Uuid,
        seen_at: chrono::DateTime<chrono::Utc>,
        stale_before: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), StoreError>;

    // Mutation log (per-company, server-assigned monotonic sync versions)
    /// Assigns the next sync versions; idempotent on mutation id — a re-pushed
    /// id gets its previously assigned version back. Returns `(id, version)`
    /// pairs in input order.
    async fn push_mutations(
        &self,
        company_id: Uuid,
        mutations: Vec<MutationRecord>,
    ) -> Result<Vec<(String, i64)>, StoreError>;
    /// One page of mutations with version > `after`, ordered by version
    /// ascending, with `sync_version` set on each returned record. At most
    /// `limit` records are returned (`limit` must be positive); `has_more`
    /// reports exactly whether further mutations exist past the page.
    async fn pull_mutations(
        &self,
        company_id: Uuid,
        after: i64,
        limit: i64,
    ) -> Result<MutationPage, StoreError>;
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
    /// All customer/supplier subledger rows a voucher produced (submit and
    /// reversal rows).
    async fn party_transactions_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<PartyTransaction>, StoreError>;
    /// All tax subledger rows a voucher produced (submit and reversal rows).
    async fn tax_transactions_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<TaxTransaction>, StoreError>;
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

    // ------------------------------------------------------------------
    // Whole-table inspection (test suite / operational verification)
    //
    // The store-parameterized test suite asserts ledger state through these
    // on BOTH implementations, so mem and pg are pinned to the same
    // behaviour by the same assertions.
    // ------------------------------------------------------------------

    /// All webhook events received so far, oldest first.
    async fn webhook_events(&self) -> Result<Vec<WebhookEvent>, StoreError>;
    /// Every stock ledger entry of a company, insertion order.
    async fn all_stock_ledger_entries(
        &self,
        company_id: Uuid,
    ) -> Result<Vec<StockLedgerEntry>, StoreError>;
    /// Every customer/supplier subledger row of a company.
    async fn all_party_transactions(
        &self,
        company_id: Uuid,
    ) -> Result<Vec<PartyTransaction>, StoreError>;
    /// Every tax subledger row of a company.
    async fn all_tax_transactions(
        &self,
        company_id: Uuid,
    ) -> Result<Vec<TaxTransaction>, StoreError>;
    /// Every settlement of a company.
    async fn all_settlements(&self, company_id: Uuid) -> Result<Vec<Settlement>, StoreError>;
    /// Every (item, warehouse) bin of a company.
    async fn all_bins(&self, company_id: Uuid) -> Result<Vec<Bin>, StoreError>;
}
