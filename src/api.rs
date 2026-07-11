//! HTTP surface of the Phase 2 coordination backend: identity bootstrap,
//! invitations, devices, mutation-log sync, content-addressed blobs, webhook
//! intake and the audit feed. Every state-changing endpoint writes an audit
//! row (roadmap §6 criterion 8).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::{
    constant_time_eq, generate_token, hash_token, require_membership, require_role, AuthContext,
};
use crate::error::ApiError;
use crate::model::{
    AuditEntry, Device, Invitation, MutationRecord, Role, WebhookEvent, WebhookKind,
};
use crate::posting::engine::{
    allowed_roles, cancel_document as engine_cancel, submit_document as engine_submit, Actor,
    CancelCommand, SubmitCommand,
};
use crate::posting::model::{CompanySettings, Item, POSTED_DOCTYPES};
use crate::AppState;

const INVITATION_TTL_DAYS: i64 = 7;
const DEFAULT_AUDIT_LIMIT: i64 = 100;
const MAX_AUDIT_LIMIT: i64 = 500;
/// Absolute lifetime of newly issued user tokens, overridable via the
/// `ATLAS_USER_TOKEN_TTL_DAYS` environment variable.
const DEFAULT_USER_TOKEN_TTL_DAYS: i64 = 30;
/// Maximum (and default) sync-pull page size, overridable via the
/// `ATLAS_SYNC_PULL_PAGE_SIZE` environment variable.
const DEFAULT_SYNC_PULL_PAGE_SIZE: i64 = 200;

/// The server-side cap on how many mutations one `/sync/pull` returns.
fn sync_pull_max_page_size() -> i64 {
    std::env::var("ATLAS_SYNC_PULL_PAGE_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(DEFAULT_SYNC_PULL_PAGE_SIZE)
}

/// Request-body cap for every route except blob upload: JSON APIs and
/// webhook intake have no business receiving multi-megabyte bodies.
const JSON_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum accepted blob size in MiB, overridable via the
/// `ATLAS_MAX_BLOB_MB` environment variable (read once at router build).
const DEFAULT_MAX_BLOB_MB: usize = 25;

/// The blob route's body limit in bytes, derived from `ATLAS_MAX_BLOB_MB`.
fn max_blob_bytes() -> usize {
    let mb = std::env::var("ATLAS_MAX_BLOB_MB")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|mb| *mb > 0)
        .unwrap_or(DEFAULT_MAX_BLOB_MB);
    mb * 1024 * 1024
}

/// When a user token issued right now expires.
fn user_token_expires_at() -> chrono::DateTime<Utc> {
    let days = std::env::var("ATLAS_USER_TOKEN_TTL_DAYS")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|days| *days > 0)
        .unwrap_or(DEFAULT_USER_TOKEN_TTL_DAYS);
    Utc::now() + Duration::days(days)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/companies", post(create_company))
        .route(
            "/companies/{company_id}/invitations",
            post(create_invitation),
        )
        .route("/invitations/{token}/accept", post(accept_invitation))
        .route(
            "/companies/{company_id}/devices",
            post(register_device).get(list_devices),
        )
        .route(
            "/companies/{company_id}/devices/{device_id}/revoke",
            post(revoke_device),
        )
        .route("/companies/{company_id}/members", get(list_members))
        .route(
            "/companies/{company_id}/members/{user_id}",
            delete(remove_member),
        )
        .route(
            "/companies/{company_id}/members/{user_id}/role",
            post(change_member_role),
        )
        .route("/companies/{company_id}/sync/push", post(sync_push))
        .route("/companies/{company_id}/sync/pull", get(sync_pull))
        .route("/companies/{company_id}/sync/ack", post(sync_ack))
        .route(
            "/companies/{company_id}/blobs/{sha256}",
            put(put_blob)
                .get(get_blob)
                .head(head_blob)
                // Per-route body limit: blob uploads are the one place large
                // bodies are legitimate. Innermost layer wins, overriding the
                // router-wide JSON cap below.
                .layer(DefaultBodyLimit::max(max_blob_bytes())),
        )
        .route("/webhooks/payments/{provider}", post(webhook_payment))
        .route("/webhooks/channels/{connector}", post(webhook_channel))
        .route("/companies/{company_id}/audit", get(read_audit))
        .route(
            "/companies/{company_id}/settings",
            put(put_settings).get(get_settings),
        )
        .route("/companies/{company_id}/items", post(upsert_item))
        .route(
            "/companies/{company_id}/commands/submit-document",
            post(submit_document),
        )
        .route(
            "/companies/{company_id}/commands/cancel-document",
            post(cancel_document),
        )
        // Portal-link management + the token-scoped portal plane.
        .merge(crate::portal::routes())
        // Pay-link management + the token-scoped pay plane.
        .merge(crate::pay::routes())
        // Explicit request-body cap on every route registered above (blob
        // upload carries its own higher per-route limit). An oversized body
        // is rejected by the extractor with a plain 413, before any handler
        // (and thus any `ApiError` mapping) runs.
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT_BYTES))
        .with_state(state)
}

