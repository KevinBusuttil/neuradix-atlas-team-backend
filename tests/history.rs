//! C4 — server-history identity (`GET …/sync/history`) and the lost-history
//! report (`POST …/commands/report-lost-history`). Runs on the in-memory
//! store, and on a real PostgreSQL database when `ATLAS_TEST_DATABASE_URL`
//! is set. The full restore scenarios (pg_dump / pg_restore under real
//! devices) are in the hub's `test/team_e2e/team_restore_e2e_test.dart`.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};
use support::TestApp;

fn mutation(id: &str, document_id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "type": "createDocument",
        "docType": "Customer",
        "documentId": document_id,
        "payload": {
            "id": document_id,
            "doctype": "Customer",
            "docstatus": 0,
            "payload": json!({ "customer_name": name }).to_string(),
            "created_at": 1751791234567i64,
            "modified_at": 1751791234567i64,
            "sync_state": "synced",
        },
        "deviceId": "device-a",
        "userId": "owner",
        "localTimestamp": 1751791234567i64,
        "syncVersion": null,
        "status": "pending"
    })
}

async fn push(app: &mut TestApp, mutations: Vec<Value>) {
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

async fn history(app: &mut TestApp, at: i64) -> Value {
    let token = app.device_token("owner").await;
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/history?at={at}", app.company_id),
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "history failed: {body}");
    body
}

fn service_invoice(document_id: &str) -> Value {
    json!({
        "doctype": "Sales Invoice",
        "document_id": document_id,
        "payload": { "customer": "CUST-1", "posting_date": "2026-07-06" },
        "items": [{ "item": "SVC-1", "qty": 1, "rate": 10 }]
    })
}

#[tokio::test]
async fn the_chain_identifies_a_history_even_after_the_log_grows_past_the_cursor() {
    // Server A: the history a device saw (versions 1..5).
    let mut a = TestApp::new().await;
    push(
        &mut a,
        (1..=5)
            .map(|i| mutation(&format!("m-{i}"), &format!("C-{i}"), "x"))
            .collect(),
    )
    .await;
    let seen = history(&mut a, 5).await;
    assert_eq!(seen["protocol"], 1);
    assert_eq!(seen["head"], 5);
    assert_eq!(seen["atHash"], seen["headHash"]);

    // Server B: the same log restored to version 3, then different activity
    // advanced it to 6 — past the device's cursor of 5.
    let mut b = TestApp::new().await;
    push(
        &mut b,
        (1..=3)
            .map(|i| mutation(&format!("m-{i}"), &format!("C-{i}"), "x"))
            .collect(),
    )
    .await;
    push(
        &mut b,
        (4..=6)
            .map(|i| mutation(&format!("n-{i}"), &format!("C-{i}"), "y"))
            .collect(),
    )
    .await;
    let now = history(&mut b, 5).await;
    assert_eq!(now["head"], 6, "the numeric head alone would look healthy");
    assert_ne!(
        now["atHash"], seen["atHash"],
        "the changed history is detected"
    );
    // The shared prefix still agrees, so a device knows where they part.
    assert_eq!(
        history(&mut a, 3).await["atHash"],
        history(&mut b, 3).await["atHash"]
    );

    // A log restored behind the checkpoint has no hash there at all.
    let mut c = TestApp::new().await;
    push(
        &mut c,
        (1..=3)
            .map(|i| mutation(&format!("m-{i}"), &format!("C-{i}"), "x"))
            .collect(),
    )
    .await;
    let behind = history(&mut c, 5).await;
    assert_eq!(behind["atHash"], Value::Null);
    assert_eq!(history(&mut c, 0).await["atHash"], "0".repeat(64));
}

#[tokio::test]
async fn same_ids_with_different_content_are_a_different_history() {
    let mut a = TestApp::new().await;
    push(&mut a, vec![mutation("m-1", "C-1", "Harbour Bar")]).await;
    let mut b = TestApp::new().await;
    push(&mut b, vec![mutation("m-1", "C-1", "Harbour Bar Ltd")]).await;
    assert_ne!(
        history(&mut a, 1).await["atHash"],
        history(&mut b, 1).await["atHash"]
    );
    // A retried push of an id already logged does not move the chain.
    let before = history(&mut a, 1).await;
    push(&mut a, vec![mutation("m-1", "C-1", "Harbour Bar")]).await;
    assert_eq!(history(&mut a, 1).await, before);
}

