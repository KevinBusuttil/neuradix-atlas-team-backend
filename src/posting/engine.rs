//! Command orchestration: builds one atomic [`PostingCommit`] per official
//! action (submit / cancel), semantically ported from the Dart
//! `LedgerDerivation` + `LedgerDerivationService` pair so both engines pass
//! the shared fixture suite.
//!
//! Submit derives GL + stock ledger rows forward, costs issues from the
//! authoritative SLE history (moving average / FIFO, `stock.rs`), posts the
//! perpetual-inventory GL counterpart of every costed movement, splits a
//! Purchase Invoice's stock value off the expense leg onto GRNI, validates
//! (balance, negative stock, period lock) and recomputes the touched bins.
//! Cancel mirrors the stored rows exactly — negated quantities at the
//! original stored rates (never re-costed), swapped GL columns, `-reversal`
//! ids — so a cancellation backs out precisely what was posted.

use std::collections::{BTreeMap, HashMap};

use chrono::Utc;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::model::AuditEntry;
use crate::posting::model::{
    series_key, Bin, CommitOutcome, CompanySettings, GlEntry, Item, PartyKind, PartyTransaction,
    PostingBatch, PostingCommit, Settlement, StockLedgerEntry, TaxTransaction, POSTED_DOCTYPES,
};
use crate::posting::stock::{compute_balance, issue_rate, LedgerRow};
use crate::posting::values::{
    as_non_empty, as_num, is_stock_item_type, is_true, outstanding_amount, round2, uom_factor,
    REVERSAL_SUFFIX,
};
use crate::store::{Store, StoreError};

/// Amounts below this are treated as zero (mirrors the Dart 1e-7 epsilon).
const EPS: f64 = 1e-7;
/// Half-cent tolerance for the money cross-checks (per line / per stated
/// total) — the same sub-cent tolerance as the JE balance guard and the
/// settlement paid-epsilon.
const MONEY_EPS: f64 = 0.005;
/// JE-style balance guard tolerance on the generated GL.
const BALANCE_TOLERANCE: f64 = 0.005;
/// `tax_type` of a withholding tax row: deducted from the document total
/// (carried with a negative `tax_amount`), mirroring the Dart
/// `HubTaxEngine.withholdingType`.
const WITHHOLDING_TAX_TYPE: &str = "Withholding";
/// Stale-state retries before giving up under pathological contention.
const MAX_RETRIES: usize = 16;

#[derive(Debug)]
pub enum PostingError {
    /// 422 — the command is well-formed but violates an accounting rule.
    Validation(String),
    /// 404 — the referenced document does not exist.
    NotFound,
    /// 409 — duplicate id / wrong docstatus / exhausted retries.
    Conflict(String),
    Store(StoreError),
}

impl From<StoreError> for PostingError {
    fn from(err: StoreError) -> Self {
        PostingError::Store(err)
    }
}

/// Who is acting, for the audit row written inside the commit.
#[derive(Debug, Clone, Copy)]
pub struct Actor {
    /// The acting user. `None` is the **system actor** — commands issued by
    /// the backend itself (the Stripe webhook processor), with no user behind
    /// them; audit rows carry a null user, matching how replication-authored
    /// mutations carry an empty user id.
    pub user_id: Option<Uuid>,
    pub device_id: Option<Uuid>,
    /// Device id stamped on the mutations this commit replicates onto the
    /// company log; `None` means the default
    /// [`crate::posting::replication::SYSTEM_DEVICE_ID`]. System actors set
    /// their own (e.g. `atlas-payments`) so the provenance of a posting is
    /// visible in the log.
    pub replication_device_id: Option<&'static str>,
}

impl Actor {
    /// A user acting through the command API.
    pub fn user(user_id: Uuid, device_id: Option<Uuid>) -> Self {
        Self {
            user_id: Some(user_id),
            device_id,
            replication_device_id: None,
        }
    }

