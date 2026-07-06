//! PostgreSQL [`Store`] implementation (SQLx, runtime queries only so the
//! build never needs a live database). Schema lives in `migrations/`.

use async_trait::async_trait;
use sqlx::postgres::PgPool;
use sqlx::Row;
use uuid::Uuid;

use crate::model::{
    AuditEntry, Company, Device, Invitation, MutationRecord, Role, TokenIdentity, User,
    WebhookEvent,
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
}
