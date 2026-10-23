//! Posting authority on the **sync plane**.
//!
//! The command API (`posting`) is the only way official accounting reaches a
//! Team company. The mutation log, however, replicates whatever a device
//! pushes to every other device, and the Dart sync engine applies a pulled
//! row verbatim: a `GL Entry` row lands in every ledger and a
//! `submitDocument` flips a document to submitted everywhere. Without this
//! guard, one device could therefore publish official state that the backend
//! never validated, numbered or posted (a hand-made GL row, a "submitted"
//! Journal Entry, a Bin with any quantity).
//!
//! [`check_client_mutation`] rejects, for client-pushed mutations:
//!
//! * **Derived ledger rows** ([`DERIVED_DOCTYPES`]): GL, stock ledger,
//!   bins, settlements and the party / tax subledgers. Only the backend's
//!   posting commits author these; devices receive them through the log.
//! * **Official lifecycle transitions** on [`SERVER_AUTHORITY_DOCTYPES`]:
//!   `submitDocument` / `cancelDocument`, or a create/update carrying
//!   `docstatus >= 1`. Drafts of these doctypes sync normally; making them
//!   official goes through `commands/submit-document` (or, for the doctypes
//!   the backend cannot post yet, is not possible in Team at all).
//! * **Spoofed system provenance**: the backend's reserved device ids and
//!   mutation-id prefixes, so a device can neither impersonate the posting
//!   authority in the log nor pre-empt a deterministic system mutation id
//!   (the log is idempotent on id, so a squatted id would silently swallow
//!   the real replication).
//!
//! POS Invoice is the deliberate exception (decision recorded in the hub's
//! `docs/team/TEAM_POSTING_AUTHORITY.md`): a till posts the sale locally so
//! checkout never needs the network, and its lifecycle mutations replicate as
//! the *local fiscal sale*. Its Team-wide accounting (GL, stock, tax) is
//! derived only by the backend when the till confirms the sale through the
//! command API (`offline_fiscal`), so its ledger rows are still never
//! accepted from a device.

use serde_json::Value;

use crate::model::{MutationRecord, MutationType};

/// Rows only a posting commit may author.
pub const DERIVED_DOCTYPES: [&str; 8] = [
    "GL Entry",
    "Stock Ledger Entry",
    "Bin",
    "Settlement",
    "Customer Transaction",
    "Supplier Transaction",
    "Tax Transaction",
    "Posting Batch",
];

/// Every document type whose submission produces official ledger effects
/// (the Dart `LedgerDerivation.postingSources`) except POS Invoice. Their
/// drafts sync; their official lifecycle is server-only. Journal Entry,
/// Expense and Customer Deposit are listed although the backend cannot post
/// them yet: until it can, they cannot be made official in a Team company at
/// all, which is the safe failure (the client says so before submitting).
pub const SERVER_AUTHORITY_DOCTYPES: [&str; 9] = [
    "Sales Invoice",
    "Purchase Invoice",
    "Purchase Receipt",
    "Delivery Note",
    "Payment Entry",
    "Stock Entry",
    "Journal Entry",
    "Expense",
    "Customer Deposit",
];

/// Device ids the backend stamps on the mutations it authors.
pub const RESERVED_DEVICE_IDS: [&str; 3] = [
    crate::posting::replication::SYSTEM_DEVICE_ID,
    crate::pay::PAYMENTS_DEVICE_ID,
    crate::portal::PORTAL_DEVICE_ID,
];

/// Mutation-id prefixes the backend generates deterministically.
pub const RESERVED_ID_PREFIXES: [&str; 2] = ["postmut-", "portal-"];

