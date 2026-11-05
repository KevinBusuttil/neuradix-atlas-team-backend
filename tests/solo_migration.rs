//! C3 — the one-time Solo → Team take-on (`commands/import-solo-opening`).
//! Runs on the in-memory store, and on a real PostgreSQL database when
//! `ATLAS_TEST_DATABASE_URL` is set.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use support::{approx, TestApp};

fn envelope_mutation(id: &str, doctype: &str, document_id: &str, fields: Value) -> Value {
    json!({
        "id": id,
        "type": "createDocument",
        "docType": doctype,
        "documentId": document_id,
        "payload": {
            "id": document_id,
            "doctype": doctype,
            "company": null,
            "docstatus": 0,
            "payload": fields.to_string(),
            "created_at": 1751791234567i64,
            "modified_at": 1751791234567i64,
            "sync_version": null,
            "sync_state": "synced",
            "amended_from": null,
        },
        "deviceId": "solo-device",
        "userId": "owner",
        "localTimestamp": 1751791234567i64,
        "syncVersion": null,
        "status": "pending"
    })
}

/// What the Solo device publishes before the opening position: the chart of
/// accounts it uses, its parties and its items.
async fn publish_masters(app: &mut TestApp) {
    let mut mutations = Vec::new();
    for account in [
        "Debtors",
        "Creditors",
        "Stock",
        "Bank",
        "Capital",
        "Sales",
        "VAT",
    ] {
        mutations.push(envelope_mutation(
            &format!("m-acc-{account}"),
            "Account",
            account,
            json!({ "account_name": account }),
        ));
    }
    mutations.push(envelope_mutation(
        "m-c1",
        "Customer",
        "C-1",
        json!({ "customer_name": "Harbour Bar" }),
    ));
    mutations.push(envelope_mutation(
        "m-s1",
        "Supplier",
        "S-1",
        json!({ "supplier_name": "Glassworks" }),
    ));
    mutations.push(envelope_mutation(
        "m-bottle",
        "Item",
        "BOTTLE",
        json!({ "item_code": "BOTTLE", "item_type": "Stock", "stock_uom": "Nos" }),
    ));
    mutations.push(envelope_mutation(
        "m-svc",
        "Item",
        "SVC",
        json!({ "item_code": "SVC", "item_type": "Service" }),
    ));
    let token = app.device_token("owner").await;
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/sync/push", app.company_id),
            Some(&token),
            json!({ "mutations": mutations }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "push failed: {body}");
}

/// A Solo book's position: bank 1000, two customers' worth of receivables
/// (one party), stock 240 (24 bottles at 10), a supplier owed 118, VAT 18
/// payable, and capital/sales making it balance.
fn package(migration_id: &str) -> Value {
    json!({
        "migration_id": migration_id,
        "posting_date": "2026-11-05",
        "source": { "app": "Atlas Solo", "device": "solo-device" },
        "lines": [
            { "account": "Bank", "debit": 1000.0 },
            { "account": "Debtors", "debit": 236.0, "party_type": "Customer", "party": "C-1" },
            { "account": "Stock", "debit": 240.0 },
            { "account": "Creditors", "credit": 118.0, "party_type": "Supplier", "party": "S-1" },
            { "account": "VAT", "credit": 18.0 },
            { "account": "Sales", "credit": 340.0 },
            { "account": "Capital", "credit": 1000.0 }
        ],
        "stock": [
            { "item": "BOTTLE", "warehouse": "Main Store", "qty": 24.0, "stock_value": 240.0, "account": "Stock" }
        ],
        "numbering": [
            { "doctype": "Sales Invoice", "last_issued": "SINV-.2026.-0002", "last_value": 2 },
            { "doctype": "Purchase Invoice", "last_issued": "PINV-.2026.-0001", "last_value": 1 }
        ],
        "expected": {
            "trial_balance": {
                "Bank": 1000.0, "Debtors": 236.0, "Stock": 240.0,
                "Creditors": -118.0, "VAT": -18.0, "Sales": -340.0, "Capital": -1000.0
            },
            "receivables": 236.0,
            "payables": 118.0,
            "stock_value": 240.0
        }
    })
}

