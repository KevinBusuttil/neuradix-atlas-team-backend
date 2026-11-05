//! Posting-authority domain types (Phase 3): official documents, GL entries,
//! stock ledger entries, bins, settlements, posting batches, numbering, the
//! per-company item registry and company settings.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::model::AuditEntry;

/// Doctypes the posting authority accepts. Everything else stays on the
/// draft/sync plane.
pub const POSTED_DOCTYPES: [&str; 10] = [
    "Sales Invoice",
    "Purchase Invoice",
    "Purchase Receipt",
    "Delivery Note",
    "POS Invoice",
    "Payment Entry",
    "Stock Entry",
    "Journal Entry",
    "Expense",
    "Customer Deposit",
];

/// Official-number series key per doctype. One strictly sequential, gap-free
/// series per (company, key); values are assigned only inside a successful
/// submit commit.
pub fn series_key(doctype: &str) -> Option<&'static str> {
    Some(match doctype {
        "Sales Invoice" => "SINV",
        "Purchase Invoice" => "PINV",
        "Purchase Receipt" => "PREC",
        "Delivery Note" => "DN",
        "POS Invoice" => "POS",
        "Payment Entry" => "PAY",
        "Stock Entry" => "STE",
        "Journal Entry" => "JV",
        "Expense" => "EXP",
        "Customer Deposit" => "DEP",
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
    /// Journal Entry dimension (the Dart `_gl` `cost_center`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_center: Option<String>,
    pub voucher_type: String,
    pub voucher_no: String,
    pub posting_date: String,
    /// Transaction currency of the voucher (base-stamped vouchers only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Exchange rate to the company/base currency; stock valuation legs
    /// always carry 1 (valuation cost is already base currency).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversion_rate: Option<f64>,
    /// `debit × conversion_rate`, kept at full precision (not rounded per
    /// leg) so the base ledger balances — the Dart `_stampBaseAmounts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_debit: Option<f64>,
    /// `credit × conversion_rate`, full precision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_credit: Option<f64>,
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
    /// Transaction UOM the line was entered in (`None` = the item's stock
    /// UOM). `qty_change`/`valuation_rate` are already converted to stock
    /// units — the UOM rides along for display parity with the Dart rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uom: Option<String>,
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

/// Which party subledger a [`PartyTransaction`] belongs to. Picks both the
/// derived doctype and the wire field the party id is stored under, matching
/// the Dart `Customer Transaction` / `Supplier Transaction` row shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PartyKind {
    Customer,
    Supplier,
}

impl PartyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PartyKind::Customer => "Customer",
            PartyKind::Supplier => "Supplier",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "Customer" => Some(PartyKind::Customer),
            "Supplier" => Some(PartyKind::Supplier),
            _ => None,
        }
    }

    /// The derived doctype the row replicates as.
    pub fn doctype(&self) -> &'static str {
        match self {
            PartyKind::Customer => "Customer Transaction",
            PartyKind::Supplier => "Supplier Transaction",
        }
    }

    /// The payload key the party id is stored under (Dart uses `customer` /
    /// `supplier`, not a generic `party` field).
    pub fn party_field(&self) -> &'static str {
        match self {
            PartyKind::Customer => "customer",
            PartyKind::Supplier => "supplier",
        }
    }
}

/// Customer / supplier subledger row, ported from the Dart derivation:
/// `CT-{doc id}` (Customer Transaction) and `VT-{doc id}` (Supplier
/// Transaction), positive = the party owes / is owed more, payments negative,
/// reversals negated with a `-reversal` id.
#[derive(Debug, Clone, Serialize)]
pub struct PartyTransaction {
    pub id: String,
    pub company_id: Uuid,
    pub kind: PartyKind,
    /// Invoice / CreditNote / Payment / Adjustment.
    pub trans_type: String,
    pub party: String,
    pub posting_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    pub amount: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Exchange rate to the company/base currency (1 for same-currency).
    pub conversion_rate: f64,
    /// `amount × conversion_rate`, kept at full precision like the Dart
    /// `_stampBaseAmounts`.
    pub base_amount: f64,
    pub voucher_type: String,
    pub voucher_no: String,
    pub is_reversal: bool,
    pub batch_id: String,
}

impl PartyTransaction {
    /// The row's payload fields with the exact Dart derivation field names —
    /// what replicates to client devices and what the command response
    /// carries.
    pub fn row_fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("trans_type".into(), json!(self.trans_type));
        fields.insert(self.kind.party_field().into(), json!(self.party));
        fields.insert("posting_date".into(), json!(self.posting_date));
        if let Some(due_date) = &self.due_date {
            fields.insert("due_date".into(), json!(due_date));
        }
        fields.insert("amount".into(), json!(self.amount));
        if let Some(currency) = &self.currency {
            fields.insert("currency".into(), json!(currency));
        }
        fields.insert("conversion_rate".into(), json!(self.conversion_rate));
        fields.insert("base_amount".into(), json!(self.base_amount));
        fields.insert("voucher_type".into(), json!(self.voucher_type));
        fields.insert("voucher_no".into(), json!(self.voucher_no));
        fields.insert("is_reversal".into(), json!(self.is_reversal));
        fields
    }
}