async fn audit(
    state: &AppState,
    company_id: Uuid,
    auth: Option<&AuthContext>,
    action: &str,
    detail: Value,
) -> Result<(), ApiError> {
    state
        .store
        .append_audit(AuditEntry {
            id: Uuid::new_v4(),
            company_id,
            user_id: auth.map(|a| a.user_id()),
            device_id: auth.and_then(|a| a.device_id()),
            action: action.to_string(),
            detail,
            at: Utc::now(),
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

// ---------------------------------------------------------------------------
// Companies + identity
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateCompanyRequest {
    name: String,
    #[serde(alias = "ownerEmail")]
    owner_email: String,
    #[serde(alias = "ownerName")]
    owner_name: String,
}

/// The header carrying the bootstrap shared secret when `ATLAS_BOOTSTRAP_TOKEN`
/// gates company creation.
pub const BOOTSTRAP_TOKEN_HEADER: &str = "x-atlas-bootstrap-token";

/// Bootstrap: create company + owner user + owner membership, return a user
/// token (roadmap §6 criterion 1).
///
/// When the operator configured `ATLAS_BOOTSTRAP_TOKEN`, the request must
/// carry the matching [`BOOTSTRAP_TOKEN_HEADER`] (compared in constant time);
/// otherwise creation stays open — the self-hoster default, warned about at
/// startup.
async fn create_company(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateCompanyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(expected) = state.config.bootstrap_token.as_deref() {
        let provided = headers
            .get(BOOTSTRAP_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !constant_time_eq(provided, expected) {
            return Err(ApiError::ForbiddenReason(
                "company creation is gated on this instance: send the correct \
                 X-Atlas-Bootstrap-Token header (the operator-configured \
                 ATLAS_BOOTSTRAP_TOKEN value)"
                    .into(),
            ));
        }
    }
    if req.name.trim().is_empty() || req.owner_email.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "name and owner_email are required".into(),
        ));
    }
    let company = state.store.create_company(req.name.trim()).await?;
    let owner = state
        .store
        .upsert_user(req.owner_email.trim(), req.owner_name.trim())
        .await?;
    state
        .store
        .upsert_membership(owner.id, company.id, Role::Owner)
        .await?;
    let token = generate_token();
    state
        .store
        .insert_user_token(
            &hash_token(&token),
            owner.id,
            company.id,
            Some(user_token_expires_at()),
        )
        .await?;
    state
        .store
        .append_audit(AuditEntry {
            id: Uuid::new_v4(),
            company_id: company.id,
            user_id: Some(owner.id),
            device_id: None,
            action: "company.create".into(),
            detail: json!({ "name": company.name, "ownerEmail": owner.email }),
            at: Utc::now(),
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "company": company,
            "userId": owner.id,
            "token": token,
        })),
    ))
}

#[derive(Deserialize)]
struct CreateInvitationRequest {
    email: String,
    role: Role,
}

/// Owner/admin invites a user by email (roadmap §6 criterion 2).
async fn create_invitation(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<CreateInvitationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner, Role::Admin])?;
    if req.email.trim().is_empty() {
        return Err(ApiError::BadRequest("email is required".into()));
    }
    // The plaintext token is returned exactly once; only its hash is stored.
    let token = generate_token();
    let expires_at = Utc::now() + Duration::days(INVITATION_TTL_DAYS);
    state
        .store
        .create_invitation(Invitation {
            id: Uuid::new_v4(),
            token_hash: hash_token(&token),
            company_id,
            email: req.email.trim().to_string(),
            role: req.role,
            created_by: auth.user_id(),
            accepted_by: None,
            created_at: Utc::now(),
            expires_at,
        })
        .await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "invitation.create",
        json!({ "email": req.email.trim(), "role": req.role }),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "token": token, "expiresAt": expires_at })),
    ))
}

