//! Language-neutral fixture runner (STOCK_COGS_IMPLEMENTATION_PLAN §5): every
//! `tests/fixtures/*.json` is a `{setup, actions, expect}` scenario driven
//! through the command API over `MemStore`, asserting GL entries, stock
//! ledger, bins, account balances, settlements and invoice outstanding. The
//! same JSON is the contract for the Dart Solo engine (fixture #1 is the
//! scenario `mercantis.hub.flutter/test/stock_cogs_acceptance_test.dart`
//! exercises today).

mod support;

use std::collections::HashMap;
use std::path::PathBuf;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use support::{approx, TestApp};

#[derive(Deserialize)]
struct Fixture {
    name: String,
    #[serde(default)]
    setup: Setup,
    actions: Vec<Action>,
    #[serde(default)]
    expect: Option<Checks>,
}

#[derive(Deserialize, Default)]
struct Setup {
    #[serde(default)]
    settings: Option<Value>,
    #[serde(default)]
    items: Vec<Value>,
}

#[derive(Deserialize)]
struct Action {
    #[serde(default)]
    comment: Option<String>,
    /// Role acting; a member with this role (and a device) is created lazily.
    #[serde(default)]
    as_role: Option<String>,
    #[serde(default)]
    submit: Option<Value>,
    #[serde(default)]
    cancel: Option<Value>,
    /// Expected HTTP status; defaults to 200.
    #[serde(default)]
    expect_status: Option<u16>,
    /// Substring the error body must contain (rejection actions).
    #[serde(default)]
    error_contains: Option<String>,
    #[serde(default)]
    expect: Option<Checks>,
}

#[derive(Deserialize, Default)]
struct Checks {
    /// account -> Σ(debit − credit) across the whole GL.
    #[serde(default)]
    account_balances: HashMap<String, f64>,
    /// account balance scoped to one voucher.
    #[serde(default)]
    voucher_accounts: Vec<VoucherAccount>,
    /// "ITEM@WAREHOUSE" -> expected bin (missing bin compares as zero).
    #[serde(default)]
    bins: HashMap<String, BinCheck>,
    #[serde(default)]
    gl_count: HashMap<String, usize>,
    #[serde(default)]
    sle_count: HashMap<String, usize>,
    #[serde(default)]
    settlement_count: HashMap<String, usize>,
    /// "Doctype/DocumentId" -> outstanding_amount.
    #[serde(default)]
    outstanding: HashMap<String, f64>,
    #[serde(default)]
    gross_profit: Option<GrossProfit>,
    /// Official number expected on THIS action's response.
    #[serde(default)]
    number: Option<String>,
    /// docstatus expected on THIS action's response.
    #[serde(default)]
    docstatus: Option<i64>,
}

#[derive(Deserialize)]
struct VoucherAccount {
    account: String,
    voucher: String,
    balance: f64,
}

#[derive(Deserialize)]
struct BinCheck {
    qty: f64,
    value: f64,
    #[serde(default)]
    rate: Option<f64>,
}

#[derive(Deserialize)]
struct GrossProfit {
    income: String,
    cogs: String,
    value: f64,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

async fn run_fixture(file: &str) {
    let path = fixtures_dir().join(file);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));
    let fixture: Fixture =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("bad fixture {file}: {e}"));
    let label = format!("{file} ({})", fixture.name);

    let mut app = TestApp::new().await;
    if let Some(settings) = &fixture.setup.settings {
        app.put_settings(settings.clone()).await;
    }
    for item in &fixture.setup.items {
        app.upsert_item(item.clone()).await;
    }

    for (index, action) in fixture.actions.iter().enumerate() {
        let step = format!(
            "{label} action #{index}{}",
            action
                .comment
                .as_deref()
                .map(|c| format!(" — {c}"))
                .unwrap_or_default()
        );
        let role = action.as_role.as_deref().unwrap_or("owner");
        let (status, body) = match (&action.submit, &action.cancel) {
            (Some(submit), None) => app.submit_as(role, submit.clone()).await,
            (None, Some(cancel)) => app.cancel_as(role, cancel.clone()).await,
            _ => panic!("{step}: action must have exactly one of submit/cancel"),
        };
        let expected =
            StatusCode::from_u16(action.expect_status.unwrap_or(200)).expect("valid status");
        assert_eq!(status, expected, "{step}: unexpected status, body {body}");
        if let Some(needle) = &action.error_contains {
            let error = body["error"].as_str().unwrap_or_default();
            assert!(
                error.contains(needle),
                "{step}: error {error:?} does not contain {needle:?}"
            );
        }
        if let Some(checks) = &action.expect {
            assert_checks(&app, checks, Some(&body), &step);
        }
    }

    if let Some(checks) = &fixture.expect {
        assert_checks(&app, checks, None, &format!("{label} final expectations"));
    }
}

