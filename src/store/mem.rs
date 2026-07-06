//! In-memory [`Store`] implementation. Used by the test suite and the `--mem`
//! development mode; state lives for the process lifetime only.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, MutationRecord, Role, TokenIdentity, User,
    WebhookEvent,
};

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
    /// (user_id, company_id) -> role
    memberships: HashMap<(Uuid, Uuid), Role>,
    /// token hash -> (user_id, company_id)
    user_tokens: HashMap<String, (Uuid, Uuid)>,
    invitations: HashMap<String, Invitation>,
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
            .or_insert(role);
        Ok(())
    }

    async fn membership_role(
        &self,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<Option<Role>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.memberships.get(&(user_id, company_id)).copied())
    }

    async fn insert_user_token(
        &self,
        token_hash: &str,
        user_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .user_tokens
            .insert(token_hash.to_string(), (user_id, company_id));
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
            .map(|&(user_id, company_id)| TokenIdentity {
                user_id,
                company_id,
                device_id: None,
            }))
    }

    async fn create_invitation(&self, invitation: Invitation) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.invitations.contains_key(&invitation.token) {
            return Err(StoreError::Conflict("invitation token exists".into()));
        }
        inner
            .invitations
            .insert(invitation.token.clone(), invitation);
        Ok(())
    }

    async fn invitation(&self, token: &str) -> Result<Option<Invitation>, StoreError> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.invitations.get(token).cloned())
    }

    async fn mark_invitation_accepted(&self, token: &str, user_id: Uuid) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        match inner.invitations.get_mut(token) {
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

    async fn push_mutations(
        &self,
        company_id: Uuid,
        mutations: Vec<MutationRecord>,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let mut versions = Vec::with_capacity(mutations.len());
        for record in mutations {
            let key = (company_id, record.id.clone());
            if let Some(&existing) = inner.mutation_versions.get(&key) {
                versions.push((record.id, existing));
                continue;
            }
            let next = inner.counters.entry(company_id).or_insert(0);
            *next += 1;
            let version = *next;
            inner.mutation_versions.insert(key, version);
            versions.push((record.id.clone(), version));
            inner
                .mutations
                .entry(company_id)
                .or_default()
                .push(StoredMutation {
                    record,
                    version,
                    acknowledged: false,
                });
        }
        Ok(versions)
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
}
