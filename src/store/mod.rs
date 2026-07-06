//! Persistence boundary. Everything the HTTP layer needs is behind the
//! [`Store`] trait so the service runs identically over the in-memory store
//! (`--mem` dev mode, test suite) and Postgres (production, `DATABASE_URL`).

pub mod mem;
pub mod pg;

use async_trait::async_trait;
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, MutationRecord, Role, TokenIdentity, User,
    WebhookEvent,
};

pub use mem::MemStore;
pub use pg::PgStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A uniqueness / state conflict the caller may surface as 409.
    #[error("conflict: {0}")]
    Conflict(String),
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
}
