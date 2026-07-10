//! Fail-closed posting validations (gap analysis §8-C2/C3/C4): the posting
//! authority never trusts client-computed money. Tampered totals, fabricated
//! tax rows, over-allocation, junk posting dates and cancelling a settled
//! invoice are all rejected before anything reaches the official ledger.

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::TestApp;

/// Submits and asserts a 422 whose error message contains `needle`.
async fn expect_422(app: &mut TestApp, role: &str, body: Value, needle: &str) {
    let (status, response) = app.submit_as(role, body).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "expected 422, got {status}: {response}"
    );
    let error = response["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(needle),
        "error {error:?} does not contain {needle:?}"
    );
}

fn service_invoice(document_id: &str, payload_extra: Value, items: Value) -> Value {
    let mut payload = json!({ "customer": "CUST-1", "posting_date": "2026-07-06" });
    if let (Some(base), Some(extra)) = (payload.as_object_mut(), payload_extra.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({
        "doctype": "Sales Invoice",
        "document_id": document_id,
        "payload": payload,
        "items": items
    })
}

async fn app_with_service_item() -> TestApp {
    let app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    app
}

// ---------------------------------------------------------------------------
// posting_date validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn junk_posting_dates_are_rejected_with_422() {
    let mut app = app_with_service_item().await;
    for (i, junk) in [
        "9999",
        "zzz",
        "2026-02-30",
        "2026-13-01",
        "2026-00-10",
        "2026-06-00",
        "1899-12-31",
        "2101-01-01",
        "2026/07/06",
        "2026-7-6",
        "2026-07-06T00:00:00",
    ]
    .iter()
    .enumerate()
    {
        expect_422(
            &mut app,
            "owner",
            service_invoice(
                &format!("SINV-DATE-{i}"),
                json!({ "posting_date": junk }),
                json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
            ),
            "posting_date",
        )
        .await;
    }
    // A non-string posting_date is rejected too, never silently defaulted.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-DATE-NUM",
            json!({ "posting_date": 20260706 }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "posting_date",
    )
    .await;
    // Nothing posted, no official number burnt: the first valid submit is 00001.
    let (status, body) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-DATE-OK",
                json!({}),
                json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "valid submit failed: {body}");
    assert_eq!(body["number"], json!("SINV-00001"));
}

#[tokio::test]
async fn absent_posting_date_defaults_to_today_and_boundary_years_post() {
    let mut app = app_with_service_item().await;
    // Absent → today (derive-when-missing behaviour is preserved).
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-TODAY",
                "payload": { "customer": "CUST-1" },
                "items": [{ "item": "SVC-1", "qty": 1, "rate": 10 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "dateless submit failed: {body}");
    // Boundary years are inclusive.
    for (id, date) in [("SINV-Y1900", "1900-01-01"), ("SINV-Y2100", "2100-12-31")] {
        let (status, body) = app
            .submit_as(
                "owner",
                service_invoice(
                    id,
                    json!({ "posting_date": date }),
                    json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
                ),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "boundary {date} failed: {body}");
    }
}
