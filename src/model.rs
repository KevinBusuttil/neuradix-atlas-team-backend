//! Domain model for the Atlas Team coordination backend.
//!
//! `MutationRecord` mirrors the Flutter client's sync contract
//! (`mercantis_core/lib/src/sync_engine/mutation_record.dart`) — field names on
//! the wire are camelCase and enum values use Dart enum `name`s, so the
//! client's `HttpCloudAdapter` can (de)serialize records unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Company {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub created_at: DateTime<Utc>,
}

/// Membership role profiles (decision doc §8). These compile down to the
/// client's granular permission rules; server-side they gate whole endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Sales,
    Purchasing,
    Stock,
    Pos,
    Accountant,
    Advisor,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Sales => "sales",
            Role::Purchasing => "purchasing",
            Role::Stock => "stock",
            Role::Pos => "pos",
            Role::Accountant => "accountant",
            Role::Advisor => "advisor",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "owner" => Role::Owner,
            "admin" => Role::Admin,
            "sales" => Role::Sales,
            "purchasing" => Role::Purchasing,
            "stock" => Role::Stock,
            "pos" => Role::Pos,
            "accountant" => Role::Accountant,
            "advisor" => Role::Advisor,
            _ => return None,
        })
    }
}

/// A company membership joined with its user record — the member-list read
/// model. `created_at` is the membership's creation time (when the user
/// joined the company), not the user's.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Invitation {
    pub token: String,
    pub company_id: Uuid,
    pub email: String,
    pub role: Role,
    pub created_by: Uuid,
    pub accepted_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: Uuid,
    pub company_id: Uuid,
    pub user_id: Uuid,
    pub name: String,
    pub token_hash: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// Stamped on successful device-token authentication (throttled — see
    /// `Store::touch_device_seen`), so owners/admins can spot stale or
    /// stolen credentials in the device list.
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// What a bearer token resolves to. `device_id` is `Some` for device tokens
/// and `None` for user tokens.
#[derive(Debug, Clone, Copy)]
pub struct TokenIdentity {
    pub user_id: Uuid,
    pub company_id: Uuid,
    pub device_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Portal links (customer / accountant portal plane)
// ---------------------------------------------------------------------------

/// What a portal link exposes: a single customer's documents, or the whole
/// company read-only for an accountant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortalLinkKind {
    Customer,
    Accountant,
}

impl PortalLinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PortalLinkKind::Customer => "customer",
            PortalLinkKind::Accountant => "accountant",
        }
    }

    pub fn parse(s: &str) -> Option<PortalLinkKind> {
        Some(match s {
            "customer" => PortalLinkKind::Customer,
            "accountant" => PortalLinkKind::Accountant,
            _ => return None,
        })
    }
}

/// A tokenized portal grant. The token itself is a distinct token kind: it is
/// stored (hashed) only here, never in `user_tokens`/`devices`, so a portal
/// token can never authenticate a member/device endpoint and vice versa.
#[derive(Debug, Clone)]
pub struct PortalLink {
    pub id: Uuid,
    pub company_id: Uuid,
    pub kind: PortalLinkKind,
    /// The customer id the link is scoped to (customer kind only).
    pub party: Option<String>,
    pub label: Option<String>,
    pub token_hash: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Pay links (invoice payment plane — pay.atlas.neuradix.app)
// ---------------------------------------------------------------------------

/// A tokenized invoice payment link. Like portal links, the token is a
/// distinct token kind: its hash is stored only here, so a pay token can
/// never authenticate a member/device/portal endpoint and vice versa. A pay
/// link exposes exactly one submitted Sales Invoice, read-only, plus the
/// payment handoff (Stripe Payment Link URL or manual instructions).
#[derive(Debug, Clone)]
pub struct PayLink {
    pub id: Uuid,
    pub company_id: Uuid,
    /// The Sales Invoice document id the link pays.
    pub invoice_id: String,
    pub token_hash: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Sync (mirrors the Dart MutationRecord)
// ---------------------------------------------------------------------------

/// Dart `MutationType` enum names, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MutationType {
    CreateDocument,
    UpdateDocument,
    DeleteDocument,
    SubmitDocument,
    CancelDocument,
    AmendDocument,
    InstallApp,
    UninstallApp,
    CreateAttachment,
    DeleteAttachment,
}

/// Dart `MutationStatus` enum names, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MutationStatus {
    #[default]
    Pending,
    Pushing,
    Pushed,
    Failed,
}

/// Wire-compatible mirror of the Dart `MutationRecord`.
///
/// `localTimestamp` is milliseconds since epoch (Dart
/// `DateTime.millisecondsSinceEpoch`); `syncVersion` is a string because the
/// Dart field is `String?` — the server assigns monotonically increasing
/// integers per company and stringifies them here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutationRecord {
    pub id: String,
    #[serde(rename = "type")]
    pub mutation_type: MutationType,
    pub doc_type: String,
    pub document_id: String,
    pub payload: serde_json::Map<String, Value>,
    pub device_id: String,
    pub user_id: String,
    pub local_timestamp: i64,
    #[serde(default)]
    pub sync_version: Option<String>,
    #[serde(default)]
    pub status: MutationStatus,
}

// ---------------------------------------------------------------------------
// Audit + webhooks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: Uuid,
    pub company_id: Uuid,
    pub user_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
    pub action: String,
    pub detail: Value,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookKind {
    Payment,
    Channel,
}

impl WebhookKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WebhookKind::Payment => "payment",
            WebhookKind::Channel => "channel",
        }
    }

    pub fn parse(s: &str) -> Option<WebhookKind> {
        Some(match s {
            "payment" => WebhookKind::Payment,
            "channel" => WebhookKind::Channel,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct WebhookEvent {
    pub id: Uuid,
    pub kind: WebhookKind,
    pub provider: String,
    pub headers: Value,
    pub body: Vec<u8>,
    pub received_at: DateTime<Utc>,
}