#[derive(Deserialize)]
struct AcceptInvitationRequest {
    #[serde(alias = "displayName")]
    display_name: String,
}

/// Invited user joins: creates the user (if new) + membership, returns a user
/// token (roadmap §6 criterion 3, first half).
async fn accept_invitation(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Json(req): Json<AcceptInvitationRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // The presented token is a credential: hash it before lookup (only
    // hashes are stored) and answer 401, not 404, when it matches nothing.
    let invitation = state
        .store
        .invitation_by_hash(&hash_token(&token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if invitation.accepted_by.is_some() {
        return Err(ApiError::Conflict("invitation already accepted".into()));
    }
    if invitation.expires_at < Utc::now() {
        return Err(ApiError::Gone("invitation expired".into()));
    }
    let user = state
        .store
        .upsert_user(&invitation.email, req.display_name.trim())
        .await?;
    state
        .store
        .upsert_membership(user.id, invitation.company_id, invitation.role)
        .await?;
    state
        .store
        .mark_invitation_accepted(invitation.id, user.id)
        .await?;
    let user_token = generate_token();
    state
        .store
        .insert_user_token(
            &hash_token(&user_token),
            user.id,
            invitation.company_id,
            Some(user_token_expires_at()),
        )
        .await?;
    state
        .store
        .append_audit(AuditEntry {
            id: Uuid::new_v4(),
            company_id: invitation.company_id,
            user_id: Some(user.id),
            device_id: None,
            action: "invitation.accept".into(),
            detail: json!({ "email": invitation.email, "role": invitation.role }),
            at: Utc::now(),
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "userId": user.id,
            "companyId": invitation.company_id,
            "role": invitation.role,
            "token": user_token,
        })),
    ))
}

#[derive(Deserialize)]
struct RegisterDeviceRequest {
    name: String,
}

/// Register a device for the authenticated member; returns the device token
/// (roadmap §6 criterion 3, second half).
async fn register_device(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<RegisterDeviceRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name is required".into()));
    }
    let device_token = generate_token();
    let device = Device {
        id: Uuid::new_v4(),
        company_id,
        user_id: auth.user_id(),
        name: req.name.trim().to_string(),
        token_hash: hash_token(&device_token),
        created_at: Utc::now(),
        revoked_at: None,
        last_seen_at: None,
    };
    state.store.create_device(device.clone()).await?;
    state
        .store
        .append_audit(AuditEntry {
            id: Uuid::new_v4(),
            company_id,
            user_id: Some(auth.user_id()),
            device_id: Some(device.id),
            action: "device.register".into(),
            detail: json!({ "name": device.name }),
            at: Utc::now(),
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "deviceId": device.id, "deviceToken": device_token })),
    ))
}

/// Device inventory: any member sees their own devices; owner/admin see the
/// whole company's (including revoked ones — the point is credential
/// visibility).
async fn list_devices(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    let sees_all = matches!(role, Role::Owner | Role::Admin);
    let devices: Vec<Value> = state
        .store
        .devices(company_id)
        .await?
        .into_iter()
        .filter(|device| sees_all || device.user_id == auth.user_id())
        .map(|device| {
            json!({
                "id": device.id,
                "userId": device.user_id,
                "name": device.name,
                "createdAt": device.created_at,
                "revokedAt": device.revoked_at,
                "lastSeenAt": device.last_seen_at,
            })
        })
        .collect();
    Ok(Json(json!({ "devices": devices })))
}

