//! Posting-authority guarantees that need real concurrency or the sync
//! plane: gap-free numbering under parallel submits, idempotency-key replay,
//! server-side immutability of posted documents, concurrent oversell safety
//! and cancel/audit behaviour.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use support::{approx, TestApp};

fn service_invoice(document_id: &str) -> Value {
    json!({
        "doctype": "Sales Invoice",
        "document_id": document_id,
        "payload": { "customer": "CUST-1", "posting_date": "2026-07-06" },
        "items": [{ "item": "SVC-1", "qty": 1, "rate": 10 }]
    })
}

#[tokio::test]
async fn gap_free_numbering_under_20_concurrent_submits() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let token = app.device_token("owner").await;
    let uri = format!(
        "/companies/{}/commands/submit-document",
        app.company_id.clone()
    );

    // 20 tasks race the same SINV series through the full HTTP stack.
    let mut handles = Vec::new();
    for i in 0..20 {
        let router = app.router.clone();
        let uri = uri.clone();
        let token = token.clone();
        handles.push(tokio::spawn(async move {
            use axum::body::Body;
            use http_body_util::BodyExt;
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    service_invoice(&format!("SINV-C{i}")).to_string(),
                ))
                .unwrap();
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            (status, body)
        }));
    }

    let mut numbers = Vec::new();
    for handle in handles {
        let (status, body) = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK, "concurrent submit failed: {body}");
        numbers.push(body["number"].as_str().unwrap().to_string());
    }
    numbers.sort();
    // Strictly sequential 1..=20 — no gaps, no duplicates, no races.
    let expected: Vec<String> = (1..=20).map(|n| format!("SINV-{n:05}")).collect();
    assert_eq!(numbers, expected);
}

#[tokio::test]
async fn idempotency_key_replay_returns_same_result_without_double_posting() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "ITEM-A", "item_type": "Stock" }))
        .await;
    let submit = json!({
        "doctype": "Purchase Invoice",
        "document_id": "PINV-I1",
        "payload": {
            "supplier": "SUPP-1",
            "posting_date": "2026-07-06",
            "update_stock": 1,
            "set_warehouse": "WH"
        },
        "items": [{ "item": "ITEM-A", "qty": 10, "rate": 5 }],
        "idempotency_key": "retry-abc-123"
    });

    let (status, first) = app.submit_as("owner", submit.clone()).await;
    assert_eq!(status, StatusCode::OK, "first submit failed: {first}");
    assert_eq!(first["number"], json!("PINV-00001"));

    // Replay: byte-identical response, nothing posted twice.
    let (status, replay) = app.submit_as("owner", submit).await;
    assert_eq!(status, StatusCode::OK, "replay failed: {replay}");
    assert_eq!(first, replay, "replay must return the committed response");
    assert_eq!(app.gl_count("PINV-I1").await, 4, "GL must not double-post");
    assert_eq!(
        app.sle_count("PINV-I1").await,
        1,
        "SLE must not double-post"
    );
    let bin = app.bin("ITEM-A", "WH").await.unwrap();
    assert!(approx(bin.actual_qty, 10.0), "bin qty {}", bin.actual_qty);
    assert!(approx(bin.stock_value, 50.0));

    // The series did not advance on the replay: the next document takes 00002.
    let (status, next) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Purchase Invoice",
                "document_id": "PINV-I2",
                "payload": { "supplier": "SUPP-1", "posting_date": "2026-07-06" },
                "items": [{ "item": "ITEM-A", "qty": 1, "rate": 5 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "next submit failed: {next}");
    assert_eq!(next["number"], json!("PINV-00002"));
}

#[tokio::test]
async fn submitted_documents_are_immutable_via_sync_push() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let (status, body) = app.submit_as("owner", service_invoice("SINV-M1")).await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    let token = app.device_token("owner").await;

    let push = |document_id: &str| {
        json!({ "mutations": [{
            "id": format!("m-{document_id}"),
            "type": "updateDocument",
            "docType": "Sales Invoice",
            "documentId": document_id,
            "payload": { "grand_total": 999999 },
            "deviceId": "dev",
            "userId": "user",
            "localTimestamp": 1751791234567i64,
            "syncVersion": null,
            "status": "pending"
        }]})
    };

    // Mutating the officially posted invoice through the sync plane → 409.
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/sync/push", app.company_id),
            Some(&token),
            push("SINV-M1"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "expected 409, got {body}");
    assert!(
        body["error"].as_str().unwrap().contains("immutable"),
        "unexpected error: {body}"
    );

    // A draft of the same doctype (never officially posted) still syncs.
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/sync/push", app.company_id),
            Some(&token),
            push("SINV-DRAFT-1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "draft push failed: {body}");

    // Cancelled documents are official history too — still immutable.
    let (status, body) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-M1" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {body}");
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/companies/{}/sync/push", app.company_id),
            Some(&token),
            push("SINV-M1"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn concurrent_oversell_admits_exactly_one_winner() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "ITEM-A", "item_type": "Stock" }))
        .await;
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Purchase Invoice",
                "document_id": "PINV-O1",
                "payload": {
                    "supplier": "SUPP-1", "posting_date": "2026-07-06",
                    "update_stock": 1, "set_warehouse": "WH"
                },
                "items": [{ "item": "ITEM-A", "qty": 10, "rate": 5 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "seed purchase failed: {body}");
    let token = app.device_token("owner").await;

    // Two devices race to sell 7 of the 10 on hand: exactly one may win.
    let sell = |id: &str| {
        json!({
            "doctype": "Sales Invoice",
            "document_id": id,
            "payload": {
                "customer": "CUST-1", "posting_date": "2026-07-06",
                "update_stock": 1, "set_warehouse": "WH"
            },
            "items": [{ "item": "ITEM-A", "qty": 7, "rate": 12 }]
        })
    };
    let mut handles = Vec::new();
    for id in ["SINV-O1", "SINV-O2"] {
        let router = app.router.clone();
        let uri = format!("/companies/{}/commands/submit-document", app.company_id);
        let token = token.clone();
        let body = sell(id);
        handles.push(tokio::spawn(async move {
            use axum::body::Body;
            use tower::ServiceExt;
            let request = axum::http::Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap();
            router.oneshot(request).await.unwrap().status()
        }));
    }
    let mut statuses = Vec::new();
    for handle in handles {
        statuses.push(handle.await.unwrap());
    }
    statuses.sort();
    assert_eq!(
        statuses,
        vec![StatusCode::OK, StatusCode::UNPROCESSABLE_ENTITY],
        "exactly one concurrent sale of 7/10 may succeed"
    );
    let bin = app.bin("ITEM-A", "WH").await.unwrap();
    assert!(approx(bin.actual_qty, 3.0), "bin qty {}", bin.actual_qty);
    assert!(approx(app.account_balance("COGS").await, 35.0));
}

