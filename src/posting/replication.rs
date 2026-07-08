//! Replication of posting results onto the company mutation log.
//!
//! `Store::posting_commit` writes official documents, GL / stock ledger /
//! settlement rows and bins into the backend's posting tables — but client
//! devices only ever sync the company mutation log (`sync/push` /
//! `sync/pull`). This module renders one posting commit as system-authored
//! [`MutationRecord`]s (device id [`SYSTEM_DEVICE_ID`]) that the stores append
//! to the log inside the same atomic commit, so every device's normal sync
//! pull receives the posted state.
//!
//! Wire shape: each mutation's payload is the Dart sync engine's **row
//! envelope** (what `sync_engine.dart` `_applyMutation` writes into the client
//! `documents` table) whose inner `payload` field is a JSON-encoded *string*
//! of the row's fields. Derived-row field names mirror the Dart
//! `ledger_derivation.dart` builders (`_gl`, `_sle`, settlements, bins) so the
//! replicated rows are indistinguishable from locally derived ones.
//!
//! Every mutation id is deterministic (`postmut-…`, reversal ids inherit the
//! `-reversal` suffix) and the log is idempotent on id, so idempotency-key
//! replays and stale-state retries can never duplicate log entries.

use chrono::Utc;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::model::{MutationRecord, MutationStatus, MutationType};
use crate::posting::model::{
    Bin, GlEntry, PartyTransaction, PostedDocument, Settlement, StockLedgerEntry, TaxTransaction,
};
use crate::posting::values::REVERSAL_SUFFIX;

/// Device id stamped on server-authored mutations. Devices skip only their
/// *own* device id when applying a pull, so this constant applies everywhere.
pub const SYSTEM_DEVICE_ID: &str = "atlas-backend";

/// The final transactional state one commit produced, handed over by the
/// store *inside* its atomicity boundary: `document` already carries the
/// allocated official number and the target docstatus, and
/// `outstanding_documents` are the referenced invoices *after* their
/// `outstanding_amount` maintenance.
pub struct ReplicationSources<'a> {
    pub document: &'a PostedDocument,
    /// false: submit commit; true: cancel commit (reversal rows).
    pub is_cancel: bool,
    pub gl_entries: &'a [GlEntry],
    pub stock_ledger_entries: &'a [StockLedgerEntry],
    pub party_transactions: &'a [PartyTransaction],
    pub tax_transactions: &'a [TaxTransaction],
    pub settlements: &'a [Settlement],
    pub bins: &'a [Bin],
    pub outstanding_documents: &'a [PostedDocument],
    /// The acting user (from the commit's audit row).
    pub user_id: Option<Uuid>,
    /// Device id stamped on the replicated mutations ([`SYSTEM_DEVICE_ID`]
    /// unless the commit came from a system actor with its own, e.g. the
    /// payments webhook's `atlas-payments`).
    pub device_id: &'a str,
}