/// VAT subledger row (`TT-{doc id}-{i}`), one per invoice tax row — the VAT
/// return reads these. A zero-amount tax row still records its taxable base.
#[derive(Debug, Clone, Serialize)]
pub struct TaxTransaction {
    pub id: String,
    pub company_id: Uuid,
    pub tax_type: String,
    /// Tax code (the Dart row's `tax` field, from the line's `tax_code`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax: Option<String>,
    pub posting_date: String,
    /// Taxable base (signed; negated on reversal).
    pub base_amount: f64,
    pub tax_amount: f64,
    pub rate: f64,
    pub party_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
    pub voucher_type: String,
    pub voucher_no: String,
    pub is_reversal: bool,
    pub batch_id: String,
}

impl TaxTransaction {
    /// The row's payload fields with the exact Dart derivation field names.
    pub fn row_fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("tax_type".into(), json!(self.tax_type));
        if let Some(tax) = &self.tax {
            fields.insert("tax".into(), json!(tax));
        }
        fields.insert("posting_date".into(), json!(self.posting_date));
        fields.insert("base_amount".into(), json!(self.base_amount));
        fields.insert("tax_amount".into(), json!(self.tax_amount));
        fields.insert("rate".into(), json!(self.rate));
        fields.insert("party_type".into(), json!(self.party_type));
        if let Some(party) = &self.party {
            fields.insert("party".into(), json!(party));
        }
        fields.insert("voucher_type".into(), json!(self.voucher_type));
        fields.insert("voucher_no".into(), json!(self.voucher_no));
        fields.insert("is_reversal".into(), json!(self.is_reversal));
        fields
    }
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

/// One row of an item's UOM conversion table (the Dart `Item.uoms` /
/// `UOM Conversion Detail` child): an alternative unit plus its factor
/// relative to the item's `stock_uom`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UomConversion {
    pub uom: String,
    pub conversion_factor: f64,
}

/// Per-company item registry entry: the posting engine needs the stock /
/// service distinction, valuation method, UOM conversions and account
/// overrides; everything else about an item stays on the sync plane.
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
    /// The unit the stock ledger and bins track. Unset ⇒ every line posts in
    /// stock units (factor 1), matching the Dart `uomFactor` guard.
    #[serde(default)]
    pub stock_uom: Option<String>,
    /// Alternative UOMs: `[{uom, conversion_factor}]`, factor = stock units
    /// per one of `uom`.
    #[serde(default)]
    pub uoms: Vec<UomConversion>,
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
    /// Liability a Customer Deposit credits (Dart seed `Customer Deposits`).
    pub default_customer_deposit_account: String,
    /// Input VAT account an Expense with VAT and no `tax_account` debits
    /// (Dart seed `VAT`).
    pub default_vat_account: String,
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
            default_customer_deposit_account: "Customer Deposits".into(),
            default_vat_account: "VAT".into(),
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
    /// Offline-fiscal POS confirmation: the till's receipt number, used as
    /// the official number instead of a series allocation. The store refuses
    /// it (409) when another document of the doctype already carries it.
    pub preassigned_number: Option<String>,
    /// Offline-fiscal POS confirmation: (till series, device id). The store
    /// refuses it (409) when the series is already bound to another device.
    pub till_binding: Option<(String, String)>,
    pub gl_entries: Vec<GlEntry>,
    pub stock_ledger_entries: Vec<StockLedgerEntry>,
    pub party_transactions: Vec<PartyTransaction>,
    pub tax_transactions: Vec<TaxTransaction>,
    pub settlements: Vec<Settlement>,
    pub bins: Vec<Bin>,
    /// (doctype, document_id, outstanding_amount) payload maintenance for
    /// invoices referenced by a payment.
    pub outstanding_updates: Vec<(String, String, f64)>,
    /// (item, warehouse, prior SLE row count) the engine's computation was
    /// based on; a mismatch at commit time means a concurrent posting touched
    /// the pair.
    pub sle_expectations: Vec<(String, String, usize)>,
    /// Device id for the mutations this commit replicates onto the company
    /// log; `None` → [`crate::posting::replication::SYSTEM_DEVICE_ID`].
    pub replication_device_id: Option<&'static str>,
    pub audit: AuditEntry,
    /// Command response; the store stamps `number` after allocation and
    /// persists it under the idempotency key so replays return it verbatim.
    pub response: Value,
    /// (series key, last value already issued elsewhere): the store raises
    /// the series so the next official number follows it. Never lowers a
    /// series. Used by the Solo → Team take-on so Team numbering continues
    /// after the last number the Solo book issued.
    pub numbering_floors: Vec<(String, i64)>,
    /// Present only on the Solo → Team take-on: the store refuses the commit
    /// unless the company holds no official document and no earlier
    /// migration, and records the migration in the same transaction.
    pub solo_migration: Option<SoloMigrationMark>,
    /// C5: the posting date the store re-checks against the books lock
    /// INSIDE the commit, so a lock set between the engine's check and the
    /// commit still wins (the store answers `Stale`, the engine rebuilds and
    /// refuses with the 422).
    pub lock_check_date: Option<String>,
    /// C5: a Customer Deposit invariant the store re-checks inside the
    /// commit (see [`DepositGuard`]).
    pub deposit_guard: Option<DepositGuard>,
}

