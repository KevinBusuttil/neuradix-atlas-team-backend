//! Posted results must replicate to client devices through the sync plane:
//! every submit/cancel appends system-authored mutations (device
//! `atlas-backend`) to the company mutation log, in the Dart sync engine's
//! row-envelope wire shape, inside the same atomic commit — and idempotency
//! replays append nothing new.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use support::{approx, TestApp};
use uuid::Uuid;

async fn pull(app: &mut TestApp, after: i64) -> Vec<Value> {
    let token = app.device_token("owner").await;
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after={after}", app.company_id),
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "pull failed: {body}");
    body["mutations"].as_array().unwrap().clone()
}

fn find<'a>(mutations: &'a [Value], id: &str) -> &'a Value {
    mutations
        .iter()
        .find(|m| m["id"] == json!(id))
        .unwrap_or_else(|| panic!("missing mutation {id} in {mutations:#?}"))
}

/// Parses a mutation's envelope `payload` field — a JSON-encoded string of
/// the row's fields, per the Dart sync contract.
fn envelope_fields(mutation: &Value) -> Value {
    let text = mutation["payload"]["payload"]
        .as_str()
        .unwrap_or_else(|| panic!("envelope payload must be a JSON string: {mutation:#?}"));
    serde_json::from_str(text).expect("envelope payload string must parse as JSON")
}

fn fixture_0001_purchase(idempotency_key: &str) -> Value {
    json!({
        "doctype": "Purchase Invoice",
        "document_id": "PINV-1",
        "payload": {
            "supplier": "SUPP-1",
            "posting_date": "2026-07-01",
            "update_stock": 1,
            "set_warehouse": "WH"
        },
        "items": [{ "item": "ITEM-A", "qty": 10, "rate": 5 }],
        "idempotency_key": idempotency_key
    })
}

async fn stock_item_app() -> TestApp {
    let app = TestApp::new().await;
    app.upsert_item(json!({
        "id": "ITEM-A", "item_type": "Stock", "valuation_method": "Moving Average"
    }))
    .await;
    app
}

