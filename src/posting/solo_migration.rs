//! C3 — the one-time **Solo → Team take-on**.
//!
//! A Solo book that already issued official documents cannot simply start
//! syncing into a Team company: its submitted vouchers and ledger rows are
//! refused by the sync-plane authority guard (a device may not publish
//! official state), and replaying them as drafts would have the server
//! number them again. The take-on instead moves the Solo book's **position
//! at a cut-over date**, posted by this server:
//!
//! * one opening **Journal Entry** whose lines are the Solo trial balance,
//!   split by party on receivable / payable lines (party subledger rows are
//!   derived exactly like the Dart JE derivation: `CT-`/`VT-{id}-{i}`,
//!   `Adjustment`);
//! * opening **stock** per item and warehouse at the Solo valuation (stock
//!   ledger rows and bins; the stock value is already in the GL lines, so no
//!   extra GL legs are posted);
//! * **numbering continuation**: each Team series starts after the last
//!   number the Solo book issued for that document type, so Team never
//!   reuses a number the business already gave a customer.
//!
//! Server-side guarantees (every one enforced before anything is written,
//! and the write is one atomic posting commit):
//!
//! * only the company **owner**, from a registered device;
//! * only into a company that holds **no official document** and no earlier
//!   take-on (store-enforced inside the commit transaction);
//! * accounts, parties and items must already be on the Team server (the
//!   client publishes master data first), stock items must be stock items;
//! * the lines balance; stock value per inventory account equals that
//!   account's GL balance; the party lines equal the stated receivables and
//!   payables; the stated trial balance matches the lines account by account;
//! * the cut-over date is valid and not in a locked period;
//! * idempotent: the key is the migration id, and a replay with a
//!   different package is refused instead of silently returning the first.
//!
//! The Solo documents themselves stay in the Solo book (kept on the device
//! as an archive and in the safety backup); they never become Team
//! documents, so nothing here re-numbers or rewrites history.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::posting::engine::{
    audit_row, check_balanced, check_period_lock, load_items, validated_posting_date, Actor,
    PostingError,
};
use crate::posting::model::{
    format_number, series_key, Bin, CommitOutcome, GlEntry, PartyKind, PartyTransaction,
    PostedDocument, PostingBatch, PostingCommit, SoloMigrationMark, StockLedgerEntry,
};
use crate::posting::values::{is_stock_item_type, round2};
use crate::store::{Store, StoreError};