/// Revoke a device token. A member may revoke their own device; owner/admin
/// may revoke anyone's. Idempotent — an already-revoked device keeps its
/// original `revokedAt` and still answers 200.
async fn revoke_device(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, device_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    let device = state
        .store
        .device(company_id, device_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if device.user_id != auth.user_id() {
        require_role(role, &[Role::Owner, Role::Admin])?;
    }
    let device = state
        .store
        .revoke_device(company_id, device_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "device.revoke",
        json!({ "deviceId": device.id, "deviceUserId": device.user_id }),
    )
    .await?;
    Ok(Json(
        json!({ "id": device.id, "revokedAt": device.revoked_at }),
    ))
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

/// Member directory: visible to every member of the company.
async fn list_members(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    let members = state.store.company_members(company_id).await?;
    Ok(Json(json!({ "members": members })))
}

/// How many owners the company has — the guard behind the last-owner rules.
async fn owner_count(state: &AppState, company_id: Uuid) -> Result<usize, ApiError> {
    Ok(state
        .store
        .company_members(company_id)
        .await?
        .iter()
        .filter(|member| member.role == Role::Owner)
        .count())
}

/// Remove a member (owner/admin). An owner may only be removed by themself,
/// and never when they are the last owner (409 — a company must always keep
/// one). Admins cannot remove owners or other admins. Removal also revokes
/// the member's devices in this company — device tokens carry company access;
/// account-global user tokens are left alone.
async fn remove_member(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner, Role::Admin])?;
    let target_role = state
        .store
        .membership_role(user_id, company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    match target_role {
        Role::Owner => {
            if user_id != auth.user_id() {
                return Err(ApiError::Forbidden);
            }
            if owner_count(&state, company_id).await? <= 1 {
                return Err(ApiError::Conflict(
                    "cannot remove the last owner of the company".into(),
                ));
            }
        }
        Role::Admin => {
            if role == Role::Admin && user_id != auth.user_id() {
                return Err(ApiError::Forbidden);
            }
        }
        _ => {}
    }
    if !state.store.remove_membership(user_id, company_id).await? {
        return Err(ApiError::NotFound);
    }
    let revoked_devices = state.store.revoke_user_devices(company_id, user_id).await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "member.remove",
        json!({ "userId": user_id, "role": target_role, "revokedDevices": revoked_devices }),
    )
    .await?;
    Ok(Json(json!({
        "userId": user_id,
        "removed": true,
        "revokedDevices": revoked_devices,
    })))
}

#[derive(Deserialize)]
struct ChangeRoleRequest {
    role: String,
}

/// Change a member's role (owner only). Demoting the last owner is a 409 —
/// a company must always keep one.
async fn change_member_role(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<ChangeRoleRequest>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner])?;
    let new_role = Role::parse(&req.role)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown role {}", req.role)))?;
    let target_role = state
        .store
        .membership_role(user_id, company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if target_role == Role::Owner
        && new_role != Role::Owner
        && owner_count(&state, company_id).await? <= 1
    {
        return Err(ApiError::Conflict(
            "cannot demote the last owner of the company".into(),
        ));
    }
    if !state
        .store
        .set_membership_role(user_id, company_id, new_role)
        .await?
    {
        return Err(ApiError::NotFound);
    }
    audit(
        &state,
        company_id,
        Some(&auth),
        "member.role",
        json!({ "userId": user_id, "from": target_role, "to": new_role }),
    )
    .await?;
    Ok(Json(json!({ "userId": user_id, "role": new_role })))
}

// ---------------------------------------------------------------------------
// Sync (mutation log — replication plane, decision doc §7)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PushRequest {
    mutations: Vec<MutationRecord>,
}

/// Device pushes a batch of `MutationRecord`s. The server assigns
/// monotonically increasing per-company sync versions; the call is idempotent
/// on mutation id (roadmap §6 criterion 4).
///
/// The request body is subject to the router-wide 2 MiB JSON cap
/// ([`JSON_BODY_LIMIT_BYTES`]) — a batch of many mutations with large
/// payloads can exceed it and gets a 413. That is by design: the client
/// pushes in batches, so an oversized batch is split, not accepted.
async fn sync_push(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<PushRequest>,
) -> Result<Json<Value>, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    auth.require_device()?;
    // Immutability (roadmap §6 criterion 7): once a posted-doctype document is
    // official (submitted or cancelled), the sync plane may not touch it —
    // changes must go through the command API, which posts reversals instead
    // of editing history.
    for mutation in &req.mutations {
        if !POSTED_DOCTYPES.contains(&mutation.doc_type.as_str()) {
            continue;
        }
        if let Some(doc) = state
            .store
            .posted_document(company_id, &mutation.doc_type, &mutation.document_id)
            .await?
        {
            if doc.docstatus >= 1 {
                return Err(ApiError::Conflict(format!(
                    "{} {} is officially posted (docstatus {}) and immutable; use the command API",
                    mutation.doc_type, mutation.document_id, doc.docstatus
                )));
            }
        }
    }
    let count = req.mutations.len();
    let assigned = state
        .store
        .push_mutations(company_id, req.mutations)
        .await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "sync.push",
        json!({ "count": count }),
    )
    .await?;
    let versions: serde_json::Map<String, Value> = assigned
        .into_iter()
        .map(|(id, version)| (id, Value::from(version)))
        .collect();
    Ok(Json(json!({ "versions": versions })))
}