#[tokio::test]
async fn submit_replicates_document_gl_sle_and_bin_mutations() {
    let mut app = stock_item_app().await;
    let (status, body) = app
        .submit_as("owner", fixture_0001_purchase("repl-0001"))
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    assert_eq!(body["number"], json!("PINV-00001"));

    let mutations = pull(&mut app, 0).await;

    // Every replicated mutation is system-authored, user-attributed, pushed,
    // and carries a server-assigned sync version.
    for mutation in &mutations {
        assert_eq!(mutation["deviceId"], json!("atlas-backend"), "{mutation}");
        assert_eq!(mutation["status"], json!("pushed"), "{mutation}");
        assert!(
            mutation["userId"].as_str().unwrap().parse::<Uuid>().is_ok(),
            "userId must be the acting user's uuid: {mutation}"
        );
        assert!(
            mutation["syncVersion"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .is_ok(),
            "syncVersion must be assigned: {mutation}"
        );
    }

    // The document: submitDocument, envelope docstatus 1, payload string with
    // the official number merged in and no child rows.
    let doc = find(&mutations, "postmut-PINV-1-doc");
    assert_eq!(doc["type"], json!("submitDocument"));
    assert_eq!(doc["docType"], json!("Purchase Invoice"));
    assert_eq!(doc["documentId"], json!("PINV-1"));
    assert_eq!(doc["payload"]["id"], json!("PINV-1"));
    assert_eq!(doc["payload"]["doctype"], json!("Purchase Invoice"));
    assert_eq!(doc["payload"]["docstatus"], json!(1));
    assert_eq!(doc["payload"]["sync_state"], json!("synced"));
    assert!(doc["payload"].get("__children").is_none());
    let fields = envelope_fields(doc);
    assert_eq!(fields["official_number"], json!("PINV-00001"));
    assert_eq!(fields["supplier"], json!("SUPP-1"));
    assert_eq!(fields["posting_date"], json!("2026-07-01"));
    assert!(
        fields.get("items").is_none(),
        "line items must not ride in the header payload: {fields}"
    );

    // GL entries with the Dart field names. The AP leg carries the party.
    let ap = find(&mutations, "postmut-GL-PINV-1-credit");
    assert_eq!(ap["type"], json!("createDocument"));
    assert_eq!(ap["docType"], json!("GL Entry"));
    assert_eq!(ap["documentId"], json!("GL-PINV-1-credit"));
    let fields = envelope_fields(ap);
    assert_eq!(fields["account"], json!("Creditors"));
    assert!(approx(fields["debit"].as_f64().unwrap(), 0.0));
    assert!(approx(fields["credit"].as_f64().unwrap(), 50.0));
    assert_eq!(fields["party_type"], json!("Supplier"));
    assert_eq!(fields["party"], json!("SUPP-1"));
    assert_eq!(fields["voucher_type"], json!("Purchase Invoice"));
    assert_eq!(fields["voucher_no"], json!("PINV-1"));
    assert_eq!(fields["is_reversal"], json!(false));
    assert_eq!(fields["posting_date"], json!("2026-07-01"));

    // A non-party leg omits party_type/party entirely (Dart `_gl` semantics).
    let inventory = find(&mutations, "postmut-SLE-PINV-1-0-gl-d");
    let fields = envelope_fields(inventory);
    assert_eq!(fields["account"], json!("Stock"));
    assert!(approx(fields["debit"].as_f64().unwrap(), 50.0));
    assert!(fields.get("party_type").is_none(), "{fields}");
    assert!(fields.get("party").is_none(), "{fields}");
    // The remaining perpetual-inventory legs replicate too.
    find(&mutations, "postmut-SLE-PINV-1-0-gl-c");
    find(&mutations, "postmut-GL-PINV-1-grni");

    // The stock ledger entry with the Dart `_sle` field names.
    let sle = find(&mutations, "postmut-SLE-PINV-1-0");
    assert_eq!(sle["type"], json!("createDocument"));
    assert_eq!(sle["docType"], json!("Stock Ledger Entry"));
    let fields = envelope_fields(sle);
    assert_eq!(fields["trans_type"], json!("Receipt"));
    assert_eq!(fields["item"], json!("ITEM-A"));
    assert_eq!(fields["warehouse"], json!("WH"));
    assert!(approx(fields["qty_change"].as_f64().unwrap(), 10.0));
    assert!(approx(fields["valuation_rate"].as_f64().unwrap(), 5.0));
    assert_eq!(fields["voucher_type"], json!("Purchase Invoice"));
    assert_eq!(fields["voucher_no"], json!("PINV-1"));
    assert_eq!(fields["is_reversal"], json!(false));

    // The recomputed bin as an updateDocument on `BIN-{item}-{warehouse}`.
    let bin = find(&mutations, "postmut-PINV-1-bin-ITEM-A-WH");
    assert_eq!(bin["type"], json!("updateDocument"));
    assert_eq!(bin["docType"], json!("Bin"));
    assert_eq!(bin["documentId"], json!("BIN-ITEM-A-WH"));
    assert_eq!(bin["payload"]["docstatus"], json!(0));
    let fields = envelope_fields(bin);
    assert_eq!(fields["item"], json!("ITEM-A"));
    assert_eq!(fields["warehouse"], json!("WH"));
    assert!(approx(fields["actual_qty"].as_f64().unwrap(), 10.0));
    assert!(approx(fields["stock_value"].as_f64().unwrap(), 50.0));
    assert!(approx(fields["valuation_rate"].as_f64().unwrap(), 5.0));
}

#[tokio::test]
async fn submit_and_cancel_replicate_subledger_rows_in_dart_envelopes() {
    let mut app = stock_item_app().await;
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-SL1",
                "payload": {
                    "customer": "CUST-1",
                    "posting_date": "2026-07-02",
                    "due_date": "2026-08-01",
                    "taxes": [{
                        "tax_type": "VAT",
                        "tax_code": "VAT-15",
                        "tax_account": "VAT Output",
                        "taxable_amount": 60,
                        "tax_amount": 9,
                        "rate": 15
                    }]
                },
                "items": [{ "item": "ITEM-A", "qty": 3, "rate": 20 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");

    let mutations = pull(&mut app, 0).await;

    // Customer Transaction — `CT-{id}`, party under its `customer` field.
    let ct = find(&mutations, "postmut-CT-SINV-SL1");
    assert_eq!(ct["type"], json!("createDocument"));
    assert_eq!(ct["docType"], json!("Customer Transaction"));
    assert_eq!(ct["documentId"], json!("CT-SINV-SL1"));
    assert_eq!(ct["deviceId"], json!("atlas-backend"));
    assert_eq!(ct["payload"]["doctype"], json!("Customer Transaction"));
    let fields = envelope_fields(ct);
    assert_eq!(fields["trans_type"], json!("Invoice"));
    assert_eq!(fields["customer"], json!("CUST-1"));
    assert_eq!(fields["posting_date"], json!("2026-07-02"));
    assert_eq!(fields["due_date"], json!("2026-08-01"));
    assert!(approx(fields["amount"].as_f64().unwrap(), 69.0));
    assert!(approx(fields["base_amount"].as_f64().unwrap(), 69.0));
    assert!(approx(fields["conversion_rate"].as_f64().unwrap(), 1.0));
    assert_eq!(fields["voucher_type"], json!("Sales Invoice"));
    assert_eq!(fields["voucher_no"], json!("SINV-SL1"));
    assert_eq!(fields["is_reversal"], json!(false));
    assert!(fields.get("party").is_none(), "{fields}");

    // Tax Transaction — `TT-{id}-{i}` with base + tax + rate.
    let tt = find(&mutations, "postmut-TT-SINV-SL1-0");
    assert_eq!(tt["docType"], json!("Tax Transaction"));
    let fields = envelope_fields(tt);
    assert_eq!(fields["tax_type"], json!("VAT"));
    assert_eq!(fields["tax"], json!("VAT-15"));
    assert!(approx(fields["base_amount"].as_f64().unwrap(), 60.0));
    assert!(approx(fields["tax_amount"].as_f64().unwrap(), 9.0));
    assert!(approx(fields["rate"].as_f64().unwrap(), 15.0));
    assert_eq!(fields["party_type"], json!("Customer"));
    assert_eq!(fields["party"], json!("CUST-1"));
    assert_eq!(fields["is_reversal"], json!(false));

    // The document's header payload must not carry the `taxes` child table.
    let doc = find(&mutations, "postmut-SINV-SL1-doc");
    let fields = envelope_fields(doc);
    assert!(fields.get("taxes").is_none(), "{fields}");
    assert!(fields.get("items").is_none(), "{fields}");

    // Cancel: negated subledger rows with `-reversal` ids and the Dart cancel
    // trans_type (Invoice → CreditNote).
    let last: i64 = mutations.last().unwrap()["syncVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (status, body) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-SL1" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {body}");
    let mutations = pull(&mut app, last).await;

    let ct = find(&mutations, "postmut-CT-SINV-SL1-reversal");
    assert_eq!(ct["docType"], json!("Customer Transaction"));
    let fields = envelope_fields(ct);
    assert_eq!(fields["trans_type"], json!("CreditNote"));
    assert!(approx(fields["amount"].as_f64().unwrap(), -69.0));
    assert!(approx(fields["base_amount"].as_f64().unwrap(), -69.0));
    assert_eq!(fields["is_reversal"], json!(true));

    let tt = find(&mutations, "postmut-TT-SINV-SL1-0-reversal");
    let fields = envelope_fields(tt);
    assert!(approx(fields["base_amount"].as_f64().unwrap(), -60.0));
    assert!(approx(fields["tax_amount"].as_f64().unwrap(), -9.0));
    assert!(approx(fields["rate"].as_f64().unwrap(), 15.0));
    assert_eq!(fields["is_reversal"], json!(true));
}

#[tokio::test]
async fn base_stamped_gl_and_party_fields_replicate() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-FX-R1",
                "payload": {
                    "customer": "CUST-1",
                    "posting_date": "2026-07-01",
                    "currency": "USD",
                    "conversion_rate": 0.9
                },
                "items": [{ "item": "SVC-1", "qty": 1, "rate": 100 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");

    let mutations = pull(&mut app, 0).await;

    // GL leg with the Dart `_stampBaseAmounts` fields.
    let gl = find(&mutations, "postmut-GL-SINV-FX-R1-debit");
    let fields = envelope_fields(gl);
    assert!(approx(fields["debit"].as_f64().unwrap(), 100.0));
    assert!(approx(fields["base_debit"].as_f64().unwrap(), 90.0));
    assert!(approx(fields["base_credit"].as_f64().unwrap(), 0.0));
    assert!(approx(fields["conversion_rate"].as_f64().unwrap(), 0.9));
    assert_eq!(fields["currency"], json!("USD"));

    // Customer Transaction with base_amount + conversion_rate + currency.
    let ct = find(&mutations, "postmut-CT-SINV-FX-R1");
    let fields = envelope_fields(ct);
    assert!(approx(fields["amount"].as_f64().unwrap(), 100.0));
    assert!(approx(fields["base_amount"].as_f64().unwrap(), 90.0));
    assert!(approx(fields["conversion_rate"].as_f64().unwrap(), 0.9));
    assert_eq!(fields["currency"], json!("USD"));
}

#[tokio::test]
async fn payment_replicates_supplier_transaction_row() {
    let mut app = TestApp::new().await;
    let (status, body) = app
        .submit_as(
            "accountant",
            json!({
                "doctype": "Payment Entry",
                "document_id": "PAY-SUP-1",
                "payload": {
                    "payment_type": "Pay",
                    "party": "SUPP-1",
                    "paid_amount": 40,
                    "posting_date": "2026-07-02"
                },
                "items": []
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "payment failed: {body}");

    let mutations = pull(&mut app, 0).await;
    // Supplier Transaction — the Dart `VT-{id}` id scheme, party under
    // `supplier`, negative on submit (a payment reduces what we owe).
    let vt = find(&mutations, "postmut-VT-PAY-SUP-1");
    assert_eq!(vt["docType"], json!("Supplier Transaction"));
    let fields = envelope_fields(vt);
    assert_eq!(fields["trans_type"], json!("Payment"));
    assert_eq!(fields["supplier"], json!("SUPP-1"));
    assert!(approx(fields["amount"].as_f64().unwrap(), -40.0));
    assert_eq!(fields["is_reversal"], json!(false));
}

#[tokio::test]
async fn idempotency_replay_appends_no_new_mutations() {
    let mut app = stock_item_app().await;
    let (status, first) = app
        .submit_as("owner", fixture_0001_purchase("repl-replay"))
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {first}");
    let before = pull(&mut app, 0).await;
    assert!(!before.is_empty());

    let (status, replay) = app
        .submit_as("owner", fixture_0001_purchase("repl-replay"))
        .await;
    assert_eq!(status, StatusCode::OK, "replay failed: {replay}");
    assert_eq!(first, replay);

    let after = pull(&mut app, 0).await;
    assert_eq!(
        before.len(),
        after.len(),
        "an idempotency replay must not append to the mutation log"
    );
}

#[tokio::test]
async fn cancel_replicates_cancel_document_and_reversal_rows() {
    let mut app = stock_item_app().await;
    let (status, body) = app
        .submit_as("owner", fixture_0001_purchase("repl-cancel"))
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    let submitted = pull(&mut app, 0).await;
    let last_version: i64 = submitted.last().unwrap()["syncVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let (status, body) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Purchase Invoice", "document_id": "PINV-1" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {body}");

    // Only the cancel batch is new past the submit cursor.
    let mutations = pull(&mut app, last_version).await;

    let doc = find(&mutations, "postmut-PINV-1-doc-reversal");
    assert_eq!(doc["type"], json!("cancelDocument"));
    assert_eq!(doc["docType"], json!("Purchase Invoice"));
    assert_eq!(doc["documentId"], json!("PINV-1"));
    assert_eq!(doc["payload"]["docstatus"], json!(2));
    assert_eq!(doc["deviceId"], json!("atlas-backend"));
    let fields = envelope_fields(doc);
    assert_eq!(
        fields["official_number"],
        json!("PINV-00001"),
        "a cancelled document keeps its official number"
    );

    // Reversal GL / SLE rows arrive as createDocument mutations.
    let ap = find(&mutations, "postmut-GL-PINV-1-credit-reversal");
    assert_eq!(ap["type"], json!("createDocument"));
    let fields = envelope_fields(ap);
    assert!(approx(fields["debit"].as_f64().unwrap(), 50.0));
    assert!(approx(fields["credit"].as_f64().unwrap(), 0.0));
    assert_eq!(fields["is_reversal"], json!(true));

    let sle = find(&mutations, "postmut-SLE-PINV-1-0-reversal");
    let fields = envelope_fields(sle);
    assert!(approx(fields["qty_change"].as_f64().unwrap(), -10.0));
    assert!(approx(fields["valuation_rate"].as_f64().unwrap(), 5.0));
    assert_eq!(fields["is_reversal"], json!(true));

    // The bin update reflects the restored (empty) balance.
    let bin = find(&mutations, "postmut-PINV-1-bin-ITEM-A-WH-reversal");
    assert_eq!(bin["type"], json!("updateDocument"));
    assert_eq!(bin["documentId"], json!("BIN-ITEM-A-WH"));
    let fields = envelope_fields(bin);
    assert!(approx(fields["actual_qty"].as_f64().unwrap(), 0.0));
    assert!(approx(fields["stock_value"].as_f64().unwrap(), 0.0));
}

#[tokio::test]
async fn payment_replicates_settlement_and_outstanding_update() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-P1",
                "payload": { "customer": "CUST-1", "posting_date": "2026-07-01" },
                "items": [{ "item": "SVC-1", "qty": 1, "rate": 100 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "invoice failed: {body}");

    let (status, body) = app
        .submit_as(
            "accountant",
            json!({
                "doctype": "Payment Entry",
                "document_id": "PAY-1",
                "payload": {
                    "payment_type": "Receive",
                    "party": "CUST-1",
                    "paid_amount": 60,
                    "posting_date": "2026-07-02",
                    "references": [{
                        "reference_doctype": "Sales Invoice",
                        "reference_name": "SINV-P1",
                        "allocated_amount": 60
                    }]
                },
                "items": []
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "payment failed: {body}");

    let mutations = pull(&mut app, 0).await;

    // The settlement row with the Dart field names.
    let settlement = find(&mutations, "postmut-STL-PAY-1-0");
    assert_eq!(settlement["type"], json!("createDocument"));
    assert_eq!(settlement["docType"], json!("Settlement"));
    assert_eq!(settlement["deviceId"], json!("atlas-backend"));
    let fields = envelope_fields(settlement);
    assert_eq!(fields["payment_voucher_type"], json!("Payment Entry"));
    assert_eq!(fields["payment_voucher_no"], json!("PAY-1"));
    assert_eq!(fields["invoice_voucher_type"], json!("Sales Invoice"));
    assert_eq!(fields["invoice_voucher_no"], json!("SINV-P1"));
    assert_eq!(fields["party_type"], json!("Customer"));
    assert_eq!(fields["party"], json!("CUST-1"));
    assert!(approx(fields["allocated_amount"].as_f64().unwrap(), 60.0));
    assert_eq!(fields["is_reversal"], json!(false));

    // The referenced invoice's outstanding maintenance: a header-only
    // updateDocument whose payload carries the recomputed outstanding.
    let update = find(&mutations, "postmut-PAY-1-outstanding-SINV-P1");
    assert_eq!(update["type"], json!("updateDocument"));
    assert_eq!(update["docType"], json!("Sales Invoice"));
    assert_eq!(update["documentId"], json!("SINV-P1"));
    assert_eq!(update["payload"]["docstatus"], json!(1));
    assert!(update["payload"].get("__children").is_none());
    let fields = envelope_fields(update);
    assert!(approx(fields["outstanding_amount"].as_f64().unwrap(), 40.0));
    assert!(approx(fields["grand_total"].as_f64().unwrap(), 100.0));
    assert!(fields.get("items").is_none());

    // The invoice's own submit mutation replicated with its initial
    // outstanding (100) — the update supersedes it at a higher version.
    let invoice = find(&mutations, "postmut-SINV-P1-doc");
    let fields = envelope_fields(invoice);
    assert!(approx(
        fields["outstanding_amount"].as_f64().unwrap(),
        100.0
    ));
    let v = |m: &Value| m["syncVersion"].as_str().unwrap().parse::<i64>().unwrap();
    assert!(v(update) > v(invoice));
}
