//! Bearer-token authentication.
//!
//! Tokens are opaque 32-byte random values, handed out exactly once; the
//! store keeps only their SHA-256 hex hash. A token resolves to
//! (user, company, device?) — device tokens carry a device id, user tokens do
//! not. Company-scoped authorization is a separate membership check against
//! the company id in the request path (`require_membership`).
//!
//! # Token lifecycle
//!
//! * **User tokens** carry an absolute expiry (default 30 days, env
//!   `ATLAS_USER_TOKEN_TTL_DAYS`); expired tokens no longer resolve. Tokens
//!   issued before expiry existed have a null `expires_at` and are treated
//!   as non-expiring, so the migration does not lock out live sessions.
//! * **Device tokens** deliberately have **no** absolute expiry: they are
//!   long-lived credentials for offline-first devices that may sync only
//!   sporadically, and an expiry would silently brick a device holding
//!   unpushed local mutations. The controls are explicit revocation
//!   (per-device, or all of a member's devices on removal) plus
//!   `last_seen_at` visibility in the device list, which lets owners/admins
//!   spot and revoke stale or stolen credentials.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use chrono::{Duration, Utc};
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::ApiError;
use crate::model::{Role, TokenIdentity};
use crate::AppState;

/// `last_seen_at` write throttle: a device's last-seen stamp is refreshed on
/// successful authentication at most once per this window, so sync polling
/// does not turn every request into a devices-table write.
const DEVICE_SEEN_THROTTLE_MINUTES: i64 = 5;

/// Generate a fresh opaque token (64 hex chars, 256 bits of randomness).
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// SHA-256 hex of a token — the only form ever persisted.
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Authenticated caller, resolved from the `Authorization: Bearer` header.
#[derive(Debug, Clone, Copy)]
pub struct AuthContext(pub TokenIdentity);

impl AuthContext {
    pub fn user_id(&self) -> Uuid {
        self.0.user_id
    }

    pub fn device_id(&self) -> Option<Uuid> {
        self.0.device_id
    }

    /// Sync endpoints require a device token, not a user token.
    pub fn require_device(&self) -> Result<Uuid, ApiError> {
        self.0.device_id.ok_or(ApiError::Forbidden)
    }
}

impl FromRequestParts<AppState> for AuthContext {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(ApiError::Unauthorized)?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(ApiError::Unauthorized)?;
        let identity = state
            .store
            .resolve_token(&hash_token(token))
            .await?
            .ok_or(ApiError::Unauthorized)?;
        if let Some(device_id) = identity.device_id {
            let now = Utc::now();
            state
                .store
                .touch_device_seen(
                    device_id,
                    now,
                    now - Duration::minutes(DEVICE_SEEN_THROTTLE_MINUTES),
                )
                .await?;
        }
        Ok(AuthContext(identity))
    }
}

/// The caller must be a member of `company_id`; returns their role.
pub async fn require_membership(
    state: &AppState,
    auth: &AuthContext,
    company_id: Uuid,
) -> Result<Role, ApiError> {
    state
        .store
        .membership_role(auth.user_id(), company_id)
        .await?
        .ok_or(ApiError::Forbidden)
}

/// The caller's role must be one of `allowed`.
pub fn require_role(role: Role, allowed: &[Role]) -> Result<(), ApiError> {
    if allowed.contains(&role) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}
