//! In-memory [`Store`] implementation. Used by the test suite and the `--mem`
//! development mode; state lives for the process lifetime only.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, Member, MutationRecord, PayLink, PortalLink, Role,
    TokenIdentity, User, WebhookEvent,
};
use crate::posting::model::{
    format_number, Bin, CommitOutcome, CompanySettings, GlEntry, Item, PartyTransaction,
    PostedDocument, PostingBatch, PostingCommit, Settlement, StockLedgerEntry, TaxTransaction,
};
use crate::posting::replication::{replication_mutations, ReplicationSources, SYSTEM_DEVICE_ID};
use crate::projection::{fold_mutation, CompanyDocument, ProjectionAction};

use super::{Store, StoreError};

#[derive(Debug, Clone)]
struct StoredMutation {
    record: MutationRecord,
    version: i64,
    acknowledged: bool,
}

#[derive(Default)]
struct Inner {
    companies: HashMap<Uuid, Company>,
    users: HashMap<Uuid, User>,
    users_by_email: HashMap<String, Uuid>,
    /// (user_id, company_id) -> (role, membership created_at)
    memberships: HashMap<(Uuid, Uuid), (Role, chrono::DateTime<Utc>)>,
    /// token hash -> (user_id, company_id, expires_at; None = non-expiring)
    user_tokens: HashMap<String, (Uuid, Uuid, Option<chrono::DateTime<Utc>>)>,
    /// invitation id -> invitation (tokens live here only as hashes)
    invitations: HashMap<Uuid, Invitation>,
    devices: HashMap<Uuid, Device>,
    /// token hash -> device id
    device_tokens: HashMap<String, Uuid>,
    /// company -> ordered mutation log
    mutations: HashMap<Uuid, Vec<StoredMutation>>,
    /// (company_id, mutation id) -> assigned version
    mutation_versions: HashMap<(Uuid, String), i64>,
    /// company -> last assigned version
    counters: HashMap<Uuid, i64>,
    /// (company_id, sha256) -> bytes
    blobs: HashMap<(Uuid, String), Vec<u8>>,
    audit: Vec<AuditEntry>,
    webhooks: Vec<WebhookEvent>,

    // Posting authority (Phase 3)
    settings: HashMap<Uuid, CompanySettings>,
    /// (company_id, item id) -> registry entry
    items: HashMap<(Uuid, String), Item>,
    /// (company_id, doctype, document id) -> official document
    documents: HashMap<(Uuid, String, String), PostedDocument>,
    gl_entries: HashMap<Uuid, Vec<GlEntry>>,
    stock_ledger: HashMap<Uuid, Vec<StockLedgerEntry>>,
    /// company -> last assigned SLE sequence (chronological replay order)
    sle_seq: HashMap<Uuid, i64>,
    /// (company_id, item, warehouse) -> derived balance
    bins: HashMap<(Uuid, String, String), Bin>,
    party_transactions: HashMap<Uuid, Vec<PartyTransaction>>,
    tax_transactions: HashMap<Uuid, Vec<TaxTransaction>>,
    settlements: HashMap<Uuid, Vec<Settlement>>,
    batches: HashMap<(Uuid, String), PostingBatch>,
    /// (company_id, series key) -> last allocated value (gap-free)
    series: HashMap<(Uuid, String), i64>,
    /// (company_id, idempotency key) -> committed response
    idempotency: HashMap<(Uuid, String), Value>,

    // Portal (links + materialized document read model)
    /// link id -> portal link
    portal_links: HashMap<Uuid, PortalLink>,
    /// link id -> pay link (invoice payment plane)
    pay_links: HashMap<Uuid, PayLink>,
    /// (company_id, doctype, document id) -> projected read-model row
    company_documents: HashMap<(Uuid, String, String), CompanyDocument>,
}