/// Renders one posting commit as the mutation records to append to the
/// company log: the document itself (`submitDocument` / `cancelDocument`),
/// one `createDocument` per GL / stock ledger / settlement row, one
/// `updateDocument` per touched bin, and one `updateDocument` per invoice
/// whose outstanding this commit maintained.
pub fn replication_mutations(
    src: &ReplicationSources<'_>,
) -> Result<Vec<MutationRecord>, serde_json::Error> {
    let now_ms = Utc::now().timestamp_millis();
    let user_id = src.user_id.map(|id| id.to_string()).unwrap_or_default();
    // Cancel-side ids get the reversal suffix so they never collide with the
    // submit-side ids of the same document / bin / invoice.
    let sfx = if src.is_cancel { REVERSAL_SUFFIX } else { "" };
    let record = |id: String,
                  mutation_type: MutationType,
                  doc_type: &str,
                  document_id: &str,
                  payload: Map<String, Value>| MutationRecord {
        id,
        mutation_type,
        doc_type: doc_type.to_string(),
        document_id: document_id.to_string(),
        payload,
        device_id: src.device_id.to_string(),
        user_id: user_id.clone(),
        local_timestamp: now_ms,
        sync_version: None,
        status: MutationStatus::Pushed,
    };

    let mut out = Vec::new();

    // The document itself. Header fields only — no `__children` and no
    // `items` key: the draft's line items already replicated through the
    // client's own draft mutations.
    let doc = src.document;
    let mut fields = header_fields(&doc.payload);
    if let Some(number) = &doc.official_number {
        fields.insert("official_number".into(), json!(number));
    }
    let mutation_type = if src.is_cancel {
        MutationType::CancelDocument
    } else {
        MutationType::SubmitDocument
    };
    out.push(record(
        format!("postmut-{}-doc{sfx}", doc.id),
        mutation_type,
        &doc.doctype,
        &doc.id,
        row_envelope(&doc.id, &doc.doctype, doc.docstatus, &fields, now_ms)?,
    ));

    // GL entries — field names per the Dart `_gl` builder.
    for entry in src.gl_entries {
        let mut fields = Map::new();
        fields.insert("posting_date".into(), json!(entry.posting_date));
        fields.insert("account".into(), json!(entry.account));
        fields.insert("debit".into(), json!(entry.debit));
        fields.insert("credit".into(), json!(entry.credit));
        if let Some(party_type) = &entry.party_type {
            fields.insert("party_type".into(), json!(party_type));
        }
        if let Some(party) = &entry.party {
            fields.insert("party".into(), json!(party));
        }
        fields.insert("voucher_type".into(), json!(entry.voucher_type));
        fields.insert("voucher_no".into(), json!(entry.voucher_no));
        // Multi-currency base stamping (the Dart `_stampBaseAmounts` /
        // `_stockGl` fields), present on stamped vouchers and stock legs.
        if let Some(currency) = &entry.currency {
            fields.insert("currency".into(), json!(currency));
        }
        if let Some(rate) = entry.conversion_rate {
            fields.insert("conversion_rate".into(), json!(rate));
        }
        if let Some(base_debit) = entry.base_debit {
            fields.insert("base_debit".into(), json!(base_debit));
        }
        if let Some(base_credit) = entry.base_credit {
            fields.insert("base_credit".into(), json!(base_credit));
        }
        fields.insert("is_reversal".into(), json!(entry.is_reversal));
        out.push(record(
            format!("postmut-{}", entry.id),
            MutationType::CreateDocument,
            "GL Entry",
            &entry.id,
            row_envelope(&entry.id, "GL Entry", 0, &fields, now_ms)?,
        ));
    }

    // Stock ledger entries — field names per the Dart `_sle` builder.
    for sle in src.stock_ledger_entries {
        let mut fields = Map::new();
        fields.insert("trans_type".into(), json!(sle.trans_type));
        fields.insert("item".into(), json!(sle.item));
        fields.insert("warehouse".into(), json!(sle.warehouse));
        fields.insert("qty_change".into(), json!(sle.qty_change));
        fields.insert("valuation_rate".into(), json!(sle.valuation_rate));
        fields.insert("posting_date".into(), json!(sle.posting_date));
        fields.insert("voucher_type".into(), json!(sle.voucher_type));
        fields.insert("voucher_no".into(), json!(sle.voucher_no));
        // Transaction UOM (qty/rate are already stock units), like the Dart
        // `_sle` builder's optional `uom` field.
        if let Some(uom) = &sle.uom {
            fields.insert("uom".into(), json!(uom));
        }
        fields.insert("is_reversal".into(), json!(sle.is_reversal));
        out.push(record(
            format!("postmut-{}", sle.id),
            MutationType::CreateDocument,
            "Stock Ledger Entry",
            &sle.id,
            row_envelope(&sle.id, "Stock Ledger Entry", 0, &fields, now_ms)?,
        ));
    }

    // Customer / supplier subledger rows — the Dart `Customer Transaction` /
    // `Supplier Transaction` row shapes (`CT-…` / `VT-…` ids, the party under
    // its `customer` / `supplier` field name).
    for txn in src.party_transactions {
        out.push(record(
            format!("postmut-{}", txn.id),
            MutationType::CreateDocument,
            txn.kind.doctype(),
            &txn.id,
            row_envelope(&txn.id, txn.kind.doctype(), 0, &txn.row_fields(), now_ms)?,
        ));
    }

    // Tax subledger rows — the Dart `Tax Transaction` shape (`TT-…` ids).
    for txn in src.tax_transactions {
        out.push(record(
            format!("postmut-{}", txn.id),
            MutationType::CreateDocument,
            "Tax Transaction",
            &txn.id,
            row_envelope(&txn.id, "Tax Transaction", 0, &txn.row_fields(), now_ms)?,
        ));
    }

    // Settlements — field names per the Dart payment-entry derivation.
    for settlement in src.settlements {
        let mut fields = Map::new();
        fields.insert(
            "payment_voucher_type".into(),
            json!(settlement.payment_voucher_type),
        );
        fields.insert(
            "payment_voucher_no".into(),
            json!(settlement.payment_voucher_no),
        );
        fields.insert(
            "invoice_voucher_type".into(),
            json!(settlement.invoice_voucher_type),
        );
        fields.insert(
            "invoice_voucher_no".into(),
            json!(settlement.invoice_voucher_no),
        );
        fields.insert("party_type".into(), json!(settlement.party_type));
        fields.insert("party".into(), json!(settlement.party));
        fields.insert(
            "allocated_amount".into(),
            json!(settlement.allocated_amount),
        );
        fields.insert("posting_date".into(), json!(settlement.posting_date));
        fields.insert("is_reversal".into(), json!(settlement.is_reversal));
        out.push(record(
            format!("postmut-{}", settlement.id),
            MutationType::CreateDocument,
            "Settlement",
            &settlement.id,
            row_envelope(&settlement.id, "Settlement", 0, &fields, now_ms)?,
        ));
    }

    // Bins — the deterministic `BIN-{item}-{warehouse}` row the Dart bin
    // recompute upserts.
    for bin in src.bins {
        let bin_id = format!("BIN-{}-{}", bin.item, bin.warehouse);
        let mut fields = Map::new();
        fields.insert("item".into(), json!(bin.item));
        fields.insert("warehouse".into(), json!(bin.warehouse));
        fields.insert("actual_qty".into(), json!(bin.actual_qty));
        fields.insert("stock_value".into(), json!(bin.stock_value));
        fields.insert("valuation_rate".into(), json!(bin.valuation_rate));
        out.push(record(
            format!("postmut-{}-bin-{}-{}{sfx}", doc.id, bin.item, bin.warehouse),
            MutationType::UpdateDocument,
            "Bin",
            &bin_id,
            row_envelope(&bin_id, "Bin", 0, &fields, now_ms)?,
        ));
    }

    // Outstanding maintenance: header-only update of each referenced invoice
    // with its recomputed `outstanding_amount` (children left intact on the
    // client because no `__children` key is present).
    for invoice in src.outstanding_documents {
        let mut fields = header_fields(&invoice.payload);
        if let Some(number) = &invoice.official_number {
            fields.insert("official_number".into(), json!(number));
        }
        out.push(record(
            format!("postmut-{}-outstanding-{}{sfx}", doc.id, invoice.id),
            MutationType::UpdateDocument,
            &invoice.doctype,
            &invoice.id,
            row_envelope(
                &invoice.id,
                &invoice.doctype,
                invoice.docstatus,
                &fields,
                now_ms,
            )?,
        ));
    }

    Ok(out)
}