    /// A backend system actor: no user, no device, mutations replicated
    /// under the given device id.
    pub fn system(replication_device_id: &'static str) -> Self {
        Self {
            user_id: None,
            device_id: None,
            replication_device_id: Some(replication_device_id),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubmitCommand {
    pub doctype: String,
    /// Client-supplied id (deterministic fixtures / offline drafts) or
    /// server-generated when absent.
    pub document_id: Option<String>,
    pub payload: Map<String, Value>,
    pub items: Vec<Value>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CancelCommand {
    pub doctype: String,
    pub document_id: String,
    pub idempotency_key: Option<String>,
}

/// Roles allowed to submit/cancel each official doctype (server-enforced).
pub fn allowed_roles(doctype: &str) -> Option<&'static [crate::model::Role]> {
    use crate::model::Role::{Accountant, Admin, Owner, Pos, Purchasing, Sales, Stock};
    Some(match doctype {
        "Sales Invoice" => &[Owner, Admin, Sales],
        "Purchase Invoice" | "Purchase Receipt" => &[Owner, Admin, Purchasing],
        "Delivery Note" => &[Owner, Admin, Stock, Sales],
        "POS Invoice" => &[Owner, Admin, Pos],
        "Stock Entry" => &[Owner, Admin, Stock],
        "Payment Entry" => &[Owner, Admin, Accountant],
        _ => return None,
    })
}

fn today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

/// Earliest year an official document may be dated in.
const MIN_POSTING_YEAR: i32 = 1900;
/// Latest year an official document may be dated in.
const MAX_POSTING_YEAR: i32 = 2100;

/// The document's `posting_date`, validated: absent / null / blank defaults
/// to today (the existing derive-when-missing behaviour); anything else must
/// be a strict `YYYY-MM-DD` string naming a real calendar date between
/// [`MIN_POSTING_YEAR`] and [`MAX_POSTING_YEAR`]. Everything downstream (the
/// period-lock comparison, GL/SLE stamping, replication) then operates on
/// validated ISO dates only, so lexicographic date comparison is sound.
fn validated_posting_date(payload: &Map<String, Value>) -> Result<String, PostingError> {
    let date = match payload.get("posting_date") {
        None | Some(Value::Null) => return Ok(today()),
        Some(Value::String(s)) if s.trim().is_empty() => return Ok(today()),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => {
            return Err(PostingError::Validation(format!(
                "posting_date must be a YYYY-MM-DD string, got {other}"
            )))
        }
    };
    let bytes = date.as_bytes();
    let well_formed = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit());
    let real_date = well_formed && {
        // Well-formed guarantees the parses succeed; from_ymd_opt rejects
        // impossible calendar dates (2026-02-30, month 13, day 0, …).
        let year: i32 = date[0..4].parse().unwrap_or_default();
        let month: u32 = date[5..7].parse().unwrap_or_default();
        let day: u32 = date[8..10].parse().unwrap_or_default();
        (MIN_POSTING_YEAR..=MAX_POSTING_YEAR).contains(&year)
            && chrono::NaiveDate::from_ymd_opt(year, month, day).is_some()
    };
    if !real_date {
        return Err(PostingError::Validation(format!(
            "posting_date {date:?} is not a valid date: expected YYYY-MM-DD \
             between {MIN_POSTING_YEAR}-01-01 and {MAX_POSTING_YEAR}-12-31"
        )));
    }
    Ok(date)
}

// ---------------------------------------------------------------------------
// Public entry points (retry loop around build + atomic commit)
// ---------------------------------------------------------------------------

pub async fn submit_document(
    store: &dyn Store,
    company_id: Uuid,
    cmd: SubmitCommand,
    actor: Actor,
) -> Result<CommitOutcome, PostingError> {
    if !POSTED_DOCTYPES.contains(&cmd.doctype.as_str()) {
        return Err(PostingError::Validation(format!(
            "unsupported doctype {}",
            cmd.doctype
        )));
    }
    if let Some(outcome) = replay(store, company_id, cmd.idempotency_key.as_deref()).await? {
        return Ok(outcome);
    }
    // Fixed across retries so a stale-state recompute posts the same document.
    let document_id = cmd
        .document_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    for _ in 0..MAX_RETRIES {
        let commit = build_submit(store, company_id, &cmd, &document_id, actor).await?;
        match store.posting_commit(commit).await {
            Ok(outcome) => return Ok(outcome),
            Err(StoreError::Stale(_)) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(PostingError::Conflict(
        "posting contention: stock ledger kept moving; retry".into(),
    ))
}

pub async fn cancel_document(
    store: &dyn Store,
    company_id: Uuid,
    cmd: CancelCommand,
    actor: Actor,
) -> Result<CommitOutcome, PostingError> {
    if let Some(outcome) = replay(store, company_id, cmd.idempotency_key.as_deref()).await? {
        return Ok(outcome);
    }
    for _ in 0..MAX_RETRIES {
        let commit = build_cancel(store, company_id, &cmd, actor).await?;
        match store.posting_commit(commit).await {
            Ok(outcome) => return Ok(outcome),
            Err(StoreError::Stale(_)) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    Err(PostingError::Conflict(
        "posting contention: stock ledger kept moving; retry".into(),
    ))
}

async fn replay(
    store: &dyn Store,
    company_id: Uuid,
    key: Option<&str>,
) -> Result<Option<CommitOutcome>, PostingError> {
    match key {
        Some(key) => Ok(store
            .idempotent_response(company_id, key)
            .await?
            .map(|response| CommitOutcome {
                response,
                replayed: true,
            })),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Submit
// ---------------------------------------------------------------------------

async fn build_submit(
    store: &dyn Store,
    company_id: Uuid,
    cmd: &SubmitCommand,
    document_id: &str,
    actor: Actor,
) -> Result<PostingCommit, PostingError> {
    let settings = store.company_settings(company_id).await?;
    let doctype = cmd.doctype.as_str();

    let mut payload = cmd.payload.clone();
    payload.insert("items".into(), Value::Array(cmd.items.clone()));
    let posting_date = validated_posting_date(&payload)?;
    payload.insert("posting_date".into(), json!(posting_date));
    check_period_lock(&settings, &posting_date)?;

    if store
        .posted_document(company_id, doctype, document_id)
        .await?
        .is_some()
    {
        return Err(PostingError::Conflict(format!(
            "document {doctype} {document_id} was already submitted"
        )));
    }

    resolve_account_fallbacks(doctype, &mut payload, &settings);
    if matches!(doctype, "Sales Invoice" | "Purchase Invoice") {
        validate_invoice_totals(&mut payload, true)?;
    } else if doctype == "POS Invoice" {
        // Payment is captured inline via `tenders` — no outstanding to track.
        validate_invoice_totals(&mut payload, false)?;
    }

    let batch_id = format!("PB-{document_id}");
    let batch = PostingBatch {
        id: batch_id.clone(),
        company_id,
        document_id: document_id.to_string(),
        doctype: doctype.to_string(),
        kind: "submit".into(),
        reversal_of: None,
        created_at: Utc::now(),
    };

    // Forward derivation: monetary GL legs + raw (uncosted) SLE rows.
    let ctx = RowContext {
        company_id,
        document_id,
        doctype,
        posting_date: &posting_date,
        batch_id: &batch_id,
    };
    let mut gl = derive_monetary_gl(&ctx, &payload);
    let mut sles = derive_stock_rows(&ctx, &payload);
    let (mut party_transactions, tax_transactions) = derive_subledger_rows(&ctx, &payload);
    // Multi-currency: base-stamp the transaction-currency rows before the
    // stock GL legs join (those are already base currency, rate 1) — the
    // Dart `derive()` → `_stockGlLegs` ordering.
    if is_base_stamped(doctype) {
        stamp_base_amounts(&mut gl, &mut party_transactions, &payload);
    }

    // Item registry: drop service-item movements before costing (they never
    // touch stock), then cost issues from the authoritative ledger.
    let items = load_items(store, company_id, &sles, &payload).await?;
    sles.retain(|sle| is_stock_item_type(items.get(&sle.item).map(|item| item.item_type.as_str())));
    let mut ledgers = Ledgers::default();
    cost_stock_movements(store, company_id, &mut sles, &items, &payload, &mut ledgers).await?;

    // Perpetual inventory: every costed movement posts its GL counterpart,
    // and a Purchase Invoice's stock value moves from the expense leg to GRNI.
    gl.extend(stock_gl_legs(&ctx, &sles, &items, &settings));
    if doctype == "Purchase Invoice" {
        split_grni_from_expense(&ctx, &mut gl, &payload, &items, &settings);
    }

    check_balanced(&gl)?;
    check_negative_stock(&settings, &ledgers)?;

    // Settlements + invoice outstanding maintenance.
    let mut settlements = Vec::new();
    let mut outstanding_updates = Vec::new();
    if doctype == "Payment Entry" {
        settlements = derive_settlements(&ctx, &payload);
        outstanding_updates = payment_outstanding_updates(store, company_id, &settlements).await?;
    }

    let bins = recompute_bins(company_id, &ledgers);
    let document = crate::posting::model::PostedDocument {
        id: document_id.to_string(),
        company_id,
        doctype: doctype.to_string(),
        payload: Value::Object(payload),
        docstatus: 1,
        official_number: None, // allocated inside the commit
        created_at: Utc::now(),
    };
    let (party_rows, tax_rows) = subledger_response_rows(&party_transactions, &tax_transactions);
    let response = json!({
        "document_id": document_id,
        "number": Value::Null, // stamped by the store after allocation
        "docstatus": 1,
        "gl_entries": &gl,
        "stock_ledger_entries": &sles,
        "party_transactions": party_rows,
        "tax_transactions": tax_rows,
        "bins": &bins,
        "settlements": &settlements,
    });

    Ok(PostingCommit {
        company_id,
        idempotency_key: cmd.idempotency_key.clone(),
        batch,
        document,
        document_is_new: true,
        series_key: series_key(doctype).map(str::to_string),
        gl_entries: gl,
        stock_ledger_entries: sles,
        party_transactions,
        tax_transactions,
        settlements,
        bins,
        outstanding_updates,
        sle_expectations: ledgers.expectations(),
        replication_device_id: actor.replication_device_id,
        audit: audit_row(
            company_id,
            actor,
            "command.submit-document",
            json!({ "doctype": doctype, "documentId": document_id }),
        ),
        response,
    })
}

// ---------------------------------------------------------------------------
// Cancel — mirror the stored rows exactly (never re-cost)
// ---------------------------------------------------------------------------

async fn build_cancel(
    store: &dyn Store,
    company_id: Uuid,
    cmd: &CancelCommand,
    actor: Actor,
) -> Result<PostingCommit, PostingError> {
    let settings = store.company_settings(company_id).await?;
    let mut document = store
        .posted_document(company_id, &cmd.doctype, &cmd.document_id)
        .await?
        .ok_or(PostingError::NotFound)?;
    if document.docstatus != 1 {
        return Err(PostingError::Conflict(format!(
            "document {} {} is not submitted (docstatus {})",
            cmd.doctype, cmd.document_id, document.docstatus
        )));
    }
    // The stored date was validated on submit; re-validating on cancel keeps
    // the period-lock comparison fail-closed for documents that predate the
    // validation.
    let posting_date = match document.payload.as_object() {
        Some(payload) => validated_posting_date(payload)?,
        None => today(),
    };
    check_period_lock(&settings, &posting_date)?;

    let document_id = cmd.document_id.as_str();
    let batch_id = format!("PB-{document_id}{REVERSAL_SUFFIX}");
    let batch = PostingBatch {
        id: batch_id.clone(),
        company_id,
        document_id: document_id.to_string(),
        doctype: cmd.doctype.clone(),
        kind: "cancel".into(),
        reversal_of: Some(format!("PB-{document_id}")),
        created_at: Utc::now(),
    };

    // Reversal legs: negate the stored originals with `-reversal` ids. The
    // stored SLE rates are reused verbatim so issues are reversed at their
    // original cost.
    let gl: Vec<GlEntry> = store
        .gl_for_voucher(company_id, document_id)
        .await?
        .into_iter()
        .filter(|entry| !entry.is_reversal)
        .map(|entry| GlEntry {
            id: format!("{}{REVERSAL_SUFFIX}", entry.id),
            debit: entry.credit,
            credit: entry.debit,
            // Base amounts mirror their transaction columns' swap.
            base_debit: entry.base_credit,
            base_credit: entry.base_debit,
            is_reversal: true,
            batch_id: batch_id.clone(),
            ..entry
        })
        .collect();
    let sles: Vec<StockLedgerEntry> = store
        .sles_for_voucher(company_id, document_id)
        .await?
        .into_iter()
        .filter(|sle| !sle.is_reversal)
        .map(|sle| StockLedgerEntry {
            id: format!("{}{REVERSAL_SUFFIX}", sle.id),
            qty_change: -sle.qty_change,
            is_reversal: true,
            batch_id: batch_id.clone(),
            seq: 0,
            ..sle
        })
        .collect();
    let settlements: Vec<Settlement> = store
        .settlements_for_payment(company_id, document_id)
        .await?
        .into_iter()
        .filter(|s| !s.is_reversal)
        .map(|s| Settlement {
            id: format!("{}{REVERSAL_SUFFIX}", s.id),
            allocated_amount: -s.allocated_amount,
            is_reversal: true,
            batch_id: batch_id.clone(),
            ..s
        })
        .collect();
    // Party subledger reversals negate the stored amounts and take the Dart
    // cancel trans_types: an Invoice reverses as a CreditNote, a Payment as
    // an Adjustment.
    let party_transactions: Vec<PartyTransaction> = store
        .party_transactions_for_voucher(company_id, document_id)
        .await?
        .into_iter()
        .filter(|t| !t.is_reversal)
        .map(|t| PartyTransaction {
            id: format!("{}{REVERSAL_SUFFIX}", t.id),
            trans_type: if t.trans_type == "Invoice" {
                "CreditNote".to_string()
            } else {
                "Adjustment".to_string()
            },
            amount: -t.amount,
            base_amount: -t.base_amount,
            is_reversal: true,
            batch_id: batch_id.clone(),
            ..t
        })
        .collect();
    let tax_transactions: Vec<TaxTransaction> = store
        .tax_transactions_for_voucher(company_id, document_id)
        .await?
        .into_iter()
        .filter(|t| !t.is_reversal)
        .map(|t| TaxTransaction {
            id: format!("{}{REVERSAL_SUFFIX}", t.id),
            base_amount: -t.base_amount,
            tax_amount: -t.tax_amount,
            is_reversal: true,
            batch_id: batch_id.clone(),
            ..t
        })
        .collect();

    // Replay the reversal rows onto the current ledger for the negative-stock
    // guard and the bin recompute (e.g. cancelling a receipt whose goods were
    // already sold would drive stock negative).
    let mut ledgers = Ledgers::default();
    for sle in &sles {
        let prior = ledgers
            .prior_for(store, company_id, &sle.item, &sle.warehouse)
            .await?;
        prior.push(LedgerRow {
            qty_change: sle.qty_change,
            valuation_rate: sle.valuation_rate,
        });
    }
    check_negative_stock(&settings, &ledgers)?;
    let bins = recompute_bins(company_id, &ledgers);

    let outstanding_updates = if cmd.doctype == "Payment Entry" {
        payment_outstanding_updates(store, company_id, &settlements).await?
    } else {
        Vec::new()
    };

    document.docstatus = 2;
    let (party_rows, tax_rows) = subledger_response_rows(&party_transactions, &tax_transactions);
    let response = json!({
        "document_id": document_id,
        "number": &document.official_number,
        "docstatus": 2,
        "gl_entries": &gl,
        "stock_ledger_entries": &sles,
        "party_transactions": party_rows,
        "tax_transactions": tax_rows,
        "bins": &bins,
        "settlements": &settlements,
    });

    Ok(PostingCommit {
        company_id,
        idempotency_key: cmd.idempotency_key.clone(),
        batch,
        document,
        document_is_new: false,
        series_key: None, // cancellation never consumes a number
        gl_entries: gl,
        stock_ledger_entries: sles,
        party_transactions,
        tax_transactions,
        settlements,
        bins,
        outstanding_updates,
        sle_expectations: ledgers.expectations(),
        replication_device_id: actor.replication_device_id,
        audit: audit_row(
            company_id,
            actor,
            "command.cancel-document",
            json!({ "doctype": cmd.doctype, "documentId": document_id }),
        ),
        response,
    })
}

// ---------------------------------------------------------------------------
// Forward derivation (port of the Dart `LedgerDerivation`, submit side)
// ---------------------------------------------------------------------------

struct RowContext<'a> {
    company_id: Uuid,
    document_id: &'a str,
    doctype: &'a str,
    posting_date: &'a str,
    batch_id: &'a str,
}

impl RowContext<'_> {
    #[allow(clippy::too_many_arguments)]
    fn gl(
        &self,
        id: String,
        account: String,
        debit: f64,
        credit: f64,
        party_type: Option<&str>,
        party: Option<String>,
    ) -> GlEntry {
        GlEntry {
            id,
            company_id: self.company_id,
            account,
            debit,
            credit,
            party_type: party_type.map(str::to_string),
            party,
            voucher_type: self.doctype.to_string(),
            voucher_no: self.document_id.to_string(),
            posting_date: self.posting_date.to_string(),
            currency: None,
            conversion_rate: None,
            base_debit: None,
            base_credit: None,
            is_reversal: false,
            batch_id: self.batch_id.to_string(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn sle(
        &self,
        id: String,
        trans_type: &str,
        item: String,
        warehouse: String,
        qty_change: f64,
        valuation_rate: f64,
        uom: Option<String>,
    ) -> StockLedgerEntry {
        StockLedgerEntry {
            id,
            company_id: self.company_id,
            trans_type: trans_type.to_string(),
            item,
            warehouse,
            qty_change,
            valuation_rate,
            voucher_type: self.doctype.to_string(),
            voucher_no: self.document_id.to_string(),
            posting_date: self.posting_date.to_string(),
            uom,
            is_reversal: false,
            batch_id: self.batch_id.to_string(),
            seq: 0, // assigned by the store at commit
        }
    }
}

fn items_of(payload: &Map<String, Value>) -> &[Value] {
    match payload.get("items") {
        Some(Value::Array(items)) => items,
        _ => &[],
    }
}

/// The line's monetary amount: the sent `amount` when present (a null reads
/// as absent), else `round2(qty × rate)` — the Dart client computes
/// `amount = qty * rate` per line. Sent amounts are cross-checked against
/// the recomputation in [`validate_invoice_totals`] before this is trusted.
fn line_amount(line: &Map<String, Value>) -> f64 {
    match line.get("amount") {
        Some(v) if !v.is_null() => as_num(Some(v)),
        _ => round2(as_num(line.get("qty")) * as_num(line.get("rate"))),
    }
}

fn total_tax(payload: &Map<String, Value>) -> f64 {
    match payload.get("taxes") {
        Some(Value::Array(rows)) => rows
            .iter()
            .filter_map(Value::as_object)
            .map(|row| as_num(row.get("tax_amount")))
            .sum(),
        _ => 0.0,
    }
}

/// Blank posting accounts resolve from the company defaults (the Dart
/// `accountFallbacks` map), so a minimal voucher still posts balanced GL.
fn resolve_account_fallbacks(
    doctype: &str,
    payload: &mut Map<String, Value>,
    settings: &CompanySettings,
) {
    let fallbacks: &[(&str, &str)] = match doctype {
        "Sales Invoice" => &[("debit_to", "receivable"), ("income_account", "income")],
        "Purchase Invoice" => &[("credit_to", "payable"), ("expense_account", "expense")],
        "POS Invoice" => &[("cash_account", "cash"), ("income_account", "income")],
        "Payment Entry" => match as_non_empty(payload.get("payment_type")).as_deref() {
            Some("Receive") => &[("paid_from", "receivable"), ("paid_to", "cash")],
            Some("Pay") => &[("paid_from", "cash"), ("paid_to", "payable")],
            _ => &[],
        },
        _ => &[],
    };
    for (field, default) in fallbacks {
        if as_non_empty(payload.get(*field)).is_none() {
            let value = match *default {
                "receivable" => &settings.default_receivable_account,
                "payable" => &settings.default_payable_account,
                "income" => &settings.default_income_account,
                "expense" => &settings.default_expense_account,
                _ => &settings.default_cash_account,
            };
            payload.insert((*field).to_string(), json!(value));
        }
    }
}

/// A payload money field the client actually sent: absent, null and blank
/// read as "not sent" (the server derives); anything else is coerced through
/// [`as_num`] — the same coercion every downstream reader applies — so the
/// value that gets validated is exactly the value that would post.
fn sent_number(payload: &Map<String, Value>, field: &str) -> Option<f64> {
    match payload.get(field) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.trim().is_empty() => None,
        Some(v) => Some(as_num(Some(v))),
    }
}

/// Server-side recomputation of the invoice totals, mirroring the Dart
/// `LineItemTotalsInterceptor` + `TaxCalculationInterceptor` /
/// `HubTaxEngine.compute` exactly, so a well-behaved client always passes:
///
/// * each line: `amount = round2(qty × rate)` — a sent `amount` must match
///   within [`MONEY_EPS`], an absent one is derived;
/// * exclusive pricing: `total` = round2(Σ line amounts); inclusive pricing
///   (`prices_include_tax` truthy): the line amounts are gross, the tax rows
///   carry the extracted tax, so `total` = round2(Σ lines − Σ extracted) and
///   Σ line amounts == `grand_total`;
/// * `tax_total` = round2(Σ tax row amounts) (withholding rows negative);
/// * `grand_total` = round2(total + tax_total).
///
/// Sent `total` / `tax_total` / `grand_total` must match the recomputation
/// (422 quoting expected vs sent); absent ones keep the derive-when-missing
/// behaviour. `outstanding_amount` is always re-initialised from the
/// validated grand total — a client-sent outstanding is never trusted.
fn validate_invoice_totals(
    payload: &mut Map<String, Value>,
    track_outstanding: bool,
) -> Result<(), PostingError> {
    let inclusive = is_true(payload.get("prices_include_tax"));
    let mut lines_sum = 0.0;
    let mut line_count: usize = 0;
    for (index, line) in items_of(payload).iter().enumerate() {
        let Some(line) = line.as_object() else {
            continue;
        };
        let derived = round2(as_num(line.get("qty")) * as_num(line.get("rate")));
        let amount = match line.get("amount") {
            None | Some(Value::Null) => derived,
            Some(v) => {
                let sent = as_num(Some(v));
                if (sent - derived).abs() > MONEY_EPS {
                    return Err(PostingError::Validation(format!(
                        "items[{index}]: amount {sent} does not match qty × rate = {derived}"
                    )));
                }
                sent
            }
        };
        lines_sum += amount;
        line_count += 1;
    }

    // Tax rows are validated in detail by `validate_tax_rows`; here they feed
    // the document totals.
    let mut tax_sum = 0.0;
    let mut extracted = 0.0; // inclusive: tax contained in the line amounts
    if let Some(Value::Array(rows)) = payload.get("taxes") {
        for row in rows.iter().filter_map(Value::as_object) {
            let tax_amount = as_num(row.get("tax_amount"));
            tax_sum += tax_amount;
            let withholding =
                as_non_empty(row.get("tax_type")).as_deref() == Some(WITHHOLDING_TAX_TYPE);
            if inclusive && !withholding {
                extracted += tax_amount;
            }
        }
    }
    let computed_total = round2(lines_sum - extracted);
    let computed_tax = round2(tax_sum);
    let computed_grand = round2(computed_total + computed_tax);
    // Per-line rounding can drift the sums by up to half a cent per line.
    let line_tolerance = MONEY_EPS * line_count.max(1) as f64;

    let total = match sent_number(payload, "total") {
        Some(sent) => {
            if (sent - computed_total).abs() > line_tolerance {
                return Err(PostingError::Validation(format!(
                    "total: sent {sent}, expected {computed_total} (Σ line amounts{})",
                    if inclusive { " − extracted tax" } else { "" }
                )));
            }
            sent
        }
        None => computed_total,
    };
    let tax_total = match sent_number(payload, "tax_total") {
        Some(sent) => {
            if (sent - computed_tax).abs() > MONEY_EPS {
                return Err(PostingError::Validation(format!(
                    "tax_total: sent {sent}, expected {computed_tax} (Σ tax row amounts)"
                )));
            }
            sent
        }
        None => computed_tax,
    };
    let grand = match sent_number(payload, "grand_total") {
        Some(sent) => {
            if (sent - computed_grand).abs() > line_tolerance {
                return Err(PostingError::Validation(format!(
                    "grand_total: sent {sent}, expected {computed_grand} (total + tax_total)"
                )));
            }
            sent
        }
        None => {
            payload.insert("grand_total".into(), json!(computed_grand));
            computed_grand
        }
    };
    // The identity itself, over the resolved values (a rounded sum may sit
    // half a cent from its unrounded parts).
    if (grand - (total + tax_total)).abs() > 2.0 * MONEY_EPS {
        return Err(PostingError::Validation(format!(
            "grand_total {grand} does not equal total {total} + tax_total {tax_total}"
        )));
    }

    // Derived state is never accepted from the client: the outstanding an
    // invoice starts with is exactly its validated grand total.
    payload.remove("outstanding_amount");
    if track_outstanding {
        payload.insert("outstanding_amount".into(), json!(grand));
    }
    Ok(())
}

/// Monetary GL legs per doctype (receivable/payable, income/expense, VAT,
/// payment source/destination). Stock GL legs are added after costing.
fn derive_monetary_gl(ctx: &RowContext<'_>, payload: &Map<String, Value>) -> Vec<GlEntry> {
    let id = ctx.document_id;
    match ctx.doctype {
        "Sales Invoice" => {
            let grand = as_num(payload.get("grand_total"));
            let net = grand - total_tax(payload);
            let customer = as_non_empty(payload.get("customer"));
            let mut gl = vec![
                // Dr Accounts Receivable (gross — customer owes net + VAT)
                ctx.gl(
                    format!("GL-{id}-debit"),
                    as_non_empty(payload.get("debit_to")).unwrap_or_default(),
                    grand,
                    0.0,
                    Some("Customer"),
                    customer,
                ),
                // Cr Income (net of tax)
                ctx.gl(
                    format!("GL-{id}-credit"),
                    as_non_empty(payload.get("income_account")).unwrap_or_default(),
                    0.0,
                    net,
                    None,
                    None,
                ),
            ];
            gl.extend(tax_legs(ctx, payload, true));
            gl
        }
        "Purchase Invoice" => {
            let grand = as_num(payload.get("grand_total"));
            let net = grand - total_tax(payload);
            let supplier = as_non_empty(payload.get("supplier"));
            let mut gl = vec![
                // Cr Accounts Payable (gross — we owe net + VAT)
                ctx.gl(
                    format!("GL-{id}-credit"),
                    as_non_empty(payload.get("credit_to")).unwrap_or_default(),
                    0.0,
                    grand,
                    Some("Supplier"),
                    supplier,
                ),
                // Dr Expense (net of tax); stock lines move to GRNI later.
                ctx.gl(
                    format!("GL-{id}-debit"),
                    as_non_empty(payload.get("expense_account")).unwrap_or_default(),
                    net,
                    0.0,
                    None,
                    None,
                ),
            ];
            gl.extend(tax_legs(ctx, payload, false));
            gl
        }
        // Cash sale: Dr Cash / Cr Income (net) + output VAT. No receivable and
        // no party GL fields — payment is captured inline; `tenders` never
        // post (the Dart `_posInvoice`).
        "POS Invoice" => {
            let grand = as_num(payload.get("grand_total"));
            let net = grand - total_tax(payload);
            let mut gl = vec![
                // Dr Cash / Bank — gross received.
                ctx.gl(
                    format!("GL-{id}-cash"),
                    as_non_empty(payload.get("cash_account")).unwrap_or_default(),
                    grand,
                    0.0,
                    None,
                    None,
                ),
                // Cr Income — net of tax.
                ctx.gl(
                    format!("GL-{id}-income"),
                    as_non_empty(payload.get("income_account")).unwrap_or_default(),
                    0.0,
                    net,
                    None,
                    None,
                ),
            ];
            gl.extend(tax_legs(ctx, payload, true));
            gl
        }
        "Payment Entry" => {
            let paid = as_num(payload.get("paid_amount"));
            vec![
                // Cr the source account (money leaves)
                ctx.gl(
                    format!("GL-{id}-from"),
                    as_non_empty(payload.get("paid_from")).unwrap_or_default(),
                    0.0,
                    paid,
                    None,
                    None,
                ),
                // Dr the destination account (money arrives)
                ctx.gl(
                    format!("GL-{id}-to"),
                    as_non_empty(payload.get("paid_to")).unwrap_or_default(),
                    paid,
                    0.0,
                    None,
                    None,
                ),
            ]
        }
        // Purchase Receipt / Stock Entry: GL comes only from costed stock
        // movements.
        _ => Vec::new(),
    }
}

/// One VAT GL leg per invoice tax row: output VAT credited on sales, input
/// VAT debited on purchases; zero-amount rows post nothing.
fn tax_legs(ctx: &RowContext<'_>, payload: &Map<String, Value>, is_output: bool) -> Vec<GlEntry> {
    let rows = match payload.get("taxes") {
        Some(Value::Array(rows)) => rows.as_slice(),
        _ => &[],
    };
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(row) = row.as_object() else { continue };
        let tax_amount = as_num(row.get("tax_amount"));
        let Some(account) = as_non_empty(row.get("tax_account")) else {
            continue;
        };
        if tax_amount.abs() < EPS {
            continue;
        }
        let id = ctx.document_id;
        let (debit, credit) = if is_output {
            (0.0, tax_amount)
        } else {
            (tax_amount, 0.0)
        };
        out.push(ctx.gl(
            format!("GL-{id}-tax-{i}"),
            account,
            debit,
            credit,
            None,
            None,
        ));
    }
    out
}

/// Vouchers whose monetary rows are denominated in the document's
/// transaction `currency`, so base-currency amounts get stamped onto them —
/// the Dart `_baseStampDocTypes` minus Journal Entry (not a posted doctype
/// here yet). Stock vouchers (POS / Delivery / Receipt / Stock Entry) carry
/// their cost on SLEs at valuation cost — already base currency — so they
/// are excluded; their GL legs are emitted after costing with rate 1.
fn is_base_stamped(doctype: &str) -> bool {
    matches!(
        doctype,
        "Sales Invoice" | "Purchase Invoice" | "Payment Entry"
    )
}

/// The document's exchange rate to the company/base currency
/// (`conversion_rate`), defaulting to 1 when absent or non-positive — so a
/// same-currency voucher posts base == transaction (Dart `conversionRate`).
fn conversion_rate_of(payload: &Map<String, Value>) -> f64 {
    let rate = as_num(payload.get("conversion_rate"));
    if rate > 0.0 {
        rate
    } else {
        1.0
    }
}

/// Stamps base-currency amounts onto a base-stamped voucher's
/// transaction-currency rows (the Dart `_stampBaseAmounts`): GL legs get
/// `conversion_rate` + `base_debit`/`base_credit` + `currency`; customer /
/// supplier subledger rows get `conversion_rate` + `base_amount`. Base
/// amounts are kept at full precision (NOT rounded per leg): the transaction
/// ledger balances, so unrounded products keep the base ledger balanced too.
/// Tax / settlement / stock rows deliberately stay in transaction currency.
fn stamp_base_amounts(
    gl: &mut [GlEntry],
    party_transactions: &mut [PartyTransaction],
    payload: &Map<String, Value>,
) {
    let rate = conversion_rate_of(payload);
    let currency = as_non_empty(payload.get("currency"));
    for entry in gl.iter_mut() {
        entry.conversion_rate = Some(rate);
        entry.base_debit = Some(entry.debit * rate);
        entry.base_credit = Some(entry.credit * rate);
        if let Some(currency) = &currency {
            entry.currency = Some(currency.clone());
        }
    }
    for txn in party_transactions.iter_mut() {
        txn.conversion_rate = rate;
        txn.base_amount = txn.amount * rate;
        if txn.currency.is_none() {
            txn.currency = currency.clone();
        }
    }
}

/// Customer / supplier / tax subledger rows, ported from the Dart
/// `ledger_derivation.dart`: a Sales Invoice books `CT-{id}` (Invoice,
/// +grand), a Purchase Invoice `VT-{id}` (Invoice, +grand — positive = we
/// owe), a Payment Entry `CT-{id}` / `VT-{id}` (Payment, −paid), and each
/// invoice tax row books `TT-{id}-{i}` with its taxable base + tax + rate so
/// the VAT return can be built from the subledger alone.
fn derive_subledger_rows(
    ctx: &RowContext<'_>,
    payload: &Map<String, Value>,
) -> (Vec<PartyTransaction>, Vec<TaxTransaction>) {
    let id = ctx.document_id;
    let mut party_rows = Vec::new();
    let mut tax_rows = Vec::new();
    let party_txn = |kind: PartyKind, trans_type: &str, party: String, amount: f64| {
        PartyTransaction {
            id: format!(
                "{}-{id}",
                if kind == PartyKind::Customer {
                    "CT"
                } else {
                    "VT"
                }
            ),
            company_id: ctx.company_id,
            kind,
            trans_type: trans_type.to_string(),
            party,
            posting_date: ctx.posting_date.to_string(),
            due_date: as_non_empty(payload.get("due_date")),
            amount,
            currency: as_non_empty(payload.get("currency")),
            conversion_rate: 1.0, // base stamping adjusts this for FX vouchers
            base_amount: amount,
            voucher_type: ctx.doctype.to_string(),
            voucher_no: id.to_string(),
            is_reversal: false,
            batch_id: ctx.batch_id.to_string(),
        }
    };
    match ctx.doctype {
        "Sales Invoice" => {
            let customer = as_non_empty(payload.get("customer"));
            if let Some(customer) = &customer {
                let grand = as_num(payload.get("grand_total"));
                party_rows.push(party_txn(
                    PartyKind::Customer,
                    "Invoice",
                    customer.clone(),
                    grand,
                ));
            }
            tax_rows = tax_transactions(ctx, payload, "Customer", customer);
        }
        "Purchase Invoice" => {
            let supplier = as_non_empty(payload.get("supplier"));
            if let Some(supplier) = &supplier {
                let grand = as_num(payload.get("grand_total"));
                party_rows.push(party_txn(
                    PartyKind::Supplier,
                    "Invoice",
                    supplier.clone(),
                    grand,
                ));
            }
            tax_rows = tax_transactions(ctx, payload, "Supplier", supplier);
        }
        // A POS cash sale books no party row (payment is inline) but its
        // output VAT still lands in the tax subledger (Dart `_posInvoice`).
        "POS Invoice" => {
            tax_rows = tax_transactions(
                ctx,
                payload,
                "Customer",
                as_non_empty(payload.get("customer")),
            );
        }
        "Payment Entry" => {
            // Payment reduces what is owed: negative on submit.
            if let Some(party) = as_non_empty(payload.get("party")) {
                let paid = as_num(payload.get("paid_amount"));
                match as_non_empty(payload.get("payment_type")).as_deref() {
                    Some("Receive") => {
                        party_rows.push(party_txn(PartyKind::Customer, "Payment", party, -paid));
                    }
                    Some("Pay") => {
                        party_rows.push(party_txn(PartyKind::Supplier, "Payment", party, -paid));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (party_rows, tax_rows)
}

/// One `TT-{id}-{i}` subledger row per invoice tax row — including
/// zero-amount rows, whose taxable base still belongs in the VAT return
/// (mirrors the Dart `_taxLegs`, which gates only the GL leg on the amount).
fn tax_transactions(
    ctx: &RowContext<'_>,
    payload: &Map<String, Value>,
    party_type: &str,
    party: Option<String>,
) -> Vec<TaxTransaction> {
    let rows = match payload.get("taxes") {
        Some(Value::Array(rows)) => rows.as_slice(),
        _ => &[],
    };
    let id = ctx.document_id;
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let Some(row) = row.as_object() else { continue };
        out.push(TaxTransaction {
            id: format!("TT-{id}-{i}"),
            company_id: ctx.company_id,
            tax_type: as_non_empty(row.get("tax_type")).unwrap_or_else(|| "VAT".to_string()),
            tax: as_non_empty(row.get("tax_code")),
            posting_date: ctx.posting_date.to_string(),
            base_amount: as_num(row.get("taxable_amount")),
            tax_amount: as_num(row.get("tax_amount")),
            rate: as_num(row.get("rate")),
            party_type: party_type.to_string(),
            party: party.clone(),
            voucher_type: ctx.doctype.to_string(),
            voucher_no: id.to_string(),
            is_reversal: false,
            batch_id: ctx.batch_id.to_string(),
        });
    }
    out
}

/// The subledger rows as they appear in the command response: the Dart wire
/// fields plus `id` and `doctype`, so a client can persist them verbatim.
fn subledger_response_rows(
    party_transactions: &[PartyTransaction],
    tax_transactions: &[TaxTransaction],
) -> (Vec<Value>, Vec<Value>) {
    let party = party_transactions
        .iter()
        .map(|t| {
            let mut fields = t.row_fields();
            fields.insert("id".into(), json!(t.id));
            fields.insert("doctype".into(), json!(t.kind.doctype()));
            Value::Object(fields)
        })
        .collect();
    let tax = tax_transactions
        .iter()
        .map(|t| {
            let mut fields = t.row_fields();
            fields.insert("id".into(), json!(t.id));
            fields.insert("doctype".into(), json!("Tax Transaction"));
            Value::Object(fields)
        })
        .collect();
    (party, tax)
}

/// Raw (uncosted) stock ledger rows per doctype: `SLE-{id}-{i}` for item
/// documents, `SLE-{id}-{i}-out`/`-in` for Stock Entry legs. Line index `i`
/// is the position in `items`, matching the Dart id scheme even when lines
/// are skipped.
fn derive_stock_rows(ctx: &RowContext<'_>, payload: &Map<String, Value>) -> Vec<StockLedgerEntry> {
    match ctx.doctype {
        "Sales Invoice" if is_true(payload.get("update_stock")) => {
            stock_document_rows(ctx, payload, false, "Issue")
        }
        "Purchase Invoice" if is_true(payload.get("update_stock")) => {
            stock_document_rows(ctx, payload, true, "Receipt")
        }
        "Purchase Receipt" => stock_document_rows(ctx, payload, true, "Receipt"),
        // A POS sale always issues stock (update_stock semantics are
        // built-in); a Delivery Note is the pure stock-issue document.
        "POS Invoice" | "Delivery Note" => stock_document_rows(ctx, payload, false, "Issue"),
        "Stock Entry" => stock_entry_rows(ctx, payload),
        _ => Vec::new(),
    }
}

fn stock_document_rows(
    ctx: &RowContext<'_>,
    payload: &Map<String, Value>,
    incoming: bool,
    trans_type: &str,
) -> Vec<StockLedgerEntry> {
    let set_warehouse = as_non_empty(payload.get("set_warehouse"));
    let id = ctx.document_id;
    let mut rows = Vec::new();
    for (i, line) in items_of(payload).iter().enumerate() {
        let Some(line) = line.as_object() else {
            continue;
        };
        let Some(item) = as_non_empty(line.get("item")) else {
            continue;
        };
        let Some(warehouse) = as_non_empty(line.get("warehouse")).or_else(|| set_warehouse.clone())
        else {
            continue;
        };
        let qty = as_num(line.get("qty"));
        // Receipt: +qty on submit; issue: -qty.
        let qty_change = if incoming { qty } else { -qty };
        // `valuation_rate ?? rate`, treating an explicit null as absent.
        let rate = match line.get("valuation_rate") {
            Some(v) if !v.is_null() => as_num(Some(v)),
            _ => as_num(line.get("rate")),
        };
        rows.push(ctx.sle(
            format!("SLE-{id}-{i}"),
            trans_type,
            item,
            warehouse,
            qty_change,
            rate,
            as_non_empty(line.get("uom")),
        ));
    }
    rows
}

fn stock_entry_trans_type(purpose: Option<&str>) -> &'static str {
    match purpose {
        Some("Material Receipt") => "Receipt",
        Some("Material Transfer") => "Transfer",
        Some("Repack") | Some("Stock Count") => "Adjustment",
        Some("Manufacture") | Some("Manufacturing") => "Production",
        _ => "Issue",
    }
}

fn stock_entry_rows(ctx: &RowContext<'_>, payload: &Map<String, Value>) -> Vec<StockLedgerEntry> {
    let trans = stock_entry_trans_type(as_non_empty(payload.get("stock_entry_type")).as_deref());
    let id = ctx.document_id;
    let mut rows = Vec::new();
    for (i, line) in items_of(payload).iter().enumerate() {
        let Some(line) = line.as_object() else {
            continue;
        };
        let Some(item) = as_non_empty(line.get("item")) else {
            continue;
        };
        let qty = as_num(line.get("qty"));
        let rate = as_num(line.get("valuation_rate"));
        let uom = as_non_empty(line.get("uom"));
        if let Some(source) = as_non_empty(line.get("source_warehouse")) {
            // Leaves the source on submit.
            rows.push(ctx.sle(
                format!("SLE-{id}-{i}-out"),
                trans,
                item.clone(),
                source,
                -qty,
                rate,
                uom.clone(),
            ));
        }
        if let Some(target) = as_non_empty(line.get("target_warehouse")) {
            // Enters the target on submit.
            rows.push(ctx.sle(
                format!("SLE-{id}-{i}-in"),
                trans,
                item.clone(),
                target,
                qty,
                rate,
                uom,
            ));
        }
    }
    rows
}

fn derive_settlements(ctx: &RowContext<'_>, payload: &Map<String, Value>) -> Vec<Settlement> {
    let refs = match payload.get("references") {
        Some(Value::Array(refs)) => refs.as_slice(),
        _ => &[],
    };
    let party_type = if as_non_empty(payload.get("payment_type")).as_deref() == Some("Receive") {
        "Customer"
    } else {
        "Supplier"
    };
    let party = as_non_empty(payload.get("party"));
    let id = ctx.document_id;
    let mut out = Vec::new();
    for (i, reference) in refs.iter().enumerate() {
        let Some(reference) = reference.as_object() else {
            continue;
        };
        let Some(ref_doctype) = as_non_empty(reference.get("reference_doctype")) else {
            continue;
        };
        let Some(ref_name) = as_non_empty(reference.get("reference_name")) else {
            continue;
        };
        out.push(Settlement {
            id: format!("STL-{id}-{i}"),
            company_id: ctx.company_id,
            payment_voucher_type: ctx.doctype.to_string(),
            payment_voucher_no: id.to_string(),
            invoice_voucher_type: ref_doctype,
            invoice_voucher_no: ref_name,
            party_type: party_type.to_string(),
            party: party.clone(),
            allocated_amount: as_num(reference.get("allocated_amount")),
            posting_date: ctx.posting_date.to_string(),
            is_reversal: false,
            batch_id: ctx.batch_id.to_string(),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Costing (port of `LedgerDerivationService._costStockMovements`)
// ---------------------------------------------------------------------------

/// Per-(item, warehouse) ledger cache: the stored prior rows (count recorded
/// for the commit's optimistic expectations) plus rows appended by the
/// voucher being built, so sequential issues consume in order.
#[derive(Default)]
struct Ledgers {
    rows: BTreeMap<(String, String), Vec<LedgerRow>>,
    prior_counts: HashMap<(String, String), usize>,
}

impl Ledgers {
    async fn prior_for(
        &mut self,
        store: &dyn Store,
        company_id: Uuid,
        item: &str,
        warehouse: &str,
    ) -> Result<&mut Vec<LedgerRow>, PostingError> {
        let key = (item.to_string(), warehouse.to_string());
        if !self.rows.contains_key(&key) {
            let stored = store.sles_for_pair(company_id, item, warehouse).await?;
            self.prior_counts.insert(key.clone(), stored.len());
            self.rows.insert(
                key.clone(),
                stored
                    .iter()
                    .map(|sle| LedgerRow {
                        qty_change: sle.qty_change,
                        valuation_rate: sle.valuation_rate,
                    })
                    .collect(),
            );
        }
        Ok(self.rows.get_mut(&key).expect("just inserted"))
    }

    fn expectations(&self) -> Vec<(String, String, usize)> {
        self.prior_counts
            .iter()
            .map(|((item, warehouse), count)| (item.clone(), warehouse.clone(), *count))
            .collect()
    }
}

async fn load_items(
    store: &dyn Store,
    company_id: Uuid,
    sles: &[StockLedgerEntry],
    payload: &Map<String, Value>,
) -> Result<HashMap<String, Item>, PostingError> {
    let mut ids: Vec<String> = sles.iter().map(|sle| sle.item.clone()).collect();
    // The GRNI split needs the stock/service status of every invoice line,
    // including lines that produced no SLE.
    for line in items_of(payload) {
        if let Some(item) = line.as_object().and_then(|l| as_non_empty(l.get("item"))) {
            ids.push(item);
        }
    }
    ids.sort();
    ids.dedup();
    let items = store.items(company_id, &ids).await?;
    Ok(items
        .into_iter()
        .map(|item| (item.id.clone(), item))
        .collect())
}

/// Costs the voucher's outgoing rows at the item's valuation method (issues
/// remove inventory at *cost*, never the selling rate), makes a Transfer's
/// incoming leg inherit its outgoing cost, re-enters returned goods at the
/// original voucher's cost, and spreads a Production entry's consumed cost
/// across its output.
async fn cost_stock_movements(
    store: &dyn Store,
    company_id: Uuid,
    sles: &mut [StockLedgerEntry],
    items: &HashMap<String, Item>,
    payload: &Map<String, Value>,
    ledgers: &mut Ledgers,
) -> Result<(), PostingError> {
    let is_return = is_true(payload.get("is_return"));
    let return_against = as_non_empty(payload.get("return_against"));
    let mut out_cost_by_item: HashMap<String, f64> = HashMap::new();
    let mut consumed_cost = 0.0;
    let mut production_legs: Vec<usize> = Vec::new();
    let mut produced_qty = 0.0;

    // Indexed loop: the body mutates `sles[index]` while also borrowing the
    // ledger cache mutably, which `iter_mut()` + await points can't express.
    #[allow(clippy::needless_range_loop)]
    for index in 0..sles.len() {
        let (item, warehouse, trans_type, uom) = {
            let sle = &sles[index];
            (
                sle.item.clone(),
                sle.warehouse.clone(),
                sle.trans_type.clone(),
                sle.uom.clone(),
            )
        };
        // Convert the line qty (in its transaction UOM) to stock units so the
        // ledger and bins always track the stock UOM; a plain receipt's rate
        // is divided by the same factor below so total stock value is
        // preserved across the conversion (Dart `uomFactor` semantics).
        let factor = uom_factor(items.get(&item), uom.as_deref());
        if (factor - 1.0).abs() > EPS {
            sles[index].qty_change *= factor;
        }
        let qty_change = sles[index].qty_change;
        if qty_change < 0.0 {
            let method = items.get(&item).and_then(|i| i.valuation_method.as_deref());
            let prior = ledgers
                .prior_for(store, company_id, &item, &warehouse)
                .await?;
            let rate = issue_rate(prior, -qty_change, method);
            sles[index].valuation_rate = rate;
            out_cost_by_item.insert(item.clone(), rate);
            consumed_cost += -qty_change * rate;
        } else if qty_change > 0.0 {
            if is_return {
                // Goods returning to stock re-enter at the cost they were
                // sold at (the original voucher's rate), falling back to the
                // current moving average.
                let rate = return_cost(
                    store,
                    company_id,
                    &item,
                    &warehouse,
                    return_against.as_deref(),
                    ledgers,
                )
                .await?;
                sles[index].valuation_rate = rate;
            } else if trans_type == "Transfer" {
                if let Some(rate) = out_cost_by_item.get(&item) {
                    sles[index].valuation_rate = *rate;
                }
            } else if trans_type == "Production" {
                production_legs.push(index);
                produced_qty += qty_change;
            } else if (factor - 1.0).abs() > EPS {
                // Plain receipt in a transaction UOM: the supplied rate is
                // per transaction unit — divide by the factor so
                // qty × rate (both now in stock units) preserves the value.
                sles[index].valuation_rate /= factor;
            }
            // Make sure the pair's prior ledger is loaded (for expectations,
            // the negative-stock guard and the bin recompute).
            ledgers
                .prior_for(store, company_id, &item, &warehouse)
                .await?;
        } else {
            ledgers
                .prior_for(store, company_id, &item, &warehouse)
                .await?;
        }

        // Reflect this row in the simulated ledger for later lines.
        let rate = sles[index].valuation_rate;
        let prior = ledgers
            .prior_for(store, company_id, &item, &warehouse)
            .await?;
        prior.push(LedgerRow {
            qty_change,
            valuation_rate: rate,
        });
    }

    // Allocate the consumed raw cost across all produced output at one
    // per-unit rate, so total output value equals what was consumed.
    if !production_legs.is_empty() && produced_qty > 0.0 && consumed_cost > 0.0 {
        let rate = consumed_cost / produced_qty;
        for index in production_legs {
            let sle = &mut sles[index];
            // Patch the simulated ledger row appended above as well.
            let key = (sle.item.clone(), sle.warehouse.clone());
            if let Some(rows) = ledgers.rows.get_mut(&key) {
                for row in rows.iter_mut().rev() {
                    if (row.qty_change - sle.qty_change).abs() < EPS
                        && (row.valuation_rate - sle.valuation_rate).abs() < EPS
                    {
                        row.valuation_rate = rate;
                        break;
                    }
                }
            }
            sle.valuation_rate = rate;
        }
    }
    Ok(())
}

/// The cost to re-enter returned stock: the rate the original voucher moved
/// this item at (preferring the same warehouse), or the current moving
/// average when no original row exists.
async fn return_cost(
    store: &dyn Store,
    company_id: Uuid,
    item: &str,
    warehouse: &str,
    return_against: Option<&str>,
    ledgers: &mut Ledgers,
) -> Result<f64, PostingError> {
    if let Some(voucher) = return_against {
        let originals = store.sles_for_voucher(company_id, voucher).await?;
        let mut any_rate = None;
        for original in originals.iter().filter(|sle| sle.item == item) {
            if original.warehouse == warehouse {
                return Ok(original.valuation_rate); // exact warehouse match
            }
            any_rate.get_or_insert(original.valuation_rate);
        }
        if let Some(rate) = any_rate {
            return Ok(rate);
        }
    }
    let prior = ledgers
        .prior_for(store, company_id, item, warehouse)
        .await?;
    Ok(compute_balance(prior).valuation_rate)
}

// ---------------------------------------------------------------------------
// Perpetual-inventory GL (port of `_stockGlLegs` + `_splitGrniFromExpense`)
// ---------------------------------------------------------------------------

fn resolve_account<'a>(
    item: Option<&'a Item>,
    pick: impl Fn(&Item) -> Option<&String>,
    default: &'a str,
) -> &'a str {
    item.and_then(|item| pick(item).map(String::as_str))
        .unwrap_or(default)
}

/// The GL counterpart of every costed stock movement, `v = qty × rate`:
/// sale vouchers issue Dr COGS / Cr Inventory (returns flip); purchase
/// vouchers receive Dr Inventory / Cr GRNI (returns flip); Stock Entry
/// movements post against the stock-adjustment account. Transfers and
/// Production legs post no GL — value-neutral with one inventory account.
fn stock_gl_legs(
    ctx: &RowContext<'_>,
    sles: &[StockLedgerEntry],
    items: &HashMap<String, Item>,
    settings: &CompanySettings,
) -> Vec<GlEntry> {
    let sale = matches!(
        ctx.doctype,
        "Sales Invoice" | "Delivery Note" | "POS Invoice"
    );
    let purchase = matches!(ctx.doctype, "Purchase Invoice" | "Purchase Receipt");
    let mut legs = Vec::new();
    for sle in sles {
        let value = sle.qty_change * sle.valuation_rate;
        if value.abs() < EPS || sle.trans_type == "Transfer" || sle.trans_type == "Production" {
            continue;
        }
        let item = items.get(&sle.item);
        let inventory = resolve_account(
            item,
            |i| i.inventory_account.as_ref(),
            &settings.default_inventory_account,
        );
        let counter = if sale {
            resolve_account(
                item,
                |i| i.cogs_account.as_ref(),
                &settings.default_cogs_account,
            )
        } else if purchase {
            settings.default_grni_account.as_str()
        } else {
            resolve_account(
                item,
                |i| i.stock_adjustment_account.as_ref(),
                &settings.default_stock_adjustment_account,
            )
        };
        let amount = value.abs();
        let (dr, cr) = if value > 0.0 {
            (inventory, counter)
        } else {
            (counter, inventory)
        };
        let mut debit_leg = ctx.gl(
            format!("{}-gl-d", sle.id),
            dr.to_string(),
            amount,
            0.0,
            None,
            None,
        );
        let mut credit_leg = ctx.gl(
            format!("{}-gl-c", sle.id),
            cr.to_string(),
            0.0,
            amount,
            None,
            None,
        );
        // Valuation cost is already company/base currency — the legs carry
        // conversion_rate 1 and base == amount (the Dart `_stockGl` builder).
        for leg in [&mut debit_leg, &mut credit_leg] {
            leg.conversion_rate = Some(1.0);
            leg.base_debit = Some(leg.debit);
            leg.base_credit = Some(leg.credit);
        }
        legs.push(debit_leg);
        legs.push(credit_leg);
    }
    legs
}

/// Moves a Purchase Invoice's stock-line value off the expense leg onto GRNI
/// so buying stock never lands in COGS at purchase time: the bill's own SLE
/// legs post Dr Inventory / Cr GRNI (one-document flow) or the earlier
/// receipt did (two-document flow); this split posts the Dr GRNI clearing
/// side. Service / non-stock lines keep their expense treatment.
fn split_grni_from_expense(
    ctx: &RowContext<'_>,
    gl: &mut Vec<GlEntry>,
    payload: &Map<String, Value>,
    items: &HashMap<String, Item>,
    settings: &CompanySettings,
) {
    let stock_net: f64 = items_of(payload)
        .iter()
        .filter_map(Value::as_object)
        .filter(|line| {
            as_non_empty(line.get("item")).is_some_and(|id| {
                is_stock_item_type(items.get(&id).map(|item| item.item_type.as_str()))
            })
        })
        .map(line_amount)
        .sum();
    if stock_net.abs() < EPS {
        return;
    }

    let rate = conversion_rate_of(payload);
    let expense_id = format!("GL-{}-debit", ctx.document_id);
    if let Some(pos) = gl.iter().position(|entry| entry.id == expense_id) {
        gl[pos].debit -= stock_net;
        // Keep the stamped base in step with the reduced transaction amount
        // (the Dart `_splitGrniFromExpense` restamps the moved column).
        if gl[pos].base_debit.is_some() {
            gl[pos].base_debit = Some(gl[pos].debit * rate);
        }
        if gl[pos].debit.abs() < EPS && gl[pos].credit.abs() < EPS {
            gl.remove(pos);
        }
    }
    let mut grni = ctx.gl(
        format!("GL-{}-grni", ctx.document_id),
        settings.default_grni_account.clone(),
        stock_net,
        0.0,
        None,
        None,
    );
    grni.currency = as_non_empty(payload.get("currency"));
    grni.conversion_rate = Some(rate);
    grni.base_debit = Some(stock_net * rate);
    grni.base_credit = Some(0.0);
    gl.push(grni);
}

// ---------------------------------------------------------------------------
// Validation + derived balances
// ---------------------------------------------------------------------------

fn check_period_lock(settings: &CompanySettings, posting_date: &str) -> Result<(), PostingError> {
    if let Some(lock) = &settings.books_lock_date {
        // ISO dates compare correctly as strings.
        if posting_date <= lock.as_str() {
            return Err(PostingError::Validation(format!(
                "posting date {posting_date} falls in a locked period (books locked through {lock})"
            )));
        }
    }
    Ok(())
}

/// JE-style balance guard over the generated GL.
fn check_balanced(gl: &[GlEntry]) -> Result<(), PostingError> {
    let debit: f64 = gl.iter().map(|entry| entry.debit).sum();
    let credit: f64 = gl.iter().map(|entry| entry.credit).sum();
    if (debit - credit).abs() > BALANCE_TOLERANCE {
        return Err(PostingError::Validation(format!(
            "generated GL does not balance: debit {debit} vs credit {credit}"
        )));
    }
    Ok(())
}

fn check_negative_stock(settings: &CompanySettings, ledgers: &Ledgers) -> Result<(), PostingError> {
    if settings.allow_negative_stock {
        return Ok(());
    }
    for ((item, warehouse), rows) in &ledgers.rows {
        let snap = compute_balance(rows);
        if snap.actual_qty < 0.0 {
            return Err(PostingError::Validation(format!(
                "insufficient stock: {item} in {warehouse} would go to {}",
                snap.actual_qty
            )));
        }
    }
    Ok(())
}

/// The recomputed bins for every pair this posting touched (prior rows plus
/// the voucher's own, already folded into the ledger cache).
fn recompute_bins(company_id: Uuid, ledgers: &Ledgers) -> Vec<Bin> {
    ledgers
        .rows
        .iter()
        .map(|((item, warehouse), rows)| {
            let snap = compute_balance(rows);
            Bin {
                company_id,
                item: item.clone(),
                warehouse: warehouse.clone(),
                actual_qty: snap.actual_qty,
                valuation_rate: snap.valuation_rate,
                stock_value: snap.stock_value,
            }
        })
        .collect()
}

/// Recomputes `outstanding_amount` for every submitted invoice this payment
/// (or its cancellation) touches: grand total less all stored settlements
/// plus the new signed allocations.
async fn payment_outstanding_updates(
    store: &dyn Store,
    company_id: Uuid,
    new_settlements: &[Settlement],
) -> Result<Vec<(String, String, f64)>, PostingError> {
    let mut updates = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for settlement in new_settlements {
        let key = (
            settlement.invoice_voucher_type.clone(),
            settlement.invoice_voucher_no.clone(),
        );
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let invoice = store
            .posted_document(company_id, &key.0, &key.1)
            .await?
            .ok_or_else(|| {
                PostingError::Validation(format!(
                    "payment references unknown invoice {} {}",
                    key.0, key.1
                ))
            })?;
        if invoice.docstatus != 1 {
            return Err(PostingError::Validation(format!(
                "payment references invoice {} {} which is not submitted",
                key.0, key.1
            )));
        }
        let grand = as_num(invoice.payload.get("grand_total"));
        let stored: f64 = store
            .settlements_for_invoice(company_id, &key.0, &key.1)
            .await?
            .iter()
            .map(|s| s.allocated_amount)
            .sum();
        let new: f64 = new_settlements
            .iter()
            .filter(|s| s.invoice_voucher_type == key.0 && s.invoice_voucher_no == key.1)
            .map(|s| s.allocated_amount)
            .sum();
        updates.push((key.0, key.1, outstanding_amount(grand, [stored, new])));
    }
    Ok(updates)
}

fn audit_row(company_id: Uuid, actor: Actor, action: &str, detail: Value) -> AuditEntry {
    AuditEntry {
        id: Uuid::new_v4(),
        company_id,
        user_id: actor.user_id,
        device_id: actor.device_id,
        action: action.to_string(),
        detail,
        at: Utc::now(),
    }
}