async fn import(app: &mut TestApp, role: &str, body: Value) -> (StatusCode, Value) {
    let token = app.device_token(role).await;
    app.request(
        Method::POST,
        &format!("/companies/{}/commands/import-solo-opening", app.company_id),
        Some(&token),
        body,
    )
    .await
}

#[tokio::test]
async fn the_take_on_posts_the_solo_position_and_continues_numbering() {
    let mut app = TestApp::new().await;
    publish_masters(&mut app).await;

    let (status, body) = import(&mut app, "owner", package("mig-0001-abcd")).await;
    assert_eq!(status, StatusCode::OK, "take-on failed: {body}");
    assert_eq!(body["document_id"], "SOLO-OPENING-mig-0001-abcd");
    assert_eq!(body["replayed"], false);
    assert_eq!(body["reconciliation"]["receivables"], 236.0);
    assert_eq!(body["reconciliation"]["stock_value"], 240.0);

    // The ledger holds exactly the Solo trial balance.
    for (account, net) in [
        ("Bank", 1000.0),
        ("Debtors", 236.0),
        ("Stock", 240.0),
        ("Creditors", -118.0),
        ("VAT", -18.0),
        ("Sales", -340.0),
        ("Capital", -1000.0),
    ] {
        assert!(
            approx(app.account_balance(account).await, net),
            "{account}: {}",
            app.account_balance(account).await
        );
    }
    // Stock subledger and bin at the Solo valuation.
    let bin = app.bin("BOTTLE", "Main Store").await.expect("bin");
    assert!(approx(bin.actual_qty, 24.0) && approx(bin.stock_value, 240.0));
    // Party subledger rows, Dart JE convention.
    let ct = app
        .party_transaction("CT-SOLO-OPENING-mig-0001-abcd-1")
        .await
        .expect("customer row");
    assert!(approx(ct.amount, 236.0));
    let vt = app
        .party_transaction("VT-SOLO-OPENING-mig-0001-abcd-3")
        .await
        .expect("supplier row");
    assert!(approx(vt.amount, 118.0));

    // Team numbering continues after the last Solo number.
    app.put_settings(json!({ "allow_negative_stock": false }))
        .await;
    let (status, sale) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-TEAM-1",
                "payload": {
                    "customer": "C-1",
                    "posting_date": "2026-11-06",
                    "update_stock": 1,
                    "set_warehouse": "Main Store"
                },
                "items": [{ "item": "BOTTLE", "qty": 2, "rate": 20, "warehouse": "Main Store" }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "first Team sale: {sale}");
    assert_eq!(sale["number"], "SINV-00003");
    let bin = app.bin("BOTTLE", "Main Store").await.unwrap();
    assert!(
        approx(bin.actual_qty, 22.0),
        "the sale costs from the opening stock"
    );
}

#[tokio::test]
async fn a_replay_returns_the_same_result_and_a_changed_package_is_refused() {
    let mut app = TestApp::new().await;
    publish_masters(&mut app).await;
    let (status, first) = import(&mut app, "owner", package("mig-0002-abcd")).await;
    assert_eq!(status, StatusCode::OK, "{first}");

    // A lost response: the device sends the identical package again.
    let (status, again) = import(&mut app, "owner", package("mig-0002-abcd")).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["replayed"], true);
    assert_eq!(again["document_id"], first["document_id"]);
    assert!(
        approx(app.account_balance("Bank").await, 1000.0),
        "posted once"
    );
    assert_eq!(app.gl_count("SOLO-OPENING-mig-0002-abcd").await, 7);

    // The same migration id with different figures is not silently accepted.
    let mut changed = package("mig-0002-abcd");
    changed["source"]["device"] = json!("another-device");
    let (status, body) = import(&mut app, "owner", changed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // And a second, different take-on into the same company is refused.
    let (status, body) = import(&mut app, "owner", package("mig-0003-abcd")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.to_string().contains("only once"), "{body}");
    assert!(approx(app.account_balance("Bank").await, 1000.0));
}