#[derive(Deserialize)]
struct PullQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

/// Incremental pull, paginated: one page of mutations with sync version >
/// `after` (default 0), ordered by version ascending, `syncVersion` set on
/// each record. The page size is min(`limit`, server max) — `limit` absent or
/// ≤ 0 means the server max (default 200, env `ATLAS_SYNC_PULL_PAGE_SIZE`).
/// `hasMore` reports exactly whether mutations remain past the page; clients
/// loop with `after` = the last returned `syncVersion` until it is `false`.
async fn sync_pull(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Query(query): Query<PullQuery>,
) -> Result<Json<Value>, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    auth.require_device()?;
    let max = sync_pull_max_page_size();
    let limit = match query.limit {
        Some(limit) if limit > 0 => limit.min(max),
        _ => max,
    };
    let page = state
        .store
        .pull_mutations(company_id, query.after.unwrap_or(0), limit)
        .await?;
    Ok(Json(
        json!({ "mutations": page.mutations, "hasMore": page.has_more }),
    ))
}

#[derive(Deserialize)]
struct AckRequest {
    ids: Vec<String>,
}

async fn sync_ack(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<AckRequest>,
) -> Result<Json<Value>, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    auth.require_device()?;
    let acknowledged = state.store.ack_mutations(company_id, &req.ids).await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "sync.ack",
        json!({ "count": acknowledged }),
    )
    .await?;
    Ok(Json(json!({ "acknowledged": acknowledged })))
}

// ---------------------------------------------------------------------------
// Blobs (content-addressed attachment bytes, ADR-048 contract)
// ---------------------------------------------------------------------------

/// Upload attachment bytes, addressed by their SHA-256.
///
/// Size policy: the blob route's `DefaultBodyLimit` is `ATLAS_MAX_BLOB_MB`
/// MiB (default 25, read once at router build). A body over the limit is
/// rejected by the `Bytes` extractor with a plain 413 before this handler
/// runs — never a 500; a body exactly at the limit is accepted. Every other
/// route stays under the router-wide 2 MiB cap.
async fn put_blob(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, sha256)): Path<(Uuid, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    let sha256 = sha256.to_lowercase();
    let actual = hex::encode(Sha256::digest(&body));
    if actual != sha256 {
        return Err(ApiError::Unprocessable(format!(
            "body sha256 {actual} does not match path {sha256}"
        )));
    }
    state
        .store
        .put_blob(company_id, &sha256, body.to_vec())
        .await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "blob.put",
        json!({ "sha256": sha256, "size": body.len() }),
    )
    .await?;
    Ok(StatusCode::CREATED)
}

async fn get_blob(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, sha256)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    let bytes = state
        .store
        .get_blob(company_id, &sha256.to_lowercase())
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((
        StatusCode::OK,
        [("content-type", "application/octet-stream")],
        bytes,
    ))
}

async fn head_blob(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, sha256)): Path<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    if state
        .store
        .has_blob(company_id, &sha256.to_lowercase())
        .await?
    {
        Ok(StatusCode::OK)
    } else {
        Ok(StatusCode::NOT_FOUND)
    }
}

// ---------------------------------------------------------------------------
// Webhook intake (roadmap §6 criteria 12–13 — log only; signature
// verification and processing are Phase 4)
// ---------------------------------------------------------------------------

pub(crate) fn headers_to_json(headers: &HeaderMap) -> Value {
    let map: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                Value::from(String::from_utf8_lossy(value.as_bytes()).into_owned()),
            )
        })
        .collect();
    Value::Object(map)
}

async fn log_webhook(
    state: &AppState,
    kind: WebhookKind,
    provider: String,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    state
        .store
        .insert_webhook_event(WebhookEvent {
            id: Uuid::new_v4(),
            kind,
            provider,
            headers: headers_to_json(&headers),
            body: body.to_vec(),
            received_at: Utc::now(),
        })
        .await?;
    Ok(Json(json!({ "logged": true })))
}

async fn webhook_payment(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    log_webhook(&state, WebhookKind::Payment, provider, headers, body).await
}

async fn webhook_channel(
    State(state): State<AppState>,
    Path(connector): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    log_webhook(&state, WebhookKind::Channel, connector, headers, body).await
}