/// Customer Deposit invariants that depend on OTHER documents, so they are
/// checked again inside the serialized commit (two devices applying the
/// same deposit at once cannot both pass).
#[derive(Debug, Clone)]
pub enum DepositGuard {
    /// A Payment Entry applying `amount` of deposit `deposit_id`: the
    /// deposit is submitted, belongs to `customer`, and its submitted
    /// applications plus this one stay within its amount.
    Apply {
        deposit_id: String,
        customer: String,
        amount: f64,
    },
    /// Cancelling deposit `deposit_id`: no submitted application remains.
    Release { deposit_id: String },
}

/// The one-time take-on record a [`PostingCommit`] carries (see
/// `posting::solo_migration`).
#[derive(Debug, Clone)]
pub struct SoloMigrationMark {
    pub migration_id: String,
    pub package_sha256: String,
}

/// Result of [`PostingCommit`]: the (possibly number-stamped) response, and
/// whether it was replayed from a previously committed idempotency key
/// instead of being applied.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    pub response: Value,
    pub replayed: bool,
}

/// C5: one change of the company's books lock through the authorised
/// command (`commands/set-books-lock`). The store applies it as a
/// compare-and-set on `expected_current`, serialized with every posting
/// commit of the company, and writes `audit` in the same transaction.
#[derive(Debug, Clone)]
pub struct BooksLockChange {
    /// The lock the caller saw (`None` = no lock). A stale view is refused.
    pub expected_current: Option<String>,
    /// The new lock (`None` clears it).
    pub lock_date: Option<String>,
    pub audit: AuditEntry,
}

/// What [`BooksLockChange`] did. `changed` is false when the lock already
/// had the requested value (a retried command), which writes nothing.
#[derive(Debug, Clone, Serialize)]
pub struct BooksLockOutcome {
    pub previous: Option<String>,
    pub lock_date: Option<String>,
    pub changed: bool,
}

/// Why a [`DepositGuard`] fails against the current official documents, or
/// `None` when it holds. Shared by the engine (before the commit) and both
/// stores (inside it) so the two checks can never disagree.
pub fn deposit_guard_violation(
    guard: &DepositGuard,
    deposit: Option<&PostedDocument>,
    applications: &[PostedDocument],
) -> Option<String> {
    use crate::posting::values::{as_non_empty, as_num};
    const MONEY_EPS: f64 = 0.005;
    match guard {
        DepositGuard::Apply {
            deposit_id,
            customer,
            amount,
        } => {
            let Some(deposit) = deposit.filter(|d| d.docstatus == 1) else {
                return Some(format!(
                    "deposit {deposit_id} is not submitted; submit it before applying it"
                ));
            };
            let owner = as_non_empty(deposit.payload.get("customer")).unwrap_or_default();
            if &owner != customer {
                return Some(format!(
                    "deposit {deposit_id} belongs to {owner}, not {customer}"
                ));
            }
            let total = as_num(deposit.payload.get("amount"));
            let applied: f64 = applications
                .iter()
                .filter(|p| p.docstatus == 1)
                .map(|p| as_num(p.payload.get("paid_amount")))
                .sum();
            let remaining = total - applied;
            (*amount > remaining + MONEY_EPS).then(|| {
                format!(
                    "deposit {deposit_id} has {remaining:.2} left; \
                     this application of {amount:.2} exceeds it"
                )
            })
        }
        DepositGuard::Release { deposit_id } => {
            let live: Vec<&str> = applications
                .iter()
                .filter(|p| p.docstatus == 1)
                .map(|p| p.id.as_str())
                .collect();
            (!live.is_empty()).then(|| {
                format!(
                    "deposit {deposit_id} is applied by {}; cancel those payment \
                     entries first",
                    live.join(", ")
                )
            })
        }
    }
}
