//! Posting authority end to end: the sync plane cannot publish official
//! state, the command plane binds idempotency keys to their command, and
//! offline POS sales are confirmed under the till's own receipt number
//! without ever being refused for stock.
//!
//! MemStore by default, PgStore with `ATLAS_TEST_DATABASE_URL`.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use support::{approx, TestApp};

fn envelope_mutation(
    id: &str,
    kind: &str,
    doctype: &str,
    document_id: &str,
    docstatus: i64,
) -> Value {
    json!({
        "id": id,
        "type": kind,
        "docType": doctype,
        "documentId": document_id,
        "payload": {
            "id": document_id, "doctype": doctype, "docstatus": docstatus,
            "payload": "{\"account\":\"Cash\",\"debit\":1000000,\"credit\":0}"
        },
        "deviceId": "till-1",
        "userId": "u",
        "localTimestamp": 1i64,
        "syncVersion": null,
        "status": "pending"
    })
}

async fn push(app: &mut TestApp, role: &str, mutations: Vec<Value>) -> (StatusCode, Value) {
    let token = app.device_token(role).await;
    app.request(
        Method::POST,
        &format!("/companies/{}/sync/push", app.company_id),
        Some(&token),
        json!({ "mutations": mutations }),
    )
    .await
}

async fn pulled(app: &mut TestApp) -> Vec<Value> {
    let token = app.device_token("owner").await;
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", app.company_id),
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    body["mutations"].as_array().unwrap().clone()
}

