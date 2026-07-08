//! Materialized document read model for the portal plane.
//!
//! Drafts (quotations, unpaid-invoice metadata, customers, …) exist only as
//! mutations in the per-company log; the portal has to *render* documents, so
//! both stores fold the log into a `company_documents` projection:
//! `(company_id, doctype, document_id)` → latest inner payload, child rows,
//! docstatus and update time. The fold is maintained incrementally inside the
//! same atomic step that appends to the log (client `sync/push` and the
//! posting-commit replication path both go through the stores' shared
//! append), and [`crate::store::Store::rebuild_projection`] refolds a company
//! from scratch as a recovery tool.
//!
//! Mutations are applied in sync-version order with the client's
//! `_applyMutation` semantics: create/update/submit/cancel upsert the row
//! (docstatus from the envelope, else implied by the mutation type),
//! `deleteDocument` removes it, and `__children` — carried inside the
//! envelope payload — replaces the stored child rows; a mutation without
//! `__children` leaves them intact.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::model::{MutationRecord, MutationType};

/// One row of the materialized read model.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanyDocument {
    pub company_id: Uuid,
    pub doctype: String,
    pub document_id: String,
    /// The envelope's inner payload as a JSON object (header fields).
    pub payload: Value,
    /// The `__children` rows, when any mutation ever carried them.
    pub children: Option<Value>,
    pub docstatus: i16,
    pub updated_at: DateTime<Utc>,
}

/// What folding one mutation over the current row does to the projection.
pub enum ProjectionAction {
    Upsert(CompanyDocument),
    Delete,
    Keep,
}

/// Folds one mutation over the current projection row (if any).
///
/// The mutation's wire payload is the Dart sync engine's row envelope whose
/// inner `payload` is a JSON-encoded string (system-authored mutations) or a
/// plain object; a mutation whose payload has no `payload` key at all is
/// taken as the fields directly (legacy plain-payload pushes).
pub fn fold_mutation(
    existing: Option<&CompanyDocument>,
    record: &MutationRecord,
    company_id: Uuid,
    now: DateTime<Utc>,
) -> ProjectionAction {
    match record.mutation_type {
        MutationType::DeleteDocument => return ProjectionAction::Delete,
        MutationType::CreateDocument
        | MutationType::UpdateDocument
        | MutationType::SubmitDocument
        | MutationType::CancelDocument => {}
        // App installs and attachments never touch the document read model.
        _ => return ProjectionAction::Keep,
    }
    let envelope = &record.payload;
    let mut fields: Map<String, Value> = match envelope.get("payload") {
        Some(Value::String(inner)) => serde_json::from_str::<Value>(inner)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        Some(Value::Object(map)) => map.clone(),
        _ => envelope.clone(),
    };
    let children = fields
        .remove("__children")
        .or_else(|| envelope.get("__children").cloned())
        .filter(|value| !value.is_null())
        .or_else(|| existing.and_then(|doc| doc.children.clone()));
    let docstatus = envelope
        .get("docstatus")
        .and_then(Value::as_i64)
        .map(|value| value as i16)
        .unwrap_or_else(|| match record.mutation_type {
            MutationType::SubmitDocument => 1,
            MutationType::CancelDocument => 2,
            MutationType::UpdateDocument => existing.map(|doc| doc.docstatus).unwrap_or(0),
            _ => 0,
        });
    ProjectionAction::Upsert(CompanyDocument {
        company_id,
        doctype: record.doc_type.clone(),
        document_id: record.document_id.clone(),
        payload: Value::Object(fields),
        children,
        docstatus,
        updated_at: now,
    })
}