/// A document's payload as header fields: the stored payload object minus the
/// child-row tables (which live in `document_children` client-side, already
/// replicated through the client's own draft mutations).
fn header_fields(payload: &Value) -> Map<String, Value> {
    let mut fields = match payload {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    for child_table in ["items", "taxes", "references", "tenders", "accounts"] {
        fields.remove(child_table);
    }
    fields
}

/// The Dart sync engine's row envelope — what `_applyMutation` writes into
/// the client `documents` table. The inner `payload` is a JSON-encoded
/// *string* of the row's fields, exactly as the client stores it. Public so
/// other system authors (the portal's quotation accept/reject) render the
/// same wire shape.
pub fn row_envelope(
    id: &str,
    doctype: &str,
    docstatus: i16,
    fields: &Map<String, Value>,
    now_ms: i64,
) -> Result<Map<String, Value>, serde_json::Error> {
    let mut envelope = Map::new();
    envelope.insert("id".into(), json!(id));
    envelope.insert("doctype".into(), json!(doctype));
    envelope.insert("company".into(), Value::Null);
    envelope.insert("docstatus".into(), json!(docstatus));
    envelope.insert(
        "payload".into(),
        Value::String(serde_json::to_string(fields)?),
    );
    envelope.insert("created_at".into(), json!(now_ms));
    envelope.insert("modified_at".into(), json!(now_ms));
    envelope.insert("sync_version".into(), Value::Null);
    envelope.insert("sync_state".into(), json!("synced"));
    envelope.insert("amended_from".into(), Value::Null);
    Ok(envelope)
}