#[tokio::test]
async fn cancel_requires_a_submitted_document_and_cannot_repeat() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;

    // Cancelling something that was never submitted → 404.
    let (status, _) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-NONE" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = app.submit_as("owner", service_invoice("SINV-X1")).await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    // Submitting the same document id twice → 409.
    let (status, _) = app.submit_as("owner", service_invoice("SINV-X1")).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let cancel = json!({ "doctype": "Sales Invoice", "document_id": "SINV-X1" });
    let (status, _) = app.cancel_as("owner", cancel.clone()).await;
    assert_eq!(status, StatusCode::OK);
    // Cancelling twice → 409 (already docstatus 2).
    let (status, _) = app.cancel_as("owner", cancel).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn commands_require_device_tokens_and_write_audit_rows() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;

    // A user (non-device) token is authenticated but forbidden.
    let owner_token = app.owner_token.clone();
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/companies/{}/commands/submit-document", app.company_id),
            Some(&owner_token),
            service_invoice("SINV-D1"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An unknown doctype is a 400, not a posting attempt.
    let (status, _) = app
        .submit_as(
            "owner",
            json!({ "doctype": "Journal Entry", "payload": {}, "items": [] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = app.submit_as("owner", service_invoice("SINV-D2")).await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    let (status, body) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-D2" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "cancel failed: {body}");

    // Both commands are in the audit feed with user + device attribution.
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/audit?limit=50", app.company_id),
            Some(&owner_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    for action in ["command.submit-document", "command.cancel-document"] {
        let row = entries
            .iter()
            .find(|entry| entry["action"] == json!(action))
            .unwrap_or_else(|| panic!("missing audit action {action}"));
        assert!(row["userId"].is_string(), "audit row without user: {row}");
        assert!(
            row["deviceId"].is_string(),
            "audit row without device: {row}"
        );
        assert_eq!(row["detail"]["documentId"], json!("SINV-D2"));
    }
}

#[tokio::test]
async fn pos_invoice_and_delivery_note_are_official_and_sync_immutable() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "ITEM-A", "item_type": "Stock" }))
        .await;
    let (status, body) = app
        .submit_as(
            "purchasing",
            json!({
                "doctype": "Purchase Receipt",
                "document_id": "PREC-IM1",
                "payload": { "posting_date": "2026-07-01", "set_warehouse": "WH" },
                "items": [{ "item": "ITEM-A", "qty": 5, "rate": 4 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "receipt failed: {body}");

    let (status, body) = app
        .submit_as(
            "pos",
            json!({
                "doctype": "POS Invoice",
                "document_id": "POS-IM1",
                "payload": { "posting_date": "2026-07-02", "set_warehouse": "WH" },
                "items": [{ "item": "ITEM-A", "qty": 1, "rate": 9 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "pos submit failed: {body}");
    assert_eq!(body["number"], json!("POS-00001"));

    let (status, body) = app
        .submit_as(
            "stock",
            json!({
                "doctype": "Delivery Note",
                "document_id": "DN-IM1",
                "payload": { "posting_date": "2026-07-02", "set_warehouse": "WH" },
                "items": [{ "item": "ITEM-A", "qty": 1, "rate": 9 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "dn submit failed: {body}");
    assert_eq!(body["number"], json!("DN-00001"));

    // Both new doctypes are covered by the sync-plane immutability guard.
    let token = app.device_token("owner").await;
    for (doctype, document_id) in [("POS Invoice", "POS-IM1"), ("Delivery Note", "DN-IM1")] {
        let (status, body) = app
            .request(
                Method::POST,
                &format!("/companies/{}/sync/push", app.company_id),
                Some(&token),
                json!({ "mutations": [{
                    "id": format!("m-{document_id}"),
                    "type": "updateDocument",
                    "docType": doctype,
                    "documentId": document_id,
                    "payload": { "grand_total": 999999 },
                    "deviceId": "dev",
                    "userId": "user",
                    "localTimestamp": 1751791234567i64,
                    "syncVersion": null,
                    "status": "pending"
                }]}),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{doctype} must be immutable via sync push: {body}"
        );
        assert!(
            body["error"].as_str().unwrap().contains("immutable"),
            "unexpected error: {body}"
        );
    }
}