/// The document type the opening entry is posted as.
pub const OPENING_DOCTYPE: &str = "Journal Entry";
/// Document ids of take-on entries start with this.
pub const OPENING_ID_PREFIX: &str = "SOLO-OPENING-";
/// Cent tolerance for every reconciliation in the package.
const CENT: f64 = 0.005;
/// Most lines one package may carry (a large Solo book has a few hundred
/// accounts and parties; this is a sanity bound, not a business limit).
const MAX_LINES: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpeningLine {
    pub account: String,
    #[serde(default)]
    pub debit: f64,
    #[serde(default)]
    pub credit: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpeningStock {
    pub item: String,
    pub warehouse: String,
    pub qty: f64,
    /// Value of the quantity at the Solo valuation, in base currency.
    pub stock_value: f64,
    /// The inventory account the Solo book carries this stock on.
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumberingContinuation {
    pub doctype: String,
    /// The last official number the Solo book issued (for the record).
    pub last_issued: String,
    /// Its sequence value: the Team series continues at `last_value + 1`.
    pub last_value: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedTotals {
    /// Net (debit − credit) per account.
    pub trial_balance: BTreeMap<String, f64>,
    pub receivables: f64,
    pub payables: f64,
    pub stock_value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoloOpeningCommand {
    pub migration_id: String,
    pub posting_date: String,
    #[serde(default)]
    pub source: Value,
    pub lines: Vec<OpeningLine>,
    #[serde(default)]
    pub stock: Vec<OpeningStock>,
    #[serde(default)]
    pub numbering: Vec<NumberingContinuation>,
    pub expected: ExpectedTotals,
}

impl SoloOpeningCommand {
    /// The entry's deterministic document id.
    pub fn document_id(&self) -> String {
        format!("{OPENING_ID_PREFIX}{}", self.migration_id)
    }

    /// SHA-256 of the canonical package (serde_json maps are ordered), so a
    /// replay can tell "the same package again" from "a different package
    /// under the same migration id".
    pub fn package_sha256(&self) -> String {
        let canonical = serde_json::to_vec(self).unwrap_or_default();
        hex::encode(Sha256::digest(canonical))
    }

    fn idempotency_key(&self) -> String {
        format!("solo-migration:{}", self.migration_id)
    }
}

fn invalid(msg: impl Into<String>) -> PostingError {
    PostingError::Validation(msg.into())
}

fn finite_non_negative(value: f64, what: &str) -> Result<(), PostingError> {
    if !value.is_finite() || value < 0.0 {
        return Err(invalid(format!(
            "{what} must be a finite, non-negative amount"
        )));
    }
    Ok(())
}

fn safe_id(raw: &str) -> bool {
    (8..=64).contains(&raw.len())
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Posts the take-on. See the module docs for every guarantee.
pub async fn import_solo_opening(
    store: &dyn Store,
    company_id: Uuid,
    cmd: SoloOpeningCommand,
    actor: Actor,
) -> Result<CommitOutcome, PostingError> {
    if !safe_id(&cmd.migration_id) {
        return Err(invalid(
            "migration_id must be 8–64 letters, digits, '-' or '_'",
        ));
    }
    let sha = cmd.package_sha256();
    let key = cmd.idempotency_key();
    if let Some(response) = store.idempotent_response(company_id, &key).await? {
        return same_package(response, &sha);
    }
    let commit = build(store, company_id, &cmd, &sha, &key, actor).await?;
    match store.posting_commit(commit).await {
        Ok(outcome) if outcome.replayed => same_package(outcome.response, &sha),
        Ok(outcome) => Ok(outcome),
        Err(StoreError::Conflict(msg)) => Err(PostingError::Conflict(msg)),
        Err(err) => Err(err.into()),
    }
}

fn same_package(response: Value, sha: &str) -> Result<CommitOutcome, PostingError> {
    if response.get("package_sha256").and_then(Value::as_str) == Some(sha) {
        Ok(CommitOutcome {
            response,
            replayed: true,
        })
    } else {
        Err(PostingError::Conflict(
            "this migration id was already used with a different package; \
             the company already holds that Solo opening position"
                .into(),
        ))
    }
}

async fn build(
    store: &dyn Store,
    company_id: Uuid,
    cmd: &SoloOpeningCommand,
    sha: &str,
    key: &str,
    actor: Actor,
) -> Result<PostingCommit, PostingError> {
    let settings = store.company_settings(company_id).await?;
    let mut header = Map::new();
    header.insert("posting_date".into(), json!(cmd.posting_date));
    let posting_date = validated_posting_date(&header)?;
    check_period_lock(&settings, &posting_date)?;

    if cmd.lines.is_empty() {
        return Err(invalid("the opening entry has no lines"));
    }
    if cmd.lines.len() > MAX_LINES || cmd.stock.len() > MAX_LINES {
        return Err(invalid(format!(
            "a take-on carries at most {MAX_LINES} lines"
        )));
    }

    let document_id = cmd.document_id();
    let batch_id = format!("PB-{document_id}");

    // ---- Lines: shape, masters, party subledger -------------------------
    let mut accounts = BTreeSet::new();
    let mut parties: BTreeSet<(&'static str, String)> = BTreeSet::new();
    for (i, line) in cmd.lines.iter().enumerate() {
        let account = line.account.trim();
        if account.is_empty() {
            return Err(invalid(format!("line {} has no account", i + 1)));
        }
        finite_non_negative(line.debit, &format!("line {} debit", i + 1))?;
        finite_non_negative(line.credit, &format!("line {} credit", i + 1))?;
        if line.debit > 0.0 && line.credit > 0.0 {
            return Err(invalid(format!(
                "line {} ({account}) has both a debit and a credit",
                i + 1
            )));
        }
        accounts.insert(account.to_string());
        match (line.party_type.as_deref(), line.party.as_deref()) {
            (None, None) => {}
            (Some(kind), Some(party)) if !party.trim().is_empty() => {
                let kind = PartyKind::parse(kind).ok_or_else(|| {
                    invalid(format!(
                        "line {} party_type must be Customer or Supplier",
                        i + 1
                    ))
                })?;
                parties.insert((kind.as_str(), party.trim().to_string()));
            }
            _ => {
                return Err(invalid(format!(
                    "line {} must carry both party_type and party, or neither",
                    i + 1
                )))
            }
        }
    }
    for stock in &cmd.stock {
        accounts.insert(stock.account.trim().to_string());
    }
    for account in &accounts {
        if store
            .company_document(company_id, "Account", account)
            .await?
            .is_none()
        {
            return Err(invalid(format!(
                "account {account} is not on the Team server yet: publish the \
                 chart of accounts before the opening position"
            )));
        }
    }
    for (kind, party) in &parties {
        if store
            .company_document(company_id, kind, party)
            .await?
            .is_none()
        {
            return Err(invalid(format!(
                "{kind} {party} is not on the Team server yet: publish customers \
                 and suppliers before the opening position"
            )));
        }
    }

    let mut gl = Vec::with_capacity(cmd.lines.len());
    let mut party_transactions = Vec::new();
    for (i, line) in cmd.lines.iter().enumerate() {
        let party_type = line.party_type.clone();
        let party = line.party.as_ref().map(|p| p.trim().to_string());
        gl.push(GlEntry {
            id: format!("GL-{document_id}-{i}"),
            company_id,
            account: line.account.trim().to_string(),
            debit: line.debit,
            credit: line.credit,
            party_type: party_type.clone(),
            party: party.clone(),
            voucher_type: OPENING_DOCTYPE.into(),
            voucher_no: document_id.clone(),
            posting_date: posting_date.clone(),
            currency: None,
            conversion_rate: None,
            base_debit: None,
            base_credit: None,
            is_reversal: false,
            batch_id: batch_id.clone(),
        });
        if let (Some(kind), Some(party)) = (party_type.as_deref().and_then(PartyKind::parse), party)
        {
            let net = line.debit - line.credit;
            // Same convention as the Dart JE derivation: a debit raises what
            // a customer owes; a credit raises what we owe a supplier.
            let amount = match kind {
                PartyKind::Customer => net,
                PartyKind::Supplier => -net,
            };
            let prefix = match kind {
                PartyKind::Customer => "CT",
                PartyKind::Supplier => "VT",
            };
            party_transactions.push(PartyTransaction {
                id: format!("{prefix}-{document_id}-{i}"),
                company_id,
                kind,
                trans_type: "Adjustment".into(),
                party,
                posting_date: posting_date.clone(),
                due_date: None,
                amount,
                currency: None,
                conversion_rate: 1.0,
                base_amount: amount,
                voucher_type: OPENING_DOCTYPE.into(),
                voucher_no: document_id.clone(),
                is_reversal: false,
                batch_id: batch_id.clone(),
            });
        }
    }
    check_balanced(&gl)?;

    // ---- Stock: items, quantities, value against the GL -----------------
    let mut sles = Vec::with_capacity(cmd.stock.len());
    let mut bins = Vec::with_capacity(cmd.stock.len());
    let mut pairs = BTreeSet::new();
    let mut stock_by_account: BTreeMap<String, f64> = BTreeMap::new();
    for (i, stock) in cmd.stock.iter().enumerate() {
        let item = stock.item.trim().to_string();
        let warehouse = stock.warehouse.trim().to_string();
        if item.is_empty() || warehouse.is_empty() {
            return Err(invalid(format!(
                "stock line {} needs an item and a warehouse",
                i + 1
            )));
        }
        if !stock.qty.is_finite() || stock.qty <= 0.0 {
            return Err(invalid(format!(
                "stock line {} ({item} in {warehouse}): opening quantity must be above zero",
                i + 1
            )));
        }
        finite_non_negative(stock.stock_value, &format!("stock line {} value", i + 1))?;
        if !pairs.insert((item.clone(), warehouse.clone())) {
            return Err(invalid(format!(
                "{item} in {warehouse} appears twice in the opening stock"
            )));
        }
        *stock_by_account
            .entry(stock.account.trim().to_string())
            .or_default() += stock.stock_value;
        let rate = stock.stock_value / stock.qty;
        sles.push(StockLedgerEntry {
            id: format!("SLE-{document_id}-{i}"),
            company_id,
            trans_type: "Receipt".into(),
            item: item.clone(),
            warehouse: warehouse.clone(),
            qty_change: stock.qty,
            valuation_rate: rate,
            voucher_type: OPENING_DOCTYPE.into(),
            voucher_no: document_id.clone(),
            posting_date: posting_date.clone(),
            uom: None,
            is_reversal: false,
            batch_id: batch_id.clone(),
            seq: 0,
        });
        bins.push(Bin {
            company_id,
            item,
            warehouse,
            actual_qty: stock.qty,
            valuation_rate: rate,
            stock_value: stock.stock_value,
        });
    }
    let items = load_items(store, company_id, &sles, &Map::new()).await?;
    for sle in &sles {
        match items.get(&sle.item) {
            None => {
                return Err(invalid(format!(
                    "item {} is not on the Team server yet: publish items before \
                     the opening position",
                    sle.item
                )))
            }
            Some(item) if !is_stock_item_type(Some(item.item_type.as_str())) => {
                return Err(invalid(format!(
                    "item {} is a service item and cannot carry opening stock",
                    sle.item
                )))
            }
            Some(_) => {}
        }
    }

    // ---- Reconciliation, all to the cent --------------------------------
    let mut tb: BTreeMap<String, f64> = BTreeMap::new();
    for entry in &gl {
        *tb.entry(entry.account.clone()).or_default() += entry.debit - entry.credit;
    }
    tb.retain(|_, net| net.abs() > CENT);
    let mut stated: BTreeMap<String, f64> = cmd.expected.trial_balance.clone();
    stated.retain(|_, net| net.abs() > CENT);
    let names: BTreeSet<&String> = tb.keys().chain(stated.keys()).collect();
    for account in names {
        let ours = tb.get(account).copied().unwrap_or(0.0);
        let theirs = stated.get(account).copied().unwrap_or(0.0);
        if (ours - theirs).abs() > CENT {
            return Err(invalid(format!(
                "trial balance mismatch on {account}: the lines give {:.2}, the Solo \
                 book stated {:.2}; nothing was posted",
                ours, theirs
            )));
        }
    }
    for (account, value) in &stock_by_account {
        let ledger = tb.get(account).copied().unwrap_or(0.0);
        if (ledger - value).abs() > CENT {
            return Err(invalid(format!(
                "stock does not reconcile with the ledger: opening stock on {account} is \
                 {value:.2} but the account's balance is {ledger:.2}; nothing was posted"
            )));
        }
    }
    let total_stock: f64 = stock_by_account.values().sum();
    if (total_stock - cmd.expected.stock_value).abs() > CENT {
        return Err(invalid(format!(
            "opening stock totals {total_stock:.2} but the Solo book stated {:.2}",
            cmd.expected.stock_value
        )));
    }
    let receivables: f64 = party_transactions
        .iter()
        .filter(|t| t.kind == PartyKind::Customer)
        .map(|t| t.amount)
        .sum();
    let payables: f64 = party_transactions
        .iter()
        .filter(|t| t.kind == PartyKind::Supplier)
        .map(|t| t.amount)
        .sum();
    if (receivables - cmd.expected.receivables).abs() > CENT {
        return Err(invalid(format!(
            "customer balances total {receivables:.2} but the Solo book stated \
             receivables of {:.2}",
            cmd.expected.receivables
        )));
    }
    if (payables - cmd.expected.payables).abs() > CENT {
        return Err(invalid(format!(
            "supplier balances total {payables:.2} but the Solo book stated \
             payables of {:.2}",
            cmd.expected.payables
        )));
    }

    // ---- Numbering continuation ------------------------------------------
    let mut floors = Vec::new();
    let mut continuation = Vec::new();
    let mut seen = BTreeSet::new();
    for n in &cmd.numbering {
        let key = series_key(&n.doctype)
            .ok_or_else(|| invalid(format!("{} has no Team numbering series", n.doctype)))?;
        if !seen.insert(key) {
            return Err(invalid(format!("{} appears twice in numbering", n.doctype)));
        }
        if !(0..=999_999_999).contains(&n.last_value) {
            return Err(invalid(format!(
                "{}: last number value {} is out of range",
                n.doctype, n.last_value
            )));
        }
        floors.push((key.to_string(), n.last_value));
        continuation.push(json!({
            "doctype": n.doctype,
            "solo_last_issued": n.last_issued,
            "team_next_number": format_number(key, n.last_value + 1),
        }));
    }

    // ---- The commit -------------------------------------------------------
    let mut payload = Map::new();
    payload.insert("posting_date".into(), json!(posting_date));
    payload.insert("voucher_type".into(), json!("Opening Entry"));
    payload.insert("is_opening".into(), json!(1));
    payload.insert(
        "title".into(),
        json!("Opening balances moved from Atlas Solo"),
    );
    payload.insert(
        "user_remark".into(),
        json!(format!(
            "Atlas Solo take-on {} (package {}). The Solo documents up to this date \
             stay in the Solo archive and safety backup.",
            cmd.migration_id,
            &sha[..12]
        )),
    );
    payload.insert(
        "solo_migration".into(),
        json!({
            "migration_id": cmd.migration_id,
            "package_sha256": sha,
            "source": cmd.source,
            "numbering": continuation,
        }),
    );
    payload.insert(
        "accounts".into(),
        serde_json::to_value(&cmd.lines).unwrap_or(Value::Array(vec![])),
    );
    let total_debit: f64 = gl.iter().map(|e| e.debit).sum();
    payload.insert("total_debit".into(), json!(round2(total_debit)));
    payload.insert("total_credit".into(), json!(round2(total_debit)));

    let reconciliation = json!({
        "trial_balance": tb.iter().map(|(a, v)| (a.clone(), json!(round2(*v)))).collect::<Map<String, Value>>(),
        "receivables": round2(receivables),
        "payables": round2(payables),
        "stock_value": round2(total_stock),
        "gl_entries": gl.len(),
        "stock_lines": sles.len(),
        "party_lines": party_transactions.len(),
    });
    let response = json!({
        "doctype": OPENING_DOCTYPE,
        "document_id": document_id,
        "number": Value::Null,
        "docstatus": 1,
        "migration_id": cmd.migration_id,
        "package_sha256": sha,
        "posting_date": posting_date,
        "reconciliation": reconciliation,
        "numbering": continuation,
    });
    let audit = audit_row(
        company_id,
        actor,
        "command.import-solo-opening",
        json!({
            "documentId": document_id,
            "migrationId": cmd.migration_id,
            "packageSha256": sha,
            "source": cmd.source,
            "reconciliation": reconciliation,
            "numbering": continuation,
        }),
    );
    let expectations: Vec<(String, String, usize)> = pairs
        .iter()
        .map(|(item, warehouse)| (item.clone(), warehouse.clone(), 0))
        .collect();

    Ok(PostingCommit {
        company_id,
        idempotency_key: Some(key.to_string()),
        batch: PostingBatch {
            id: batch_id,
            company_id,
            document_id: document_id.clone(),
            doctype: OPENING_DOCTYPE.into(),
            kind: "submit".into(),
            reversal_of: None,
            created_at: Utc::now(),
        },
        document: PostedDocument {
            id: document_id,
            company_id,
            doctype: OPENING_DOCTYPE.into(),
            payload: Value::Object(payload),
            docstatus: 1,
            official_number: None,
            created_at: Utc::now(),
        },
        document_is_new: true,
        series_key: None,
        preassigned_number: None,
        till_binding: None,
        gl_entries: gl,
        stock_ledger_entries: sles,
        party_transactions,
        tax_transactions: Vec::new(),
        settlements: Vec::new(),
        bins,
        outstanding_updates: Vec::new(),
        sle_expectations: expectations,
        replication_device_id: actor.replication_device_id,
        audit,
        response,
        numbering_floors: floors,
        solo_migration: Some(SoloMigrationMark {
            migration_id: cmd.migration_id.clone(),
            package_sha256: sha.to_string(),
        }),
    })
}