#[tokio::test]
async fn posting_responses_carry_the_checkpoint_past_their_rows() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let (status, body) = app.submit_as("owner", service_invoice("SINV-H1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let version = body["history"]["version"]
        .as_i64()
        .expect("history version");
    let now = history(&mut app, version).await;
    assert_eq!(
        now["head"].as_i64(),
        Some(version),
        "the rows it posted are covered"
    );
    assert_eq!(body["history"]["hash"], now["atHash"]);
    let (status, body) = app
        .cancel_as(
            "owner",
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-H1" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["history"]["version"].as_i64().unwrap() > version);
}

#[tokio::test]
async fn rows_written_before_the_chain_existed_are_linked_on_first_use() {
    let mut app = TestApp::new().await;
    push(
        &mut app,
        (1..=4)
            .map(|i| mutation(&format!("m-{i}"), &format!("C-{i}"), "x"))
            .collect(),
    )
    .await;
    let before = history(&mut app, 2).await;
    let Some(url) = app.pg_url() else {
        return; // the in-memory store has no pre-chain rows
    };
    // What a database migrated from 0012 looks like: rows with no links.
    use sqlx::Connection;
    let mut conn = sqlx::PgConnection::connect(&url).await.unwrap();
    sqlx::query("update mutations set chain_hash = null")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("update sync_counters set last_hash = null")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert_eq!(
        history(&mut app, 2).await,
        before,
        "the backfill reproduces the same chain"
    );
    push(&mut app, vec![mutation("m-5", "C-5", "x")]).await;
    assert_eq!(history(&mut app, 4).await["atHash"], before["headHash"]);
}

#[tokio::test]
async fn a_lost_history_report_flags_reused_numbers_and_raises_the_series() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    // After the restore the server issued SINV-00001 to a different invoice.
    let (status, issued) = app.submit_as("owner", service_invoice("SINV-AFTER")).await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    assert_eq!(issued["number"], "SINV-00001");

    let report = json!({
        "recovery_id": "rec-device-a-1",
        "lost": [
            { "doctype": "Sales Invoice", "document_id": "SINV-LOST-1", "number": "SINV-00001",
              "posting_date": "2026-07-05", "total": 118.0 },
            { "doctype": "Sales Invoice", "document_id": "SINV-LOST-5", "number": "SINV-00005" },
            { "doctype": "Sales Invoice", "document_id": "SINV-AFTER", "number": "SINV-00001" },
            { "doctype": "Journal Entry", "document_id": "JV-LOCAL", "number": null }
        ]
    });
    let token = app.device_token("owner").await;
    let uri = format!("/companies/{}/commands/report-lost-history", app.company_id);
    let (status, body) = app
        .request(Method::POST, &uri, Some(&token), report.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["replayed"], false);
    assert_eq!(body["reported"], 4);
    assert_eq!(body["still_present"].as_array().unwrap().len(), 1);
    let reused = body["number_reused"].as_array().unwrap();
    assert_eq!(reused.len(), 1);
    assert_eq!(reused[0]["lost_document_id"], "SINV-LOST-1");
    assert_eq!(reused[0]["server_document_id"], "SINV-AFTER");
    assert_eq!(
        body["numbering_floors"],
        json!([{ "series": "SINV", "floor": 5 }])
    );

    // The lost numbers are never issued again.
    let (_, next) = app.submit_as("owner", service_invoice("SINV-NEXT")).await;
    assert_eq!(next["number"], "SINV-00006");

    // A retried report replays and changes nothing.
    let (status, again) = app.request(Method::POST, &uri, Some(&token), report).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["replayed"], true);
    assert_eq!(again["number_reused"], body["number_reused"]);
    let (_, after) = app.submit_as("owner", service_invoice("SINV-NEXT-2")).await;
    assert_eq!(after["number"], "SINV-00007");

    // Audited once.
    let (_, audit) = app
        .request(
            Method::GET,
            &format!("/companies/{}/audit", app.company_id),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    let text = audit.to_string();
    assert_eq!(text.matches("sync.lost_history").count(), 1, "{text}");
}

#[tokio::test]
async fn lost_history_reports_are_guarded() {
    let mut app = TestApp::new().await;
    let uri = format!("/companies/{}/commands/report-lost-history", app.company_id);
    let body = json!({ "recovery_id": "rec-1", "lost": [] });

    // Sales staff cannot move numbering.
    let sales = app.device_token("sales").await;
    let (status, _) = app
        .request(Method::POST, &uri, Some(&sales), body.clone())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A user token (no device) is refused.
    let owner_user = app.owner_token.clone();
    let (status, _) = app
        .request(Method::POST, &uri, Some(&owner_user), body.clone())
        .await;
    assert_ne!(status, StatusCode::OK);
    // A malformed recovery id is refused.
    let owner = app.device_token("owner").await;
    let (status, _) = app
        .request(
            Method::POST,
            &uri,
            Some(&owner),
            json!({ "recovery_id": "../x", "lost": [] }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // The accountant may report.
    let accountant = app.device_token("accountant").await;
    let (status, body) = app
        .request(Method::POST, &uri, Some(&accountant), body)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // History needs a device token too.
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/history", app.company_id),
            Some(&owner_user),
            Value::Null,
        )
        .await;
    assert_ne!(status, StatusCode::OK);
}