#[tokio::test]
async fn a_company_that_already_posted_cannot_receive_a_take_on() {
    let mut app = TestApp::new().await;
    publish_masters(&mut app).await;
    app.upsert_item(json!({ "id": "SVC", "item_type": "Service" }))
        .await;
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-EARLY",
                "payload": { "customer": "C-1", "posting_date": "2026-11-01" },
                "items": [{ "item": "SVC", "qty": 1, "rate": 10 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = import(&mut app, "owner", package("mig-0004-abcd")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string().contains("new, empty Team company"),
        "{body}"
    );
    assert_eq!(app.gl_count("SOLO-OPENING-mig-0004-abcd").await, 0);
}

#[tokio::test]
async fn only_the_owner_may_move_a_solo_book() {
    let mut app = TestApp::new().await;
    publish_masters(&mut app).await;
    for role in ["admin", "accountant", "sales"] {
        let (status, _) = import(&mut app, role, package("mig-0005-abcd")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role} must be refused");
    }
    // A user token without a registered device is refused too.
    let owner_user = app.owner_token.clone();
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/companies/{}/commands/import-solo-opening", app.company_id),
            Some(&owner_user),
            package("mig-0005-abcd"),
        )
        .await;
    assert_ne!(status, StatusCode::OK);
    assert!(approx(app.account_balance("Bank").await, 0.0));
}

#[tokio::test]
async fn packages_that_do_not_reconcile_post_nothing() {
    let mut app = TestApp::new().await;
    publish_masters(&mut app).await;

    type Mutate = Box<dyn Fn(&mut Value)>;
    let cases: Vec<(&str, Mutate)> = vec![
        (
            "does not balance",
            Box::new(|p| p["lines"][0]["debit"] = json!(1001.0)),
        ),
        (
            "trial balance mismatch",
            Box::new(|p| p["expected"]["trial_balance"]["Bank"] = json!(999.0)),
        ),
        (
            "stock does not reconcile",
            Box::new(|p| p["stock"][0]["stock_value"] = json!(230.0)),
        ),
        (
            "receivables",
            Box::new(|p| p["expected"]["receivables"] = json!(200.0)),
        ),
        (
            "not on the Team server",
            Box::new(|p| p["lines"][1]["party"] = json!("C-UNKNOWN")),
        ),
        (
            "service item",
            Box::new(|p| p["stock"][0]["item"] = json!("SVC")),
        ),
        (
            "locked period",
            Box::new(|p| p["posting_date"] = json!("2026-01-31")),
        ),
        (
            "party_type and party",
            Box::new(|p| p["lines"][1]["party"] = Value::Null),
        ),
        (
            "no Team numbering series",
            Box::new(|p| p["numbering"][0]["doctype"] = json!("Quotation")),
        ),
        (
            "migration_id",
            Box::new(|p| p["migration_id"] = json!("x; drop")),
        ),
    ];
    app.put_settings(json!({ "books_lock_date": "2026-03-31" }))
        .await;
    for (i, (expect, mutate)) in cases.iter().enumerate() {
        let mut body = package(&format!("mig-bad-{i:04}"));
        mutate(&mut body);
        let (status, response) = import(&mut app, "owner", body).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "case {expect:?} must be refused: {response}"
        );
        assert!(
            response.to_string().contains(expect),
            "case {expect:?}: {response}"
        );
    }
    assert!(app.all_gl_entries().await.is_empty(), "nothing was posted");
    assert!(app.bin("BOTTLE", "Main Store").await.is_none());

    // The good package still goes in afterwards: refusals left no residue.
    let (status, body) = import(&mut app, "owner", package("mig-0006-abcd")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
