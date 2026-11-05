//! C5 — backend authority for Journal Entry, Expense and Customer Deposit,
//! and the single authoritative Team books lock.
//!
//! The ledger shapes these doctypes post are also pinned by the shared
//! accounting contract (`tests/fixtures/0013`–`0016`, run by the hub's Solo
//! engine too). This file covers what only a server can: roles, numbering,
//! idempotency, the deposit invariants under concurrency, and the lock
//! command (permissions, stale views, backdating, races, audit).
//!
//! MemStore by default, PgStore with `ATLAS_TEST_DATABASE_URL`.

mod support;

use atlas_team_backend::posting::engine::{
    prepare_cancel, prepare_submit, Actor, CancelCommand, SubmitCommand,
};
use atlas_team_backend::store::StoreError;
use axum::http::{Method, StatusCode};
use serde_json::{json, Map, Value};
use support::{approx, TestApp};

fn journal(id: &str, date: &str, lines: Value) -> Value {
    json!({
        "doctype": "Journal Entry",
        "document_id": id,
        "payload": { "posting_date": date, "accounts": lines },
        "items": [],
        "idempotency_key": format!("submit-JE-{id}"),
    })
}

fn expense(id: &str, payload: Value) -> Value {
    let mut p = json!({
        "posting_date": "2026-09-10",
        "description": "Printer paper",
        "expense_account": "Office Costs",
        "net_amount": 100,
        "tax_amount": 18,
        "tax_account": "VAT",
        "is_paid": 1,
        "paid_from": "Cash",
    });
    for (k, v) in payload.as_object().unwrap() {
        p[k] = v.clone();
    }
    json!({ "doctype": "Expense", "document_id": id, "payload": p, "items": [] })
}

fn deposit(id: &str, customer: &str, amount: f64) -> Value {
    json!({
        "doctype": "Customer Deposit",
        "document_id": id,
        "payload": {
            "customer": customer, "posting_date": "2026-09-01", "amount": amount,
            "cash_account": "Cash",
        },
        "items": [],
    })
}

fn application(id: &str, deposit: &str, amount: f64) -> Value {
    json!({
        "doctype": "Payment Entry",
        "document_id": id,
        "payload": {
            "payment_type": "Receive", "deposit": deposit, "paid_amount": amount,
            "posting_date": "2026-09-15", "references": [],
        },
        "items": [],
    })
}

fn command(body: &Value) -> SubmitCommand {
    SubmitCommand {
        doctype: body["doctype"].as_str().unwrap().into(),
        document_id: body["document_id"].as_str().map(str::to_string),
        payload: body["payload"]
            .as_object()
            .cloned()
            .unwrap_or_else(Map::new),
        items: body["items"].as_array().cloned().unwrap_or_default(),
        idempotency_key: None,
        offline_fiscal: false,
    }
}