/// Why a client mutation is refused; the API answers 422 with this text.
pub fn check_client_mutation(mutation: &MutationRecord) -> Result<(), String> {
    let doctype = mutation.doc_type.as_str();
    if RESERVED_DEVICE_IDS.contains(&mutation.device_id.as_str()) {
        return Err(format!(
            "mutation {} claims the reserved system device id {:?}",
            mutation.id, mutation.device_id
        ));
    }
    if RESERVED_ID_PREFIXES
        .iter()
        .any(|prefix| mutation.id.starts_with(prefix))
    {
        return Err(format!(
            "mutation id {:?} uses a prefix reserved for backend-authored mutations",
            mutation.id
        ));
    }
    if DERIVED_DOCTYPES.contains(&doctype) {
        return Err(format!(
            "{doctype} {} is a ledger row: in Atlas Team only the server posts ledger rows",
            mutation.document_id
        ));
    }
    if SERVER_AUTHORITY_DOCTYPES.contains(&doctype) {
        let lifecycle = matches!(
            mutation.mutation_type,
            MutationType::SubmitDocument | MutationType::CancelDocument
        );
        let official_state = matches!(
            mutation.mutation_type,
            MutationType::CreateDocument | MutationType::UpdateDocument
        ) && docstatus(mutation) >= 1;
        if lifecycle || official_state {
            return Err(format!(
                "{doctype} {} can only be submitted or cancelled through the Atlas Team \
                 server (commands/submit-document); a device cannot publish it as official",
                mutation.document_id
            ));
        }
    }
    Ok(())
}

/// The row envelope's `docstatus` (absent or malformed reads as a draft).
fn docstatus(mutation: &MutationRecord) -> i64 {
    match mutation.payload.get("docstatus") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MutationStatus;
    use serde_json::json;

    fn mutation(doctype: &str, mutation_type: MutationType, docstatus: i64) -> MutationRecord {
        MutationRecord {
            id: "m-1".into(),
            mutation_type,
            doc_type: doctype.into(),
            document_id: "D-1".into(),
            payload: json!({ "id": "D-1", "doctype": doctype, "docstatus": docstatus })
                .as_object()
                .unwrap()
                .clone(),
            device_id: "dev-a".into(),
            user_id: "u".into(),
            local_timestamp: 0,
            sync_version: None,
            status: MutationStatus::Pending,
        }
    }

    #[test]
    fn drafts_and_masters_pass() {
        assert!(
            check_client_mutation(&mutation("Customer", MutationType::UpdateDocument, 0)).is_ok()
        );
        assert!(
            check_client_mutation(&mutation("Sales Invoice", MutationType::CreateDocument, 0))
                .is_ok()
        );
        assert!(
            check_client_mutation(&mutation("Journal Entry", MutationType::UpdateDocument, 0))
                .is_ok()
        );
    }

    #[test]
    fn ledger_rows_and_official_transitions_are_refused() {
        for doctype in DERIVED_DOCTYPES {
            assert!(
                check_client_mutation(&mutation(doctype, MutationType::CreateDocument, 0)).is_err()
            );
        }
        for doctype in SERVER_AUTHORITY_DOCTYPES {
            assert!(
                check_client_mutation(&mutation(doctype, MutationType::SubmitDocument, 0)).is_err()
            );
            assert!(
                check_client_mutation(&mutation(doctype, MutationType::CancelDocument, 0)).is_err()
            );
            assert!(
                check_client_mutation(&mutation(doctype, MutationType::UpdateDocument, 1)).is_err()
            );
            assert!(
                check_client_mutation(&mutation(doctype, MutationType::CreateDocument, 2)).is_err()
            );
        }
    }

    #[test]
    fn pos_local_fiscal_lifecycle_is_the_deliberate_exception() {
        assert!(
            check_client_mutation(&mutation("POS Invoice", MutationType::SubmitDocument, 1))
                .is_ok()
        );
    }

    #[test]
    fn credential_path_segments_are_redacted_in_logs() {
        use crate::api::redact_path;
        assert_eq!(
            redact_path("/portal/abc123/gl.csv"),
            "/portal/<token>/gl.csv"
        );
        assert_eq!(redact_path("/pay/abc123"), "/pay/<token>");
        assert_eq!(
            redact_path("/invitations/abc/accept"),
            "/invitations/<token>/accept"
        );
        assert_eq!(
            redact_path("/companies/c1/sync/pull"),
            "/companies/c1/sync/pull"
        );
    }

    #[test]
    fn system_provenance_cannot_be_spoofed() {
        let mut m = mutation("Customer", MutationType::UpdateDocument, 0);
        m.device_id = "atlas-backend".into();
        assert!(check_client_mutation(&m).is_err());
        let mut m = mutation("Customer", MutationType::UpdateDocument, 0);
        m.id = "postmut-SINV-1".into();
        assert!(check_client_mutation(&m).is_err());
    }
}
