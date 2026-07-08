//! Posting-authority domain types (Phase 3): official documents, GL entries,
//! stock ledger entries, bins, settlements, posting batches, numbering, the
//! per-company item registry and company settings.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::model::AuditEntry;

/// Doctypes the posting authority accepts. Everything else stays on the
/// draft/sync plane.
pub const POSTED_DOCTYPES: [&str; 5] = [
    "Sales Invoice",
    "Purchase Invoice",
    "Purchase Receipt",
    "Payment Entry",
    "Stock Entry",
];

/// Official-number series key per doctype. One strictly sequential, gap-free
/// series per (company, key); values are assigned only inside a successful
/// submit commit.
pub fn series_key(doctype: &str) -> Option<&'static str> {
    Some(match doctype {
        "Sales Invoice" => "SINV",
        "Purchase Invoice" => "PINV",
        "Purchase Receipt" => "PREC",
        "Payment Entry" => "PAY",
        "Stock Entry" => "STE",
        _ => return None,
    })
}

/// Renders an allocated series value as the official number, e.g.
/// `SINV-00007`.
pub fn format_number(key: &str, value: i64) -> String {
    format!("{key}-{value:05}")
}

/// An official (submitted or cancelled) document. `payload` carries the full
/// document body including its `items` child rows; `docstatus` follows the
/// client convention: 0 draft (never stored here), 1 submitted, 2 cancelled.
#[derive(Debug, Clone, Serialize)]
pub struct PostedDocument {
    pub id: String,
    pub company_id: Uuid,
    pub doctype: String,
    pub payload: Value,
    pub docstatus: i16,
    pub official_number: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GlEntry {
    pub id: String,
    pub company_id: Uuid,
    pub account: String,
    pub debit: f64,
    pub credit: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub party_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    pub voucher_type: String,
    pub voucher_no: String,
    pub posting_date: String,
    pub is_reversal: bool,
    pub batch_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StockLedgerEntry {
    pub id: String,
    pub company_id: Uuid,
    pub trans_type: String,
    pub item: String,
    pub warehouse: String,
    pub qty_change: f64,
    pub valuation_rate: f64,
    pub voucher_type: String,
    pub voucher_no: String,
    pub posting_date: String,
    pub is_reversal: bool,
    pub batch_id: String,
    /// Per-company insertion sequence, assigned by the store at commit; the
    /// FIFO replay orders the prior ledger by it (the Dart engine's
    /// posting-date + creation-order sort collapses to this server-side).
    pub seq: i64,
}

/// Derived (item, warehouse) balance, transactionally maintained: every
/// posting that moves stock recomputes and upserts the affected bins in the
/// same commit.
#[derive(Debug, Clone, Serialize)]
pub struct Bin {
    pub company_id: Uuid,
    pub item: String,
    pub warehouse: String,
    pub actual_qty: f64,
    pub valuation_rate: f64,
    pub stock_value: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Settlement {
    pub id: String,
    pub company_id: Uuid,
    pub payment_voucher_type: String,
    pub payment_voucher_no: String,
    pub invoice_voucher_type: String,
    pub invoice_voucher_no: String,
    pub party_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    pub allocated_amount: f64,
    pub posting_date: String,
    pub is_reversal: bool,
    pub batch_id: String,
}

/// One atomic posting: a submit batch (`PB-{document_id}`) or its linked
/// reversal (`PB-{document_id}-reversal`, `reversal_of` pointing back) —
/// mirroring the deterministic-id + reversal-linkage semantics of the Dart
/// posting batches.
#[derive(Debug, Clone, Serialize)]
pub struct PostingBatch {
    pub id: String,
    pub company_id: Uuid,
    pub document_id: String,
    pub doctype: String,
    /// "submit" or "cancel".
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reversal_of: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Per-company item registry entry: the posting engine needs the stock /
/// service distinction, valuation method and account overrides; everything
/// else about an item stays on the sync plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    /// "Stock" (default) or "Service"; anything non-service moves stock,
    /// matching the Dart `isStockItem`.
    #[serde(default = "default_item_type")]
    pub item_type: String,
    /// "Moving Average" (default) or "FIFO".
    #[serde(default)]
    pub valuation_method: Option<String>,
    #[serde(default)]
    pub inventory_account: Option<String>,
    #[serde(default)]
    pub cogs_account: Option<String>,
    #[serde(default)]
    pub stock_adjustment_account: Option<String>,
}

fn default_item_type() -> String {
    "Stock".to_string()
}

/// Company-level posting settings with the same seeded account ids the Dart
/// engine falls back to, so a company that configures nothing posts to the
/// same chart as the Solo client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompanySettings {
    pub allow_negative_stock: bool,
    /// ISO date (YYYY-MM-DD); postings dated on or before it are rejected.
    pub books_lock_date: Option<String>,
    pub default_receivable_account: String,
    pub default_payable_account: String,
    pub default_income_account: String,
    pub default_expense_account: String,
    pub default_cash_account: String,
    pub default_inventory_account: String,
    pub default_cogs_account: String,
    pub default_grni_account: String,
    pub default_stock_adjustment_account: String,
    /// A Stripe Payment Link URL (`https://buy.stripe.com/...`). When set,
    /// the pay page's "Pay by card" button links here with
    /// `?client_reference_id={pay token}` appended — the backend itself never
    /// calls out to Stripe.
    pub stripe_payment_link_url: Option<String>,
    /// Manual payment instructions (bank transfer details etc.) rendered on
    /// the pay page when no Stripe Payment Link is configured.
    pub payment_instructions: Option<String>,
}

impl Default for CompanySettings {
    fn default() -> Self {
        Self {
            allow_negative_stock: false,
            books_lock_date: None,
            default_receivable_account: "Debtors".into(),
            default_payable_account: "Creditors".into(),
            default_income_account: "Sales".into(),
            default_expense_account: "COGS".into(),
            default_cash_account: "Cash".into(),
            default_inventory_account: "Stock".into(),
            default_cogs_account: "COGS".into(),
            default_grni_account: "GRNI".into(),
            default_stock_adjustment_account: "Stock Adjustment".into(),
            stripe_payment_link_url: None,
            payment_instructions: None,
        }
    }
}

/// Everything one command writes, applied by the store as a single atomic
/// transaction. The store also enforces the optimistic-concurrency
/// `sle_expectations` (the engine computed costs/bins from the prior ledger;
/// if it moved, the commit fails `StoreError::Stale` and the command retries)
/// and allocates the official number inside the same transaction so numbering
/// is gap-free under concurrency.
#[derive(Debug, Clone)]
pub struct PostingCommit {
    pub company_id: Uuid,
    pub idempotency_key: Option<String>,
    pub batch: PostingBatch,
    pub document: PostedDocument,
    /// true: insert a new official document (submit); false: update the
    /// existing document to `docstatus`/payload (cancel).
    pub document_is_new: bool,
    /// Present on submit: allocate the next value of this series and stamp
    /// `document.official_number` + the response's `number` field.
    pub series_key: Option<String>,
    pub gl_entries: Vec<GlEntry>,
    pub stock_ledger_entries: Vec<StockLedgerEntry>,
    pub settlements: Vec<Settlement>,
    pub bins: Vec<Bin>,
    /// (doctype, document_id, outstanding_amount) payload maintenance for
    /// invoices referenced by a payment.
    pub outstanding_updates: Vec<(String, String, f64)>,
    /// (item, warehouse, prior SLE row count) the engine's computation was
    /// based on; a mismatch at commit time means a concurrent posting touched
    /// the pair.
    pub sle_expectations: Vec<(String, String, usize)>,
    pub audit: AuditEntry,
    /// Command response; the store stamps `number` after allocation and
    /// persists it under the idempotency key so replays return it verbatim.
    pub response: Value,
}

/// Result of [`PostingCommit`]: the (possibly number-stamped) response, and
/// whether it was replayed from a previously committed idempotency key
/// instead of being applied.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub response: Value,
    pub replayed: bool,
}