impl Inner {
    /// Appends to the company mutation log with the same idempotent,
    /// monotonic version assignment as `push_mutations` — callable while the
    /// store mutex is already held (e.g. inside `posting_commit`). A record
    /// whose id was already logged keeps its original version and is not
    /// re-appended. Returns `(id, version)` pairs in input order.
    fn append_mutations(
        &mut self,
        company_id: Uuid,
        mutations: Vec<MutationRecord>,
    ) -> Vec<(String, i64)> {
        let mut versions = Vec::with_capacity(mutations.len());
        for record in mutations {
            let key = (company_id, record.id.clone());
            if let Some(&existing) = self.mutation_versions.get(&key) {
                versions.push((record.id, existing));
                continue;
            }
            let next = self.counters.entry(company_id).or_insert(0);
            *next += 1;
            let version = *next;
            self.mutation_versions.insert(key, version);
            versions.push((record.id.clone(), version));
            // The materialized document read model follows the log inside
            // the same mutex hold (same atomicity as the log write).
            self.apply_projection(company_id, &record);
            self.mutations
                .entry(company_id)
                .or_default()
                .push(StoredMutation {
                    record,
                    version,
                    acknowledged: false,
                });
        }
        versions
    }

    /// Folds one just-appended mutation into the `company_documents` read
    /// model.
    fn apply_projection(&mut self, company_id: Uuid, record: &MutationRecord) {
        let key = (
            company_id,
            record.doc_type.clone(),
            record.document_id.clone(),
        );
        let existing = self.company_documents.get(&key);
        match fold_mutation(existing, record, company_id, Utc::now()) {
            ProjectionAction::Upsert(doc) => {
                self.company_documents.insert(key, doc);
            }
            ProjectionAction::Delete => {
                self.company_documents.remove(&key);
            }
            ProjectionAction::Keep => {}
        }
    }
}

#[derive(Default)]
pub struct MemStore {
    inner: Mutex<Inner>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Test-inspection helper: all webhook events received so far.
    pub fn webhook_events(&self) -> Vec<WebhookEvent> {
        self.inner.lock().unwrap().webhooks.clone()
    }

    /// Test-inspection helper: every GL entry posted for a company.
    pub fn all_gl_entries(&self, company_id: Uuid) -> Vec<GlEntry> {
        self.inner
            .lock()
            .unwrap()
            .gl_entries
            .get(&company_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Test-inspection helper: every stock ledger entry for a company.
    pub fn all_stock_ledger_entries(&self, company_id: Uuid) -> Vec<StockLedgerEntry> {
        self.inner
            .lock()
            .unwrap()
            .stock_ledger
            .get(&company_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Test-inspection helper: every bin for a company.
    pub fn all_bins(&self, company_id: Uuid) -> Vec<Bin> {
        let inner = self.inner.lock().unwrap();
        inner
            .bins
            .iter()
            .filter(|((company, _, _), _)| *company == company_id)
            .map(|(_, bin)| bin.clone())
            .collect()
    }

    /// Test-inspection helper: every customer/supplier subledger row for a
    /// company.
    pub fn all_party_transactions(&self, company_id: Uuid) -> Vec<PartyTransaction> {
        self.inner
            .lock()
            .unwrap()
            .party_transactions
            .get(&company_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Test-inspection helper: every tax subledger row for a company.
    pub fn all_tax_transactions(&self, company_id: Uuid) -> Vec<TaxTransaction> {
        self.inner
            .lock()
            .unwrap()
            .tax_transactions
            .get(&company_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Test-inspection helper: every settlement for a company.
    pub fn all_settlements(&self, company_id: Uuid) -> Vec<Settlement> {
        self.inner
            .lock()
            .unwrap()
            .settlements
            .get(&company_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Test-inspection helper: a posted document.
    pub fn document(&self, company_id: Uuid, doctype: &str, id: &str) -> Option<PostedDocument> {
        self.inner
            .lock()
            .unwrap()
            .documents
            .get(&(company_id, doctype.to_string(), id.to_string()))
            .cloned()
    }
}

#[async_trait]
impl Store for MemStore {
    async fn create_company(&self, name: &str) -> Result<Company, StoreError> {
        let company = Company {
            id: Uuid::new_v4(),
            name: name.to_string(),
            created_at: Utc::now(),
        };
        let mut inner = self.inner.lock().unwrap();
        inner.companies.insert(company.id, company.clone());
        Ok(company)
    }

    async fn company(&self, company_id: Uuid) -> Result<Option<Company>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.companies.get(&company_id).cloned())
    }

    async fn upsert_user(&self, email: &str, display_name: &str) -> Result<User, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(id) = inner.users_by_email.get(email) {
            return Ok(inner.users[id].clone());
        }
        let user = User {
            id: Uuid::new_v4(),
            email: email.to_string(),
            display_name: display_name.to_string(),
            created_at: Utc::now(),
        };
        inner.users_by_email.insert(email.to_string(), user.id);
        inner.users.insert(user.id, user.clone());
        Ok(user)
    }

    async fn upsert_membership(
        &self,
        user_id: Uuid,
        company_id: Uuid,
        role: Role,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .memberships
            .entry((user_id, company_id))
            .or_insert((role, Utc::now()));
        Ok(())
    }

    async fn membership_role(
        &self,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<Option<Role>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .memberships
            .get(&(user_id, company_id))
            .map(|&(role, _)| role))
    }

    async fn company_members(&self, company_id: Uuid) -> Result<Vec<Member>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut members: Vec<Member> = inner
            .memberships
            .iter()
            .filter(|((_, company), _)| *company == company_id)
            .filter_map(|((user_id, _), &(role, created_at))| {
                inner.users.get(user_id).map(|user| Member {
                    user_id: *user_id,
                    email: user.email.clone(),
                    display_name: user.display_name.clone(),
                    role,
                    created_at,
                })
            })
            .collect();
        members.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.user_id.cmp(&b.user_id))
        });
        Ok(members)
    }

    async fn remove_membership(&self, user_id: Uuid, company_id: Uuid) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.memberships.remove(&(user_id, company_id)).is_some())
    }