fn assert_checks(app: &TestApp, checks: &Checks, response: Option<&Value>, step: &str) {
    for (account, expected) in &checks.account_balances {
        let actual = app.account_balance(account);
        assert!(
            approx(actual, *expected),
            "{step}: account {account} balance {actual}, expected {expected}"
        );
    }
    for check in &checks.voucher_accounts {
        let actual = app.voucher_account(&check.account, &check.voucher);
        assert!(
            approx(actual, check.balance),
            "{step}: {} on {} is {actual}, expected {}",
            check.account,
            check.voucher,
            check.balance
        );
    }
    for (key, expected) in &checks.bins {
        let (item, warehouse) = key
            .split_once('@')
            .unwrap_or_else(|| panic!("{step}: bin key {key} must be ITEM@WAREHOUSE"));
        let bin = app.bin(item, warehouse);
        let (qty, value, rate) = bin
            .as_ref()
            .map(|b| (b.actual_qty, b.stock_value, b.valuation_rate))
            .unwrap_or((0.0, 0.0, 0.0));
        assert!(
            approx(qty, expected.qty),
            "{step}: bin {key} qty {qty}, expected {}",
            expected.qty
        );
        assert!(
            approx(value, expected.value),
            "{step}: bin {key} value {value}, expected {}",
            expected.value
        );
        if let Some(expected_rate) = expected.rate {
            assert!(
                approx(rate, expected_rate),
                "{step}: bin {key} rate {rate}, expected {expected_rate}"
            );
        }
    }
    for (voucher, expected) in &checks.gl_count {
        let actual = app.gl_count(voucher);
        assert_eq!(actual, *expected, "{step}: GL count for {voucher}");
    }
    for (voucher, expected) in &checks.sle_count {
        let actual = app.sle_count(voucher);
        assert_eq!(actual, *expected, "{step}: SLE count for {voucher}");
    }
    for (payment, expected) in &checks.settlement_count {
        let actual = app.settlement_count(payment);
        assert_eq!(actual, *expected, "{step}: settlement count for {payment}");
    }
    for (key, expected) in &checks.outstanding {
        let (doctype, id) = key
            .split_once('/')
            .unwrap_or_else(|| panic!("{step}: outstanding key {key} must be Doctype/DocId"));
        let actual = app
            .outstanding(doctype, id)
            .unwrap_or_else(|| panic!("{step}: {key} has no outstanding_amount"));
        assert!(
            approx(actual, *expected),
            "{step}: outstanding for {key} is {actual}, expected {expected}"
        );
    }
    if let Some(gp) = &checks.gross_profit {
        // Revenue is a credit balance, COGS a debit balance.
        let revenue = -app.account_balance(&gp.income);
        let cogs = app.account_balance(&gp.cogs);
        let actual = revenue - cogs;
        assert!(
            approx(actual, gp.value),
            "{step}: gross profit {actual} (revenue {revenue} − COGS {cogs}), expected {}",
            gp.value
        );
    }
    if let Some(number) = &checks.number {
        let body = response.expect("number check requires an action response");
        assert_eq!(
            body["number"],
            json!(number),
            "{step}: official number mismatch, body {body}"
        );
    }
    if let Some(docstatus) = checks.docstatus {
        let body = response.expect("docstatus check requires an action response");
        assert_eq!(
            body["docstatus"],
            json!(docstatus),
            "{step}: docstatus mismatch, body {body}"
        );
    }
}

macro_rules! fixture_test {
    ($name:ident, $file:expr) => {
        #[tokio::test]
        async fn $name() {
            run_fixture($file).await;
        }
    };
}

fixture_test!(
    fixture_0001_mandatory_perpetual_inventory,
    "0001-mandatory-perpetual-inventory.json"
);
fixture_test!(fixture_0002_fifo_costing, "0002-fifo-costing.json");
fixture_test!(
    fixture_0003_negative_stock_rejection,
    "0003-negative-stock-rejection.json"
);
fixture_test!(
    fixture_0004_stock_adjustment_up_down,
    "0004-stock-adjustment-up-down.json"
);
fixture_test!(
    fixture_0005_payment_settlement_outstanding,
    "0005-payment-settlement-outstanding.json"
);
fixture_test!(fixture_0006_period_lock, "0006-period-lock.json");
fixture_test!(fixture_0007_role_rejection, "0007-role-rejection.json");
fixture_test!(
    fixture_0008_transfer_value_neutral,
    "0008-transfer-value-neutral.json"
);

/// Every fixture file on disk must be wired to a test above — a new fixture
/// that nobody runs is a silent hole in the contract.
#[test]
fn every_fixture_file_is_covered() {
    let covered = [
        "0001-mandatory-perpetual-inventory.json",
        "0002-fifo-costing.json",
        "0003-negative-stock-rejection.json",
        "0004-stock-adjustment-up-down.json",
        "0005-payment-settlement-outstanding.json",
        "0006-period-lock.json",
        "0007-role-rejection.json",
        "0008-transfer-value-neutral.json",
    ];
    let mut on_disk: Vec<String> = std::fs::read_dir(fixtures_dir())
        .expect("fixtures dir")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    on_disk.sort();
    let mut covered: Vec<String> = covered.iter().map(|s| s.to_string()).collect();
    covered.sort();
    assert_eq!(on_disk, covered, "fixture files and fixture tests diverge");
}