async fn audit_actions(app: &TestApp) -> Vec<Value> {
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/audit?limit=200", app.company_id),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    body["entries"].as_array().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Journal Entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn journal_entries_post_with_parties_dimensions_numbering_and_reversal() {
    let mut app = TestApp::new().await;
    let lines = json!([
        { "account": "Debtors", "debit": 250, "credit": 0,
          "party_type": "Customer", "party": "CUST-1", "cost_center": "Retail" },
        { "account": "Creditors", "debit": 0, "credit": 100,
          "party_type": "Supplier", "party": "SUPP-1" },
        { "account": "Sales", "debit": 0, "credit": 150, "cost_center": "Retail" },
    ]);
    let body = journal("JE-1", "2026-09-30", lines);
    let (status, res) = app.submit_as("accountant", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "JV-00001");

    let first = app.gl_entry("GL-JE-1-0").await.unwrap();
    assert_eq!(first.account, "Debtors");
    assert_eq!(first.party.as_deref(), Some("CUST-1"));
    assert_eq!(first.cost_center.as_deref(), Some("Retail"));
    assert!(
        approx(first.base_debit.unwrap(), 250.0),
        "JE is base-stamped"
    );
    let customer = app.party_transaction("CT-JE-1-0").await.unwrap();
    assert_eq!(customer.trans_type, "Adjustment");
    assert!(approx(customer.amount, 250.0));
    let supplier = app.party_transaction("VT-JE-1-1").await.unwrap();
    assert!(
        approx(supplier.amount, 100.0),
        "a credit to a supplier = we owe more"
    );

    // A retried submit (dropped response) replays, it never posts twice.
    let (status, again) = app.submit_as("accountant", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["number"], "JV-00001");
    assert_eq!(app.gl_count("JE-1").await, 3);

    // Cancellation reverses every leg and the subledgers; no number is used.
    let (status, res) = app
        .cancel_as(
            "accountant",
            json!({ "doctype": "Journal Entry", "document_id": "JE-1" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "JV-00001");
    for account in ["Debtors", "Creditors", "Sales"] {
        assert!(approx(app.account_balance(account).await, 0.0), "{account}");
    }
    let reversal = app.gl_entry("GL-JE-1-0-reversal").await.unwrap();
    assert_eq!(reversal.cost_center.as_deref(), Some("Retail"));
    let ct = app.party_transaction("CT-JE-1-0-reversal").await.unwrap();
    assert!(approx(ct.amount, -250.0));

    // A foreign-currency journal stamps base amounts at its rate.
    let mut fx = journal(
        "JE-FX",
        "2026-09-30",
        json!([
            { "account": "Bank USD", "debit": 100, "credit": 0 },
            { "account": "Sales", "debit": 0, "credit": 100 },
        ]),
    );
    fx["payload"]["currency"] = json!("USD");
    fx["payload"]["conversion_rate"] = json!(0.9);
    let (status, res) = app.submit_as("accountant", fx).await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "JV-00002");
    let leg = app.gl_entry("GL-JE-FX-0").await.unwrap();
    assert!(approx(leg.base_debit.unwrap(), 90.0));
    assert_eq!(leg.currency.as_deref(), Some("USD"));
}

#[tokio::test]
async fn journal_entries_are_refused_unless_they_are_sound() {
    let mut app = TestApp::new().await;
    let refused: [(&str, Value, &str); 6] = [
        (
            "unbalanced",
            json!([{ "account": "Cash", "debit": 100 }, { "account": "Sales", "credit": 90 }]),
            "does not balance",
        ),
        (
            "one line",
            json!([{ "account": "Cash", "debit": 0 }]),
            "two lines",
        ),
        (
            "negative",
            json!([{ "account": "Cash", "debit": -100 }, { "account": "Sales", "credit": -100 }]),
            "zero or more",
        ),
        (
            "both sides",
            json!([{ "account": "Cash", "debit": 50, "credit": 50 },
                   { "account": "Sales", "debit": 0, "credit": 0 }]),
            "both a debit and a credit",
        ),
        (
            "no account",
            json!([{ "account": "", "debit": 100 }, { "account": "Sales", "credit": 100 }]),
            "no account",
        ),
        (
            "party without id",
            json!([{ "account": "Debtors", "debit": 100, "party_type": "Customer" },
                   { "account": "Sales", "credit": 100 }]),
            "no party",
        ),
    ];
    for (i, (what, lines, message)) in refused.into_iter().enumerate() {
        let (status, body) = app
            .submit_as(
                "owner",
                journal(&format!("JE-BAD-{i}"), "2026-09-30", lines),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{what}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains(message),
            "{what}: {body}"
        );
    }

    // Roles: a journal can move any account, so only the finance roles.
    let ok = json!([{ "account": "Cash", "debit": 10 }, { "account": "Sales", "credit": 10 }]);
    for role in ["sales", "purchasing", "stock", "pos"] {
        let (status, _) = app
            .submit_as(
                role,
                journal(&format!("JE-{role}"), "2026-09-30", ok.clone()),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{role}");
    }
    // Nothing above consumed a number.
    let (status, res) = app
        .submit_as("admin", journal("JE-OK", "2026-09-30", ok))
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "JV-00001");
}

#[tokio::test]
async fn no_voucher_posts_to_a_group_account() {
    let mut app = TestApp::new().await;
    let token = app.device_token("owner").await;
    let account = |id: &str, group: &str| {
        json!({
            "id": format!("m-acct-{id}"), "type": "createDocument", "docType": "Account",
            "documentId": id,
            "payload": { "id": id, "doctype": "Account", "docstatus": 0,
                         "payload": json!({ "account_name": id, "is_group": group }).to_string() },
            "deviceId": "dev-1", "userId": "u", "localTimestamp": 1, "syncVersion": null,
            "status": "pending",
        })
    };
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/sync/push", app.company_id),
            Some(&token),
            json!({ "mutations": [account("Expenses", "1"), account("Rent", "0")] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = app
        .submit_as(
            "owner",
            journal(
                "JE-G",
                "2026-09-30",
                json!([{ "account": "Expenses", "debit": 10 }, { "account": "Cash", "credit": 10 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"].as_str().unwrap().contains("group account"),
        "{body}"
    );
    let (status, body) = app
        .submit_as(
            "owner",
            journal(
                "JE-L",
                "2026-09-30",
                json!([{ "account": "Rent", "debit": 10 }, { "account": "Cash", "credit": 10 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// Expense
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expenses_post_paid_or_owed_with_vat_and_reverse() {
    let mut app = TestApp::new().await;
    let (status, res) = app
        .submit_as("purchasing", expense("EXP-1", json!({})))
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "EXP-00001");
    assert!(approx(app.account_balance("Office Costs").await, 100.0));
    assert!(approx(app.account_balance("VAT").await, 18.0));
    assert!(approx(app.account_balance("Cash").await, -118.0));
    let vat = app.tax_transaction("TT-EXP-1-0").await.unwrap();
    assert!(approx(vat.base_amount, 100.0) && approx(vat.tax_amount, 18.0));
    assert_eq!(
        app.party_transaction_count("EXP-1").await,
        0,
        "paid: nothing owed"
    );

    // Owed to a supplier: Cr payable with the party, and the supplier row.
    let (status, res) = app
        .submit_as(
            "accountant",
            expense(
                "EXP-2",
                json!({ "is_paid": 0, "supplier": "SUPP-1", "tax_amount": 0, "net_amount": 40 }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    let credit = app.gl_entry("GL-EXP-2-credit").await.unwrap();
    assert_eq!(
        credit.account, "Creditors",
        "credit_to falls back to the payable"
    );
    assert_eq!(credit.party.as_deref(), Some("SUPP-1"));
    assert_eq!(
        app.tax_transaction_count("EXP-2").await,
        0,
        "no VAT, no VAT row"
    );
    let owed = app.party_transaction("VT-EXP-2").await.unwrap();
    assert_eq!(owed.trans_type, "Invoice");
    assert!(approx(owed.amount, 40.0));

    let (status, _) = app
        .cancel_as(
            "accountant",
            json!({ "doctype": "Expense", "document_id": "EXP-2" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let reversed = app.party_transaction("VT-EXP-2-reversal").await.unwrap();
    assert_eq!(reversed.trans_type, "CreditNote");
    assert!(approx(app.account_balance("Creditors").await, 0.0));

    // Refusals: a wrong gross, a foreign rate, no category, nothing spent,
    // and a role that does not buy.
    for (body, message) in [
        (
            expense("EXP-B1", json!({ "gross_amount": 120 })),
            "gross_amount",
        ),
        (
            expense(
                "EXP-B2",
                json!({ "conversion_rate": 1.2, "currency": "USD" }),
            ),
            "company currency",
        ),
        (
            expense("EXP-B3", json!({ "expense_account": "" })),
            "category",
        ),
        (
            expense("EXP-B4", json!({ "net_amount": 0, "tax_amount": 0 })),
            "more than zero",
        ),
    ] {
        let (status, res) = app.submit_as("owner", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
        assert!(res["error"].as_str().unwrap().contains(message), "{res}");
    }
    let (status, _) = app.submit_as("sales", expense("EXP-B5", json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, res) = app.submit_as("owner", expense("EXP-3", json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["number"], "EXP-00003", "refusals consume no number");
}

// ---------------------------------------------------------------------------
// Customer Deposit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deposits_are_received_applied_in_part_and_never_twice() {
    let mut app = TestApp::new().await;
    let (status, res) = app
        .submit_as("sales", deposit("DEP-1", "CUST-1", 500.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "DEP-00001");
    assert!(approx(
        app.account_balance("Customer Deposits").await,
        -500.0
    ));
    assert!(approx(app.account_balance("Cash").await, 500.0));

    // Partial application: Dr the liability (defaulted from the deposit),
    // Cr receivable, party = the deposit's customer.
    let (status, res) = app
        .submit_as("accountant", application("PAY-1", "DEP-1", 300.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    let to = app.gl_entry("GL-PAY-1-to").await.unwrap();
    assert_eq!(to.account, "Customer Deposits");
    let paid = app.party_transaction("CT-PAY-1").await.unwrap();
    assert_eq!(paid.party, "CUST-1");
    assert!(
        approx(app.account_balance("Customer Deposits").await, -200.0),
        "200 left"
    );

    // Over-application, the wrong customer, another debit account, a Pay
    // payment, and a deposit that was never submitted are all refused.
    let mut wrong_customer = application("PAY-X2", "DEP-1", 10.0);
    wrong_customer["payload"]["party"] = json!("CUST-2");
    wrong_customer["payload"]["party_type"] = json!("Customer");
    let mut wrong_account = application("PAY-X3", "DEP-1", 10.0);
    wrong_account["payload"]["paid_to"] = json!("Cash");
    let mut pay = application("PAY-X4", "DEP-1", 10.0);
    pay["payload"]["payment_type"] = json!("Pay");
    for (body, message) in [
        (application("PAY-X1", "DEP-1", 200.01), "200.00 left"),
        (wrong_customer, "belongs to CUST-1"),
        (wrong_account, "liability account"),
        (pay, "Receive payment"),
        (application("PAY-X5", "DEP-NONE", 10.0), "not submitted"),
    ] {
        let (status, res) = app.submit_as("accountant", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
        assert!(res["error"].as_str().unwrap().contains(message), "{res}");
    }

    // The rest applies exactly.
    let (status, res) = app
        .submit_as("accountant", application("PAY-2", "DEP-1", 200.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert!(approx(app.account_balance("Customer Deposits").await, 0.0));

    // A deposit still applied cannot be cancelled (refund = cancel the
    // applications first, then the deposit).
    let cancel_dep = json!({ "doctype": "Customer Deposit", "document_id": "DEP-1" });
    let (status, res) = app.cancel_as("accountant", cancel_dep.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{res}");
    assert!(res["error"].as_str().unwrap().contains("PAY-1"), "{res}");
    for id in ["PAY-1", "PAY-2"] {
        let (status, _) = app
            .cancel_as(
                "accountant",
                json!({ "doctype": "Payment Entry", "document_id": id }),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, res) = app.cancel_as("accountant", cancel_dep).await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert!(approx(app.account_balance("Customer Deposits").await, 0.0));
    assert!(approx(app.account_balance("Cash").await, 0.0), "refunded");

    // Validation and roles.
    let mut no_customer = deposit("DEP-B1", "", 10.0);
    no_customer["payload"]["customer"] = Value::Null;
    for (body, message) in [
        (deposit("DEP-B2", "CUST-1", 0.0), "more than zero"),
        (no_customer, "needs a customer"),
    ] {
        let (status, res) = app.submit_as("owner", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
        assert!(res["error"].as_str().unwrap().contains(message), "{res}");
    }
    let (status, _) = app
        .submit_as("stock", deposit("DEP-B3", "CUST-1", 10.0))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn two_devices_applying_the_same_deposit_at_once_cannot_both_win() {
    let mut app = TestApp::new().await;
    let (status, _) = app
        .submit_as("owner", deposit("DEP-C", "CUST-1", 100.0))
        .await;
    assert_eq!(status, StatusCode::OK);
    let company = app.company_uuid();
    let actor = Actor::user(app.owner_uuid(), None);

    // Both applications are built (validated) against the same state ...
    let a = prepare_submit(
        app.store.as_ref(),
        company,
        command(&application("PAY-A", "DEP-C", 80.0)),
        actor,
    )
    .await
    .unwrap();
    let b = prepare_submit(
        app.store.as_ref(),
        company,
        command(&application("PAY-B", "DEP-C", 80.0)),
        actor,
    )
    .await
    .unwrap();
    // ... the first commits, the second is refused inside its commit.
    app.store.posting_commit(a).await.unwrap();
    match app.store.posting_commit(b).await {
        Err(StoreError::Stale(reason)) => assert!(reason.contains("20.00 left"), "{reason}"),
        other => panic!("expected a stale refusal, got {other:?}"),
    }
    // Retried through the engine it is the ordinary 422.
    let (status, res) = app
        .submit_as("owner", application("PAY-B", "DEP-C", 80.0))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");

    // Cancelling the deposit while an application commits in between.
    let cancel = prepare_cancel(
        app.store.as_ref(),
        company,
        CancelCommand {
            doctype: "Customer Deposit".into(),
            document_id: "DEP-C".into(),
            idempotency_key: None,
        },
        actor,
    )
    .await;
    assert!(cancel.is_err(), "already applied: refused while building");

    // And over HTTP, concurrently: exactly one of two 60s fits in 100.
    let (status, _) = app
        .submit_as("owner", deposit("DEP-D", "CUST-1", 100.0))
        .await;
    assert_eq!(status, StatusCode::OK);
    let token = app.device_token("owner").await;
    let uri = format!("/companies/{}/commands/submit-document", app.company_id);
    let (one, two) = tokio::join!(
        app.request(
            Method::POST,
            &uri,
            Some(&token),
            application("PAY-D1", "DEP-D", 60.0)
        ),
        app.request(
            Method::POST,
            &uri,
            Some(&token),
            application("PAY-D2", "DEP-D", 60.0)
        ),
    );
    let ok = [one.0, two.0]
        .iter()
        .filter(|s| **s == StatusCode::OK)
        .count();
    assert_eq!(ok, 1, "{:?} {:?}", one, two);
}

// ---------------------------------------------------------------------------
// The Team books lock (C5.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_books_lock_moves_only_through_its_command() {
    let mut app = TestApp::new().await;
    let accountant = app.device_token("accountant").await;
    let sales = app.device_token("sales").await;
    let admin = app.device_token("admin").await;

    // Non-finance roles cannot touch it.
    let (status, _) = app.set_books_lock(&sales, json!("2026-06-30")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An accountant locks a period ...
    let (status, res) = app.set_books_lock(&accountant, json!("2026-06-30")).await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["changed"], true);
    assert_eq!(app.books_lock().await, "2026-06-30");
    // ... and may move it later, but not reopen a period.
    let (status, _) = app.set_books_lock(&accountant, json!("2026-05-31")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app.set_books_lock(&accountant, Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app.set_books_lock(&accountant, json!("2026-07-31")).await;
    assert_eq!(status, StatusCode::OK);

    // A stale view never wins: a device that still thinks the lock is
    // 2026-06-30 cannot set it (forward or back) over the newer one.
    let (status, res) = app
        .set_books_lock_expecting(&admin, json!("2026-06-30"), json!("2026-05-01"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{res}");
    assert!(
        res["error"].as_str().unwrap().contains("2026-07-31"),
        "{res}"
    );
    // A retried command (same target) is a no-op, not a conflict.
    let (status, res) = app
        .set_books_lock_expecting(&accountant, json!("2026-07-31"), json!("2026-06-30"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(res["changed"], false);

    // Settings can no longer move it; sending it back unchanged is fine.
    let owner = app.owner_token.clone();
    let uri = format!("/companies/{}/settings", app.company_id);
    let (status, _) = app
        .request(
            Method::PUT,
            &uri,
            Some(&owner),
            json!({ "books_lock_date": null }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = app
        .request(
            Method::PUT,
            &uri,
            Some(&owner),
            json!({ "books_lock_date": "2026-07-31", "allow_negative_stock": true }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(app.books_lock().await, "2026-07-31");

    // Malformed and future dates.
    let (status, _) = app.set_books_lock(&admin, json!("31/07/2026")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = app.set_books_lock(&admin, json!("2099-01-01")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // An admin reopens (clears) it; the audit names who, from and to.
    let (status, _) = app.set_books_lock(&admin, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert!(app.books_lock().await.is_null());
    let entries = audit_actions(&app).await;
    let locks: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            e["action"]
                .as_str()
                .unwrap_or("")
                .starts_with("settings.books_")
        })
        .collect();
    assert_eq!(locks.len(), 3, "two locks and one unlock: {locks:?}");
    let unlock = locks
        .iter()
        .find(|e| e["action"] == "settings.books_unlock")
        .unwrap();
    assert_eq!(unlock["detail"]["from"], "2026-07-31");
    assert!(unlock["detail"]["to"].is_null());
    assert_eq!(unlock["detail"]["role"], "admin");
    assert!(unlock["userId"].is_string() || unlock["user_id"].is_string());
}

#[tokio::test]
async fn backdated_postings_of_every_kind_meet_the_server_lock() {
    let mut app = TestApp::new().await;
    // Drafts made offline before the lock; the lock is set on another device.
    let backdated = [
        journal(
            "JE-OLD",
            "2026-06-15",
            json!([{ "account": "Cash", "debit": 5 }, { "account": "Sales", "credit": 5 }]),
        ),
        expense("EXP-OLD", json!({ "posting_date": "2026-06-15" })),
        {
            let mut d = deposit("DEP-OLD", "CUST-1", 10.0);
            d["payload"]["posting_date"] = json!("2026-06-30");
            d
        },
    ];
    let (status, _) = app
        .submit_as("owner", deposit("DEP-OPEN", "CUST-1", 10.0))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "posted before the lock (2026-09-01)"
    );
    app.put_settings(json!({ "books_lock_date": "2026-06-30" }))
        .await;
    for body in backdated {
        let (status, res) = app.submit_as("owner", body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
        assert!(res["error"].as_str().unwrap().contains("locked"), "{res}");
    }
    // Cancelling into a locked period is refused too.
    app.put_settings(json!({ "books_lock_date": "2026-09-01" }))
        .await;
    let (status, res) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Customer Deposit", "document_id": "DEP-OPEN" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
    // Nothing above consumed a number.
    let (status, res) = app
        .submit_as(
            "owner",
            journal(
                "JE-NEW",
                "2026-09-02",
                json!([{ "account": "Cash", "debit": 5 }, { "account": "Sales", "credit": 5 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["number"], "JV-00001");
}

#[tokio::test]
async fn a_lock_set_between_validation_and_commit_still_wins() {
    let mut app = TestApp::new().await;
    let company = app.company_uuid();
    let actor = Actor::user(app.owner_uuid(), None);
    let body = journal(
        "JE-RACE",
        "2026-06-15",
        json!([{ "account": "Cash", "debit": 5 }, { "account": "Sales", "credit": 5 }]),
    );
    // Built (lock check passed) while the books were open ...
    let commit = prepare_submit(app.store.as_ref(), company, command(&body), actor)
        .await
        .unwrap();
    // ... the lock lands ...
    let accountant = app.device_token("accountant").await;
    let (status, _) = app.set_books_lock(&accountant, json!("2026-06-30")).await;
    assert_eq!(status, StatusCode::OK);
    // ... and the commit is refused inside the transaction.
    match app.store.posting_commit(commit).await {
        Err(StoreError::Stale(reason)) => assert!(reason.contains("locked"), "{reason}"),
        other => panic!("expected the lock to win, got {other:?}"),
    }
    assert_eq!(app.gl_count("JE-RACE").await, 0);

    // Concurrently over HTTP: a submit dated on the new lock date and the
    // lock change race; either the posting landed first (and the lock
    // still applied after it) or it was refused. Never a posting inside a
    // period that was locked when it committed.
    let token = app.device_token("owner").await;
    let uri = format!("/companies/{}/commands/submit-document", app.company_id);
    let late = journal(
        "JE-RACE2",
        "2026-07-15",
        json!([{ "account": "Cash", "debit": 5 }, { "account": "Sales", "credit": 5 }]),
    );
    let (submit, lock) = tokio::join!(
        app.request(Method::POST, &uri, Some(&token), late),
        app.set_books_lock_expecting(&accountant, json!("2026-07-31"), json!("2026-06-30")),
    );
    assert_eq!(lock.0, StatusCode::OK, "{lock:?}");
    assert!(
        submit.0 == StatusCode::OK || submit.0 == StatusCode::UNPROCESSABLE_ENTITY,
        "{submit:?}"
    );
    assert_eq!(app.books_lock().await, "2026-07-31");
}

#[tokio::test]
async fn a_stale_device_cannot_spread_an_old_lock_through_its_company_row() {
    let mut app = TestApp::new().await;
    app.put_settings(json!({ "books_lock_date": "2026-06-30" }))
        .await;
    let token = app.device_token("owner").await;
    let row = json!({ "company_name": "Fixture Trading Ltd", "books_lock_date": "2026-01-31" });
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/sync/push", app.company_id),
            Some(&token),
            json!({ "mutations": [{
                "id": "m-co-1", "type": "updateDocument", "docType": "Company",
                "documentId": "CO-1",
                "payload": { "id": "CO-1", "doctype": "Company", "docstatus": 0,
                             "payload": row.to_string() },
                "deviceId": "stale-dev", "userId": "u", "localTimestamp": 1,
                "syncVersion": null, "status": "pending",
            }]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        app.books_lock().await,
        "2026-06-30",
        "the server lock stands"
    );
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", app.company_id),
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let pulled = body["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["documentId"] == "CO-1")
        .unwrap()
        .clone();
    let inner: Value =
        serde_json::from_str(pulled["payload"]["payload"].as_str().unwrap()).unwrap();
    assert_eq!(
        inner["books_lock_date"], "2026-06-30",
        "other devices get the server lock"
    );
    assert_eq!(inner["company_name"], "Fixture Trading Ltd");
    assert!(audit_actions(&app)
        .await
        .iter()
        .any(|e| e["action"] == "sync.books_lock_pinned"));
}

#[tokio::test]
async fn a_posted_journal_is_immutable_on_the_sync_plane_but_replays_pass() {
    let mut app = TestApp::new().await;
    let token = app.device_token("owner").await;
    let draft = |id: &str| {
        json!({
            "id": id, "type": "createDocument", "docType": "Journal Entry",
            "documentId": "JE-S1",
            "payload": { "id": "JE-S1", "doctype": "Journal Entry", "docstatus": 0,
                         "payload": "{\"posting_date\":\"2026-09-30\"}" },
            "deviceId": "dev-1", "userId": "u", "localTimestamp": 1, "syncVersion": null,
            "status": "pending",
        })
    };
    let uri = format!("/companies/{}/sync/push", app.company_id);
    let (status, _) = app
        .request(
            Method::POST,
            &uri,
            Some(&token),
            json!({ "mutations": [draft("m-je-1")] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = app
        .submit_as(
            "owner",
            journal(
                "JE-S1",
                "2026-09-30",
                json!([{ "account": "Cash", "debit": 5 }, { "account": "Sales", "credit": 5 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The same draft mutation again (a retried push) is a harmless replay ...
    let (status, body) = app
        .request(
            Method::POST,
            &uri,
            Some(&token),
            json!({ "mutations": [draft("m-je-1")] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // ... a new edit of the posted journal is refused.
    let (status, body) = app
        .request(
            Method::POST,
            &uri,
            Some(&token),
            json!({ "mutations": [draft("m-je-2")] }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// Review fix: a padded deposit reference is stored normalized, so it counts
/// against the deposit's balance and blocks its cancellation; a deposit
/// application at a foreign conversion rate is refused (deposits are held in
/// the company currency, so a rate would mis-state the liability).
#[tokio::test]
async fn a_padded_deposit_reference_still_counts_and_a_foreign_rate_is_refused() {
    let mut app = TestApp::new().await;
    let (status, _) = app
        .submit_as("owner", deposit("DEP-P", "CUST-1", 100.0))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, res) = app
        .submit_as("accountant", application("PAY-P1", "  DEP-P ", 60.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    // Only 40 is left: the padded application counted.
    let (status, res) = app
        .submit_as("accountant", application("PAY-P2", "DEP-P", 40.01))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
    assert!(
        res["error"].as_str().unwrap().contains("40.00 left"),
        "{res}"
    );
    // And the deposit cannot be cancelled while it is applied.
    let (status, res) = app
        .cancel_as(
            "accountant",
            json!({ "doctype": "Customer Deposit", "document_id": "DEP-P" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{res}");
    assert!(res["error"].as_str().unwrap().contains("PAY-P1"), "{res}");

    // A foreign-rate application is refused and posts nothing.
    let mut foreign = application("PAY-P3", "DEP-P", 10.0);
    foreign["payload"]["conversion_rate"] = json!(1.1);
    let (status, res) = app.submit_as("accountant", foreign).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{res}");
    assert!(
        res["error"].as_str().unwrap().contains("company currency"),
        "{res}"
    );
    assert!(app.gl_entry("GL-PAY-P3-to").await.is_none());
    // A rate of exactly 1 is the company currency and is accepted.
    let mut home = application("PAY-P4", "DEP-P", 40.0);
    home["payload"]["conversion_rate"] = json!(1.0);
    let (status, res) = app.submit_as("accountant", home).await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert!(approx(app.account_balance("Customer Deposits").await, 0.0));
}