    async fn set_membership_role(
        &self,
        user_id: Uuid,
        company_id: Uuid,
        role: Role,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.memberships.get_mut(&(user_id, company_id)) {
            Some((current, _)) => {
                *current = role;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn revoke_user_devices(
        &self,
        company_id: Uuid,
        user_id: Uuid,
    ) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let now = Utc::now();
        let mut revoked = 0;
        for device in inner.devices.values_mut() {
            if device.company_id == company_id
                && device.user_id == user_id
                && device.revoked_at.is_none()
            {
                device.revoked_at = Some(now);
                revoked += 1;
            }
        }
        Ok(revoked)
    }

    async fn insert_user_token(
        &self,
        token_hash: &str,
        user_id: Uuid,
        company_id: Uuid,
        expires_at: Option<chrono::DateTime<Utc>>,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .user_tokens
            .insert(token_hash.to_string(), (user_id, company_id, expires_at));
        Ok(())
    }

    async fn resolve_token(&self, token_hash: &str) -> Result<Option<TokenIdentity>, StoreError> {
        let inner = self.inner.lock().unwrap();
        if let Some(device_id) = inner.device_tokens.get(token_hash) {
            if let Some(device) = inner.devices.get(device_id) {
                if device.revoked_at.is_none() {
                    return Ok(Some(TokenIdentity {
                        user_id: device.user_id,
                        company_id: device.company_id,
                        device_id: Some(device.id),
                    }));
                }
            }
            return Ok(None);
        }
        Ok(inner
            .user_tokens
            .get(token_hash)
            // Expired user tokens do not resolve; None = non-expiring.
            .filter(|(_, _, expires_at)| expires_at.is_none_or(|expiry| expiry > Utc::now()))
            .map(|&(user_id, company_id, _)| TokenIdentity {
                user_id,
                company_id,
                device_id: None,
            }))
    }

    async fn create_invitation(&self, invitation: Invitation) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .invitations
            .values()
            .any(|existing| existing.token_hash == invitation.token_hash)
        {
            return Err(StoreError::Conflict("invitation token exists".into()));
        }
        inner.invitations.insert(invitation.id, invitation);
        Ok(())
    }

    async fn invitation_by_hash(&self, token_hash: &str) -> Result<Option<Invitation>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .invitations
            .values()
            .find(|invitation| invitation.token_hash == token_hash)
            .cloned())
    }

    async fn mark_invitation_accepted(
        &self,
        invitation_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.invitations.get_mut(&invitation_id) {
            Some(invitation) => {
                invitation.accepted_by = Some(user_id);
                Ok(())
            }
            None => Err(StoreError::Internal("invitation not found".into())),
        }
    }

    async fn create_device(&self, device: Device) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .device_tokens
            .insert(device.token_hash.clone(), device.id);
        inner.devices.insert(device.id, device);
        Ok(())
    }

    async fn devices(&self, company_id: Uuid) -> Result<Vec<Device>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut devices: Vec<Device> = inner
            .devices
            .values()
            .filter(|device| device.company_id == company_id)
            .cloned()
            .collect();
        devices.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(devices)
    }

    async fn device(
        &self,
        company_id: Uuid,
        device_id: Uuid,
    ) -> Result<Option<Device>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .devices
            .get(&device_id)
            .filter(|device| device.company_id == company_id)
            .cloned())
    }

    async fn revoke_device(
        &self,
        company_id: Uuid,
        device_id: Uuid,
    ) -> Result<Option<Device>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.devices.get_mut(&device_id) {
            Some(device) if device.company_id == company_id => {
                if device.revoked_at.is_none() {
                    device.revoked_at = Some(Utc::now());
                }
                Ok(Some(device.clone()))
            }
            _ => Ok(None),
        }
    }

    async fn touch_device_seen(
        &self,
        device_id: Uuid,
        seen_at: chrono::DateTime<Utc>,
        stale_before: chrono::DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(device) = inner.devices.get_mut(&device_id) {
            if device.last_seen_at.is_none_or(|seen| seen < stale_before) {
                device.last_seen_at = Some(seen_at);
            }
        }
        Ok(())
    }

    async fn push_mutations(
        &self,
        company_id: Uuid,
        mutations: Vec<MutationRecord>,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        Ok(inner.append_mutations(company_id, mutations))
    }

    async fn pull_mutations(
        &self,
        company_id: Uuid,
        after: i64,
    ) -> Result<Vec<MutationRecord>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<(i64, MutationRecord)> = inner
            .mutations
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|m| m.version > after)
                    .map(|m| {
                        let mut record = m.record.clone();
                        record.sync_version = Some(m.version.to_string());
                        (m.version, record)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|(version, _)| *version);
        Ok(out.into_iter().map(|(_, record)| record).collect())
    }

    async fn ack_mutations(&self, company_id: Uuid, ids: &[String]) -> Result<u64, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let mut count = 0;
        if let Some(log) = inner.mutations.get_mut(&company_id) {
            for stored in log.iter_mut() {
                if ids.contains(&stored.record.id) && !stored.acknowledged {
                    stored.acknowledged = true;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    async fn put_blob(
        &self,
        company_id: Uuid,
        sha256: &str,
        bytes: Vec<u8>,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .blobs
            .entry((company_id, sha256.to_string()))
            .or_insert(bytes);
        Ok(())
    }

    async fn get_blob(
        &self,
        company_id: Uuid,
        sha256: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.blobs.get(&(company_id, sha256.to_string())).cloned())
    }

    async fn has_blob(&self, company_id: Uuid, sha256: &str) -> Result<bool, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.blobs.contains_key(&(company_id, sha256.to_string())))
    }

    async fn append_audit(&self, entry: AuditEntry) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.audit.push(entry);
        Ok(())
    }

    async fn recent_audit(
        &self,
        company_id: Uuid,
        limit: i64,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .audit
            .iter()
            .rev()
            .filter(|entry| entry.company_id == company_id)
            .take(limit.max(0) as usize)
            .cloned()
            .collect())
    }

    async fn insert_webhook_event(&self, event: WebhookEvent) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.webhooks.push(event);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Posting authority (Phase 3)
    // ------------------------------------------------------------------

    async fn company_settings(&self, company_id: Uuid) -> Result<CompanySettings, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.settings.get(&company_id).cloned().unwrap_or_default())
    }

    async fn put_company_settings(
        &self,
        company_id: Uuid,
        settings: CompanySettings,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.settings.insert(company_id, settings);
        Ok(())
    }

    async fn upsert_item(&self, company_id: Uuid, item: Item) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.items.insert((company_id, item.id.clone()), item);
        Ok(())
    }

    async fn items(&self, company_id: Uuid, ids: &[String]) -> Result<Vec<Item>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| inner.items.get(&(company_id, id.clone())).cloned())
            .collect())
    }

    async fn posted_document(
        &self,
        company_id: Uuid,
        doctype: &str,
        id: &str,
    ) -> Result<Option<PostedDocument>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .documents
            .get(&(company_id, doctype.to_string(), id.to_string()))
            .cloned())
    }

    async fn sles_for_pair(
        &self,
        company_id: Uuid,
        item: &str,
        warehouse: &str,
    ) -> Result<Vec<StockLedgerEntry>, StoreError> {
        let inner = self.inner.lock().unwrap();
        // The per-company log is already in insertion (seq) order.
        Ok(inner
            .stock_ledger
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|sle| sle.item == item && sle.warehouse == warehouse)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn sles_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<StockLedgerEntry>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .stock_ledger
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|sle| sle.voucher_no == voucher_no)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn gl_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<GlEntry>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .gl_entries
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|entry| entry.voucher_no == voucher_no)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn party_transactions_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<PartyTransaction>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .party_transactions
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|t| t.voucher_no == voucher_no)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn tax_transactions_for_voucher(
        &self,
        company_id: Uuid,
        voucher_no: &str,
    ) -> Result<Vec<TaxTransaction>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .tax_transactions
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|t| t.voucher_no == voucher_no)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn settlements_for_invoice(
        &self,
        company_id: Uuid,
        invoice_doctype: &str,
        invoice_no: &str,
    ) -> Result<Vec<Settlement>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .settlements
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|s| {
                        s.invoice_voucher_type == invoice_doctype
                            && s.invoice_voucher_no == invoice_no
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn settlements_for_payment(
        &self,
        company_id: Uuid,
        payment_no: &str,
    ) -> Result<Vec<Settlement>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .settlements
            .get(&company_id)
            .map(|log| {
                log.iter()
                    .filter(|s| s.payment_voucher_no == payment_no)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn idempotent_response(
        &self,
        company_id: Uuid,
        key: &str,
    ) -> Result<Option<Value>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .idempotency
            .get(&(company_id, key.to_string()))
            .cloned())
    }

    async fn posting_commit(&self, commit: PostingCommit) -> Result<CommitOutcome, StoreError> {
        // One mutex over all state makes the whole commit atomic and
        // serializes concurrent commands per process (the Postgres analogue
        // is a per-company advisory transaction lock).
        let mut inner = self.inner.lock().unwrap();
        let company = commit.company_id;

        // Idempotency replay: a key that already committed returns its stored
        // response without posting anything again.
        if let Some(key) = &commit.idempotency_key {
            if let Some(response) = inner.idempotency.get(&(company, key.clone())) {
                return Ok(CommitOutcome {
                    response: response.clone(),
                    replayed: true,
                });
            }
        }

        // Optimistic concurrency: the engine costed/validated against the
        // prior stock ledger; if any touched pair grew since, recompute.
        for (item, warehouse, expected) in &commit.sle_expectations {
            let actual = inner
                .stock_ledger
                .get(&company)
                .map(|log| {
                    log.iter()
                        .filter(|sle| &sle.item == item && &sle.warehouse == warehouse)
                        .count()
                })
                .unwrap_or(0);
            if actual != *expected {
                return Err(StoreError::Stale(format!(
                    "stock ledger for {item}/{warehouse} moved ({expected} -> {actual})"
                )));
            }
        }

        let mut document = commit.document;
        let doc_key = (company, document.doctype.clone(), document.id.clone());
        if commit.document_is_new {
            if inner.documents.contains_key(&doc_key) {
                return Err(StoreError::Conflict(format!(
                    "document {} {} already exists",
                    document.doctype, document.id
                )));
            }
        } else {
            let Some(existing) = inner.documents.get(&doc_key) else {
                return Err(StoreError::Conflict(format!(
                    "document {} {} does not exist",
                    document.doctype, document.id
                )));
            };
            if existing.docstatus != 1 {
                return Err(StoreError::Conflict(format!(
                    "document {} {} is not submitted",
                    document.doctype, document.id
                )));
            }
        }

        // Gap-free official number: allocated only here, inside the same
        // atomic step that persists the posting.
        let mut response = commit.response;
        if let Some(series) = &commit.series_key {
            let next = inner.series.entry((company, series.clone())).or_insert(0);
            *next += 1;
            let number = format_number(series, *next);
            document.official_number = Some(number.clone());
            response["number"] = json!(number);
        }

        inner.documents.insert(doc_key, document.clone());
        let seq = inner.sle_seq.entry(company).or_insert(0);
        let mut sequenced = commit.stock_ledger_entries;
        for sle in &mut sequenced {
            *seq += 1;
            sle.seq = *seq;
        }
        let mut outstanding_docs = Vec::new();
        for (doctype, id, outstanding) in &commit.outstanding_updates {
            if let Some(doc) = inner
                .documents
                .get_mut(&(company, doctype.clone(), id.clone()))
            {
                if let Some(payload) = doc.payload.as_object_mut() {
                    payload.insert("outstanding_amount".into(), json!(outstanding));
                }
                outstanding_docs.push(doc.clone());
            }
        }
        // Replicate the posted results onto the company mutation log inside
        // this same mutex hold, so every device's normal sync pull receives
        // the official state. Deterministic ids + the log's idempotency on id
        // make retries and replays harmless.
        let records = replication_mutations(&ReplicationSources {
            document: &document,
            is_cancel: commit.batch.kind == "cancel",
            gl_entries: &commit.gl_entries,
            stock_ledger_entries: &sequenced,
            party_transactions: &commit.party_transactions,
            tax_transactions: &commit.tax_transactions,
            settlements: &commit.settlements,
            bins: &commit.bins,
            outstanding_documents: &outstanding_docs,
            user_id: commit.audit.user_id,
            device_id: commit.replication_device_id.unwrap_or(SYSTEM_DEVICE_ID),
        })?;
        inner.append_mutations(company, records);
        inner
            .gl_entries
            .entry(company)
            .or_default()
            .extend(commit.gl_entries);
        inner
            .stock_ledger
            .entry(company)
            .or_default()
            .extend(sequenced);
        inner
            .party_transactions
            .entry(company)
            .or_default()
            .extend(commit.party_transactions);
        inner
            .tax_transactions
            .entry(company)
            .or_default()
            .extend(commit.tax_transactions);
        inner
            .settlements
            .entry(company)
            .or_default()
            .extend(commit.settlements);
        for bin in commit.bins {
            inner
                .bins
                .insert((company, bin.item.clone(), bin.warehouse.clone()), bin);
        }
        inner
            .batches
            .insert((company, commit.batch.id.clone()), commit.batch);
        inner.audit.push(commit.audit);
        if let Some(key) = commit.idempotency_key {
            inner.idempotency.insert((company, key), response.clone());
        }
        Ok(CommitOutcome {
            response,
            replayed: false,
        })
    }

    // ------------------------------------------------------------------
    // Portal (links + materialized document read model)
    // ------------------------------------------------------------------

    async fn create_portal_link(&self, link: PortalLink) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.portal_links.insert(link.id, link);
        Ok(())
    }

    async fn portal_links(&self, company_id: Uuid) -> Result<Vec<PortalLink>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut links: Vec<PortalLink> = inner
            .portal_links
            .values()
            .filter(|link| link.company_id == company_id)
            .cloned()
            .collect();
        links.sort_by_key(|link| link.created_at);
        Ok(links)
    }

    async fn revoke_portal_link(
        &self,
        company_id: Uuid,
        link_id: Uuid,
    ) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.portal_links.get_mut(&link_id) {
            Some(link) if link.company_id == company_id => {
                link.revoked_at.get_or_insert_with(Utc::now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn portal_link_by_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<PortalLink>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .portal_links
            .values()
            .find(|link| link.token_hash == token_hash)
            .cloned())
    }

    // ------------------------------------------------------------------
    // Pay links (invoice payment plane)
    // ------------------------------------------------------------------

    async fn create_pay_link(&self, link: PayLink) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.pay_links.insert(link.id, link);
        Ok(())
    }

    async fn pay_links(&self, company_id: Uuid) -> Result<Vec<PayLink>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut links: Vec<PayLink> = inner
            .pay_links
            .values()
            .filter(|link| link.company_id == company_id)
            .cloned()
            .collect();
        links.sort_by_key(|link| link.created_at);
        Ok(links)
    }

    async fn revoke_pay_link(&self, company_id: Uuid, link_id: Uuid) -> Result<bool, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.pay_links.get_mut(&link_id) {
            Some(link) if link.company_id == company_id => {
                link.revoked_at.get_or_insert_with(Utc::now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn pay_link_by_hash(&self, token_hash: &str) -> Result<Option<PayLink>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .pay_links
            .values()
            .find(|link| link.token_hash == token_hash)
            .cloned())
    }

    async fn company_document(
        &self,
        company_id: Uuid,
        doctype: &str,
        document_id: &str,
    ) -> Result<Option<CompanyDocument>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .company_documents
            .get(&(company_id, doctype.to_string(), document_id.to_string()))
            .cloned())
    }

    async fn company_documents(
        &self,
        company_id: Uuid,
        doctype: &str,
    ) -> Result<Vec<CompanyDocument>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut docs: Vec<CompanyDocument> = inner
            .company_documents
            .iter()
            .filter(|((company, dt, _), _)| *company == company_id && dt == doctype)
            .map(|(_, doc)| doc.clone())
            .collect();
        docs.sort_by(|a, b| a.document_id.cmp(&b.document_id));
        Ok(docs)
    }

    async fn rebuild_projection(&self, company_id: Uuid) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .company_documents
            .retain(|(company, _, _), _| *company != company_id);
        let mut records: Vec<(i64, MutationRecord)> = inner
            .mutations
            .get(&company_id)
            .map(|log| log.iter().map(|m| (m.version, m.record.clone())).collect())
            .unwrap_or_default();
        records.sort_by_key(|(version, _)| *version);
        for (_, record) in records {
            inner.apply_projection(company_id, &record);
        }
        Ok(())
    }

    async fn posted_document_counts(
        &self,
        company_id: Uuid,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut counts: HashMap<String, i64> = HashMap::new();
        for (company, doctype, _) in inner.documents.keys() {
            if *company == company_id {
                *counts.entry(doctype.clone()).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(String, i64)> = counts.into_iter().collect();
        out.sort();
        Ok(out)
    }

    async fn gl_entries_ordered(&self, company_id: Uuid) -> Result<Vec<GlEntry>, StoreError> {
        let inner = self.inner.lock().unwrap();
        let mut entries = inner
            .gl_entries
            .get(&company_id)
            .cloned()
            .unwrap_or_default();
        entries.sort_by(|a, b| {
            (&a.posting_date, &a.voucher_no, &a.id).cmp(&(&b.posting_date, &b.voucher_no, &b.id))
        });
        Ok(entries)
    }
}