// ---------------------------------------------------------------------------
// Audit feed (roadmap §6 criteria 8–9)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

async fn read_audit(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner, Role::Admin, Role::Accountant])?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);
    let entries = state.store.recent_audit(company_id, limit).await?;
    Ok(Json(json!({ "entries": entries })))
}

// ---------------------------------------------------------------------------
// Posting authority (Phase 3 — roadmap §6 criteria 5–7 and 11)
// ---------------------------------------------------------------------------

/// Company posting settings (negative-stock policy, period lock, default
/// posting accounts). The request body is merged over the current settings so
/// callers can PATCH-style update a single field.
async fn put_settings(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(patch): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner, Role::Admin, Role::Accountant])?;
    let current = state.store.company_settings(company_id).await?;
    let mut merged = serde_json::to_value(&current).map_err(|e| ApiError::Internal(e.into()))?;
    let (Some(target), Some(fields)) = (merged.as_object_mut(), patch.as_object()) else {
        return Err(ApiError::BadRequest(
            "settings body must be an object".into(),
        ));
    };
    for (key, value) in fields {
        if !target.contains_key(key) {
            return Err(ApiError::BadRequest(format!("unknown setting {key}")));
        }
        target.insert(key.clone(), value.clone());
    }
    let settings: CompanySettings = serde_json::from_value(merged)
        .map_err(|e| ApiError::BadRequest(format!("invalid settings: {e}")))?;
    state
        .store
        .put_company_settings(company_id, settings.clone())
        .await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "settings.update",
        patch.clone(),
    )
    .await?;
    Ok(Json(
        serde_json::to_value(&settings).map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

async fn get_settings(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_membership(&state, &auth, company_id).await?;
    let settings = state.store.company_settings(company_id).await?;
    Ok(Json(
        serde_json::to_value(&settings).map_err(|e| ApiError::Internal(e.into()))?,
    ))
}

/// Item registry upsert: the posting engine's view of an item (stock vs
/// service, valuation method, account overrides).
async fn upsert_item(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(item): Json<Item>,
) -> Result<impl IntoResponse, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner, Role::Admin, Role::Stock])?;
    if item.id.trim().is_empty() {
        return Err(ApiError::BadRequest("item id is required".into()));
    }
    state.store.upsert_item(company_id, item.clone()).await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "item.upsert",
        json!({ "id": item.id, "itemType": item.item_type }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": item.id }))))
}

#[derive(Deserialize)]
struct SubmitDocumentRequest {
    doctype: String,
    #[serde(default)]
    document_id: Option<String>,
    #[serde(default)]
    payload: serde_json::Map<String, Value>,
    #[serde(default)]
    items: Vec<Value>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// Official submission (roadmap §6 criteria 5–6, 11): validates, allocates
/// the gap-free number, posts GL + stock + settlements atomically and returns
/// the official result. Device-token only, role-gated per doctype.
async fn submit_document(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<SubmitDocumentRequest>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    auth.require_device()?;
    let allowed = allowed_roles(&req.doctype)
        .ok_or_else(|| ApiError::BadRequest(format!("unsupported doctype {}", req.doctype)))?;
    require_role(role, allowed)?;
    let outcome = engine_submit(
        state.store.as_ref(),
        company_id,
        SubmitCommand {
            doctype: req.doctype,
            document_id: req.document_id,
            payload: req.payload,
            items: req.items,
            idempotency_key: req.idempotency_key,
        },
        Actor::user(auth.user_id(), auth.device_id()),
    )
    .await?;
    Ok(Json(outcome.response))
}

#[derive(Deserialize)]
struct CancelDocumentRequest {
    doctype: String,
    document_id: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// Cancellation: posts the linked reversal batch (negated legs, `-reversal`
/// ids, stock restored at original cost) and sets docstatus 2.
async fn cancel_document(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<CancelDocumentRequest>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    auth.require_device()?;
    let allowed = allowed_roles(&req.doctype)
        .ok_or_else(|| ApiError::BadRequest(format!("unsupported doctype {}", req.doctype)))?;
    require_role(role, allowed)?;
    let outcome = engine_cancel(
        state.store.as_ref(),
        company_id,
        CancelCommand {
            doctype: req.doctype,
            document_id: req.document_id,
            idempotency_key: req.idempotency_key,
        },
        Actor::user(auth.user_id(), auth.device_id()),
    )
    .await?;
    Ok(Json(outcome.response))
}