#[tokio::test]
async fn devices_cannot_push_ledger_rows() {
    let mut app = TestApp::new().await;
    for doctype in [
        "GL Entry",
        "Stock Ledger Entry",
        "Bin",
        "Settlement",
        "Customer Transaction",
        "Supplier Transaction",
        "Tax Transaction",
    ] {
        let (status, body) = push(
            &mut app,
            "owner",
            vec![envelope_mutation(
                "m-forged",
                "createDocument",
                doctype,
                "ROW-1",
                0,
            )],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{doctype}: {body}"
        );
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("only the server posts"));
    }
    // A forged row hidden in an otherwise valid batch sinks the whole batch:
    // nothing partial reaches the log.
    let (status, _) = push(
        &mut app,
        "owner",
        vec![
            envelope_mutation("m-ok", "createDocument", "Customer", "CUST-1", 0),
            envelope_mutation("m-bad", "createDocument", "GL Entry", "GL-X", 0),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(pulled(&mut app).await.is_empty());
    assert!(app.all_gl_entries().await.is_empty());
}

#[tokio::test]
async fn devices_cannot_make_server_authority_documents_official() {
    let mut app = TestApp::new().await;
    // Drafts of every server-authority doctype still sync.
    let (status, body) = push(
        &mut app,
        "owner",
        vec![
            envelope_mutation("d-1", "createDocument", "Sales Invoice", "SINV-D", 0),
            envelope_mutation("d-2", "createDocument", "Journal Entry", "JE-D", 0),
            envelope_mutation("d-3", "updateDocument", "Expense", "EXP-D", 0),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // …but no device may flip one to submitted/cancelled on the log, by
    // lifecycle mutation or by a row envelope carrying docstatus >= 1.
    for (kind, doctype, docstatus) in [
        ("submitDocument", "Sales Invoice", 0),
        ("submitDocument", "Journal Entry", 0),
        ("submitDocument", "Customer Deposit", 0),
        ("cancelDocument", "Payment Entry", 0),
        ("updateDocument", "Sales Invoice", 1),
        ("createDocument", "Journal Entry", 1),
        ("updateDocument", "Stock Entry", 2),
    ] {
        let (status, body) = push(
            &mut app,
            "owner",
            vec![envelope_mutation("lc", kind, doctype, "DOC-1", docstatus)],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{kind} {doctype} docstatus {docstatus}: {body}"
        );
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("Atlas Team server"));
    }
    assert_eq!(pulled(&mut app).await.len(), 3);
}

#[tokio::test]
async fn devices_cannot_spoof_backend_provenance() {
    let mut app = TestApp::new().await;
    let mut spoofed = envelope_mutation("m-1", "updateDocument", "Customer", "CUST-1", 0);
    spoofed["deviceId"] = json!("atlas-backend");
    let (status, _) = push(&mut app, "owner", vec![spoofed]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Squatting the deterministic id of a future posting replication would
    // make the real replication a silent no-op (the log is idempotent on id).
    let squat = envelope_mutation("postmut-SINV-9", "updateDocument", "Customer", "CUST-1", 0);
    let (status, _) = push(&mut app, "owner", vec![squat]).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(pulled(&mut app).await.is_empty());
}

#[tokio::test]
async fn idempotency_keys_are_bound_to_their_command() {
    let mut app = TestApp::new().await;
    let invoice = |id: &str, key: &str| {
        json!({
            "doctype": "Sales Invoice",
            "document_id": id,
            "payload": { "customer": "C1", "posting_date": "2026-03-01",
                         "receivable_account": "Debtors", "income_account": "Sales" },
            "items": [{ "item": "SVC", "qty": 1, "rate": 10, "amount": 10 }],
            "idempotency_key": key
        })
    };
    let (status, first) = app.submit_as("sales", invoice("SINV-1", "k-1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["doctype"], "Sales Invoice");

    // The genuine retry replays the original outcome.
    let (status, replay) = app.submit_as("sales", invoice("SINV-1", "k-1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["number"], first["number"]);

    // The same key for a different document is a conflict, never SINV-1's
    // outcome presented as SINV-2's — and SINV-2 is not silently dropped.
    let (status, body) = app.submit_as("sales", invoice("SINV-2", "k-1")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(app
        .store
        .posted_document(app.company_uuid(), "Sales Invoice", "SINV-2")
        .await
        .unwrap()
        .is_none());
    // A cancel cannot ride a submit's key either.
    let (status, _) = app
        .cancel_as(
            "sales",
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-9", "idempotency_key": "k-1" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

fn till_sale(id: &str, series: &str, qty: f64) -> Value {
    json!({
        "doctype": "POS Invoice",
        "document_id": id,
        "offline_fiscal": true,
        "payload": {
            "customer": "WALK-IN",
            "posting_date": "2026-07-03",
            "set_warehouse": "WH",
            "till_series": series,
            "tenders": [{ "mode_of_payment": "Cash", "amount": qty * 12.0 }]
        },
        "items": [{ "item": "ITEM-A", "qty": qty, "rate": 12 }],
        "idempotency_key": format!("submit-POS Invoice-{id}")
    })
}

async fn stock_in(app: &mut TestApp, qty: f64) {
    app.upsert_item(
        json!({ "id": "ITEM-A", "item_type": "Stock", "valuation_method": "Moving Average" }),
    )
    .await;
    let (status, body) = app
        .submit_as(
            "purchasing",
            json!({
                "doctype": "Purchase Receipt",
                "document_id": "PREC-1",
                "payload": { "supplier": "S1", "posting_date": "2026-07-01", "set_warehouse": "WH" },
                "items": [{ "item": "ITEM-A", "qty": qty, "rate": 5 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn offline_pos_sales_are_confirmed_under_the_till_receipt_number() {
    let mut app = TestApp::new().await;
    stock_in(&mut app, 10.0).await;

    // The till already sold offline; its local fiscal lifecycle replicates.
    let (status, body) = push(
        &mut app,
        "pos",
        vec![
            envelope_mutation("p-1", "createDocument", "POS Invoice", "POS-TILL1-0001", 0),
            envelope_mutation("p-2", "submitDocument", "POS Invoice", "POS-TILL1-0001", 1),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Confirmation: the receipt number IS the official number; no series
    // number is consumed; GL / stock / tax are server-derived.
    let (status, body) = app
        .submit_as("pos", till_sale("POS-TILL1-0001", "TILL1", 2.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["number"], "POS-TILL1-0001");
    assert_eq!(body["offline_fiscal"], true);
    assert_eq!(body["stock_shortfalls"], json!([]));
    assert!(app.gl_count("POS-TILL1-0001").await > 0);
    let bin = app.bin("ITEM-A", "WH").await.unwrap();
    assert!(approx(bin.actual_qty, 8.0));

    // A server-numbered POS sale afterwards still starts its own series.
    let (status, body) = app
        .submit_as(
            "pos",
            json!({
                "doctype": "POS Invoice",
                "document_id": "POS-SRV-1",
                "payload": { "customer": "WALK-IN", "posting_date": "2026-07-03", "set_warehouse": "WH" },
                "items": [{ "item": "ITEM-A", "qty": 1, "rate": 12 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["number"], "POS-00001");

    // The confirmed sale is now immutable on the sync plane.
    let (status, _) = push(
        &mut app,
        "pos",
        vec![envelope_mutation(
            "p-3",
            "cancelDocument",
            "POS Invoice",
            "POS-TILL1-0001",
            2,
        )],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A replayed confirmation (dropped response, till retries) is the same
    // outcome, not a second sale.
    let (status, body) = app
        .submit_as("pos", till_sale("POS-TILL1-0001", "TILL1", 2.0))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["number"], "POS-TILL1-0001");
    assert!(approx(
        app.bin("ITEM-A", "WH").await.unwrap().actual_qty,
        7.0
    ));
}

#[tokio::test]
async fn an_offline_sale_is_never_refused_for_stock_but_the_shortfall_is_reported() {
    let mut app = TestApp::new().await;
    stock_in(&mut app, 3.0).await;

    // Two tills sold the last units offline at the same time: 2 + 2 of 3.
    let (status, first) = app
        .submit_as("pos", till_sale("POS-TILL1-0001", "TILL1", 2.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["stock_shortfalls"], json!([]));
    let (status, second) = app
        .submit_as("owner", till_sale("POS-TILL2-0001", "TILL2", 2.0))
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let shortfall = &second["stock_shortfalls"][0];
    assert_eq!(shortfall["item"], "ITEM-A");
    assert!(approx(shortfall["actual_qty"].as_f64().unwrap(), -1.0));
    assert!(approx(
        app.bin("ITEM-A", "WH").await.unwrap().actual_qty,
        -1.0
    ));

    // The shortfall is on the audit trail for the warehouse to reconcile.
    let (_, audit) = app
        .request(
            Method::GET,
            &format!("/companies/{}/audit?limit=50", app.company_id),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert!(audit.to_string().contains("stockShortfalls"));

    // Online (server-authoritative) documents still cannot consume stock
    // that is not there.
    let (status, body) = app
        .submit_as(
            "stock",
            json!({
                "doctype": "Delivery Note",
                "document_id": "DN-1",
                "payload": { "customer": "C1", "posting_date": "2026-07-04", "set_warehouse": "WH" },
                "items": [{ "item": "ITEM-A", "qty": 1, "rate": 12 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn till_series_and_receipt_numbers_cannot_collide_across_registers() {
    let mut app = TestApp::new().await;
    stock_in(&mut app, 10.0).await;
    let (status, _) = app
        .submit_as("pos", till_sale("POS-TILL1-0001", "TILL1", 1.0))
        .await;
    assert_eq!(status, StatusCode::OK);

    // A second register configured with the same series is refused: its
    // receipts would collide with the first till's.
    let (status, body) = app
        .submit_as("owner", till_sale("POS-TILL1-0002", "TILL1", 1.0))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("bound to another device"));

    // The first till continues its own series normally.
    let (status, _) = app
        .submit_as("pos", till_sale("POS-TILL1-0002", "TILL1", 1.0))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn offline_confirmation_preconditions() {
    let mut app = TestApp::new().await;
    stock_in(&mut app, 10.0).await;

    // Only POS Invoices.
    let mut not_pos = till_sale("SINV-1", "TILL1", 1.0);
    not_pos["doctype"] = json!("Sales Invoice");
    let (status, _) = app.submit_as("owner", not_pos).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // A till series is mandatory, and the receipt must come from it.
    let mut no_series = till_sale("POS-2026-0001", "TILL1", 1.0);
    no_series["payload"]
        .as_object_mut()
        .unwrap()
        .remove("till_series");
    let (status, body) = app.submit_as("pos", no_series).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _) = app
        .submit_as("pos", till_sale("POS-TILL9-0001", "TILL1", 1.0))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Everything else is still enforced: role, period lock, totals.
    let (status, _) = app
        .submit_as("sales", till_sale("POS-TILL3-0001", "TILL3", 1.0))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let mut bad_total = till_sale("POS-TILL1-0001", "TILL1", 1.0);
    bad_total["payload"]["grand_total"] = json!(1);
    let (status, _) = app.submit_as("pos", bad_total).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    app.put_settings(json!({ "books_lock_date": "2026-12-31" }))
        .await;
    let (status, _) = app
        .submit_as("pos", till_sale("POS-TILL1-0001", "TILL1", 1.0))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn readiness_reports_the_store() {
    let app = TestApp::new().await;
    let (status, body) = app.request(Method::GET, "/ready", None, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");
    if std::env::var(support::TEST_DB_ENV).is_ok_and(|url| !url.trim().is_empty()) {
        assert!(body["schemaVersion"].as_i64().unwrap() >= 11);
    }
}

#[tokio::test]
async fn pull_reports_the_log_head_so_clients_detect_a_restored_server() {
    let mut app = TestApp::new().await;
    let (status, _) = push(
        &mut app,
        "owner",
        vec![
            envelope_mutation("h-1", "createDocument", "Customer", "C-1", 0),
            envelope_mutation("h-2", "createDocument", "Customer", "C-2", 0),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = app.device_token("owner").await;
    // A device whose cursor (from before a restore) is past the log sees an
    // empty page AND a head below its cursor — its signal to re-pull from 0.
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=40", app.company_id),
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mutations"], json!([]));
    assert_eq!(body["head"], 2);
}

/// An Item master synced by a device the way the hub saves it: fields in the
/// JSON-encoded inner payload, the UOM table under `__children`.
fn item_master(id: &str, fields: Value, uoms: &[(&str, f64)]) -> Value {
    let rows: Vec<Value> = uoms
        .iter()
        .enumerate()
        .map(|(i, (uom, factor))| {
            json!({
                "id": format!("{id}-uoms-{i}"), "parent_id": id, "parent_doctype": "Item",
                "table_name": "uoms", "row_index": i,
                "payload": json!({ "uom": uom, "conversion_factor": factor }).to_string(),
            })
        })
        .collect();
    json!({
        "id": format!("m-item-{id}"),
        "type": "createDocument",
        "docType": "Item",
        "documentId": id,
        "payload": {
            "id": id, "doctype": "Item", "docstatus": 0,
            "payload": fields.to_string(),
            "__children": { "uoms": rows },
        },
        "deviceId": "till-1",
        "userId": "u",
        "localTimestamp": 1i64,
        "syncVersion": null,
        "status": "pending"
    })
}

#[tokio::test]
async fn posting_reads_the_synced_item_master_when_the_registry_lacks_the_item() {
    // The hub never calls the item registry; it syncs its Item master as
    // ordinary documents. Posting must still know a service from a stock item
    // and convert alternative UOMs, or service lines would move stock and
    // split to GRNI and a box of twelve would post as one unit.
    let mut app = TestApp::new().await;
    let (status, body) = push(
        &mut app,
        "owner",
        vec![
            item_master(
                "SVC-SYNC",
                json!({ "item_name": "Install", "item_type": "Service" }),
                &[],
            ),
            item_master(
                "BOX-SYNC",
                json!({ "item_name": "Bottle", "item_type": "Stock", "stock_uom": "Nos" }),
                &[("Box", 12.0)],
            ),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "item sync failed: {body}");

    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Purchase Invoice",
                "document_id": "PINV-SYNC-1",
                "payload": {
                    "supplier": "SUPP-1", "posting_date": "2026-07-06",
                    "update_stock": 1, "set_warehouse": "WH"
                },
                "items": [
                    { "item": "SVC-SYNC", "qty": 1, "rate": 80 },
                    { "item": "BOX-SYNC", "qty": 2, "uom": "Box", "rate": 60 }
                ]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");

    // Only the stock line moves stock, in stock units.
    assert_eq!(app.sle_count("PINV-SYNC-1").await, 1);
    assert!(
        app.bin("SVC-SYNC", "WH").await.is_none(),
        "a service item has no bin"
    );
    let bin = app.bin("BOX-SYNC", "WH").await.unwrap();
    assert!(approx(bin.actual_qty, 24.0), "bin qty {}", bin.actual_qty);
    assert!(
        approx(bin.stock_value, 120.0),
        "bin value {}",
        bin.stock_value
    );
}
