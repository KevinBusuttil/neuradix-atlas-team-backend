//! Fail-closed posting validations (gap analysis §8-C2/C3/C4): the posting
//! authority never trusts client-computed money. Tampered totals, fabricated
//! tax rows, over-allocation, junk posting dates and cancelling a settled
//! invoice are all rejected before anything reaches the official ledger.

mod support;

use axum::http::StatusCode;
use serde_json::{json, Value};
use support::{approx, TestApp};

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

// ---------------------------------------------------------------------------
// Totals cross-check (Sales / Purchase / POS Invoice)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tampered_grand_total_is_rejected_with_expected_vs_sent() {
    let mut app = app_with_service_item().await;
    // A €10 line with a claimed €999 grand total: the classic client tamper.
    let (status, response) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-T1",
                json!({ "grand_total": 999 }),
                json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");
    let error = response["error"].as_str().unwrap();
    assert!(error.contains("999"), "sent value missing: {error}");
    assert!(error.contains("10"), "expected value missing: {error}");

    // Tampered per-line amount (amount ≠ qty × rate).
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-T2",
            json!({}),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10, "amount": 500 }]),
        ),
        "amount 500",
    )
    .await;

    // Tampered total / tax_total.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-T3",
            json!({ "total": 42 }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "total: sent 42",
    )
    .await;
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-T4",
            json!({ "tax_total": 3 }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "tax_total: sent 3",
    )
    .await;

    // POS Invoices are covered by the same cross-check.
    let (status, response) = app
        .submit_as(
            "pos",
            json!({
                "doctype": "POS Invoice",
                "document_id": "POS-T1",
                "payload": { "posting_date": "2026-07-06", "grand_total": 999 },
                "items": [{ "item": "SVC-1", "qty": 1, "rate": 10 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{response}");

    // Nothing reached the ledger.
    for voucher in ["SINV-T1", "SINV-T2", "SINV-T3", "SINV-T4", "POS-T1"] {
        assert_eq!(app.gl_count(voucher), 0, "GL leaked for {voucher}");
    }
}

#[tokio::test]
async fn consistent_client_totals_pass_and_client_outstanding_is_ignored() {
    let mut app = app_with_service_item().await;
    // Exactly what the Dart interceptors compute for 2 × €per-unit 10.55 with
    // 18% exclusive VAT — plus a hostile outstanding_amount that must be
    // ignored in favour of the validated grand total.
    let (status, body) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-OK1",
                json!({
                    "total": 21.10,
                    "tax_total": 3.80,
                    "grand_total": 24.90,
                    "outstanding_amount": 0.01,
                    "taxes": [{
                        "tax_type": "VAT",
                        "tax_code": "VAT-18",
                        "tax_account": "VAT Output",
                        "rate": 18,
                        "taxable_amount": 21.10,
                        "tax_amount": 3.80
                    }]
                }),
                json!([{ "item": "SVC-1", "qty": 2, "rate": 10.55, "amount": 21.10 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "consistent submit failed: {body}");
    assert!(
        approx(app.outstanding("Sales Invoice", "SINV-OK1").unwrap(), 24.90),
        "outstanding must be the validated grand total, not the client's"
    );
}

#[tokio::test]
async fn inclusive_pricing_invoice_computed_the_dart_way_passes() {
    let mut app = app_with_service_item().await;
    // Retail pricing: 3 × 9.99 + 1 × 20.03 = €50.00 gross, 18% contained VAT.
    // HubTaxEngine (inclusive): magnitude = round2(50 × 18 / 118) = 7.63,
    // taxable = 42.37, net = 42.37, tax = 7.63, grand = 50.00 = Σ lines.
    let (status, body) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-INC1",
                json!({
                    "prices_include_tax": true,
                    "total": 42.37,
                    "tax_total": 7.63,
                    "grand_total": 50.00,
                    "taxes": [{
                        "tax_type": "VAT",
                        "tax_code": "VAT-18",
                        "tax_account": "VAT Output",
                        "rate": 18,
                        "taxable_amount": 42.37,
                        "tax_amount": 7.63
                    }]
                }),
                json!([
                    { "item": "SVC-1", "qty": 3, "rate": 9.99, "amount": 29.97 },
                    { "item": "SVC-1", "qty": 1, "rate": 20.03, "amount": 20.03 }
                ]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "inclusive submit failed: {body}");
    assert!(approx(
        app.outstanding("Sales Invoice", "SINV-INC1").unwrap(),
        50.0
    ));
    // The GL books net revenue + output VAT, receivable gross.
    assert!(approx(app.voucher_account("Debtors", "SINV-INC1"), 50.0));
    assert!(approx(app.voucher_account("Sales", "SINV-INC1"), -42.37));
    assert!(approx(
        app.voucher_account("VAT Output", "SINV-INC1"),
        -7.63
    ));

    // The same document claiming exclusive maths (grand = lines + tax) fails:
    // in inclusive mode the tax is already inside the line amounts.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-INC2",
            json!({
                "prices_include_tax": true,
                "grand_total": 57.63,
                "taxes": [{
                    "tax_type": "VAT",
                    "rate": 18,
                    "tax_account": "VAT Output",
                    "taxable_amount": 42.37,
                    "tax_amount": 7.63
                }]
            }),
            json!([
                { "item": "SVC-1", "qty": 3, "rate": 9.99 },
                { "item": "SVC-1", "qty": 1, "rate": 20.03 }
            ]),
        ),
        "grand_total: sent 57.63",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Tax-row validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tampered_tax_rows_are_rejected() {
    let mut app = app_with_service_item().await;
    // rate 18 on a €10 base claims €5 of VAT: 422 quoting the expectation.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-TAX1",
            json!({ "taxes": [{
                "tax_type": "VAT",
                "tax_account": "VAT Output",
                "rate": 18,
                "taxable_amount": 10,
                "tax_amount": 5
            }] }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "expected 1.8",
    )
    .await;
    // A zero-rate (exempt) row must carry no tax amount.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-TAX2",
            json!({ "taxes": [{
                "tax_type": "VAT",
                "tax_account": "VAT Output",
                "rate": 0,
                "taxable_amount": 10,
                "tax_amount": 9999
            }] }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "expected 0",
    )
    .await;
    // Negative rates and negative taxable bases (outside a return) fail.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-TAX3",
            json!({ "taxes": [{ "rate": -18, "taxable_amount": 10, "tax_amount": -1.8 }] }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "rate -18",
    )
    .await;
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-TAX4",
            json!({ "taxes": [{ "rate": 18, "taxable_amount": -10, "tax_amount": -1.8 }] }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
        ),
        "taxable_amount -10",
    )
    .await;
    for voucher in ["SINV-TAX1", "SINV-TAX2", "SINV-TAX3", "SINV-TAX4"] {
        assert_eq!(app.gl_count(voucher), 0, "GL leaked for {voucher}");
        assert_eq!(app.tax_transaction_count(voucher), 0);
    }
}

#[tokio::test]
async fn valid_zero_rate_withholding_and_return_tax_rows_pass() {
    let mut app = app_with_service_item().await;
    // Zero-rated export: taxable base captured, no tax owed.
    let (status, body) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-ZR1",
                json!({ "taxes": [{
                    "tax_type": "VAT",
                    "tax_code": "VAT-0",
                    "tax_account": "VAT Output",
                    "rate": 0,
                    "taxable_amount": 10,
                    "tax_amount": 0
                }] }),
                json!([{ "item": "SVC-1", "qty": 1, "rate": 10 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "zero-rate submit failed: {body}");
    assert_eq!(app.tax_transaction_count("SINV-ZR1"), 1);
    assert!(approx(
        app.outstanding("Sales Invoice", "SINV-ZR1").unwrap(),
        10.0
    ));

    // Withholding is deducted: the Dart engine carries its amount negative.
    let (status, body) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-WH1",
                json!({ "taxes": [{
                    "tax_type": "Withholding",
                    "tax_code": "WHT-10",
                    "tax_account": "WHT Receivable",
                    "rate": 10,
                    "taxable_amount": 100,
                    "tax_amount": -10
                }] }),
                json!([{ "item": "SVC-1", "qty": 1, "rate": 100 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "withholding submit failed: {body}");
    assert!(approx(
        app.outstanding("Sales Invoice", "SINV-WH1").unwrap(),
        90.0
    ));
    // Claiming the withholding positive (inflating the receivable) fails.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-WH2",
            json!({ "taxes": [{
                "tax_type": "Withholding",
                "tax_account": "WHT Receivable",
                "rate": 10,
                "taxable_amount": 100,
                "tax_amount": 10
            }] }),
            json!([{ "item": "SVC-1", "qty": 1, "rate": 100 }]),
        ),
        "expected -10",
    )
    .await;

    // A return mirrors the fixtures: negated line amounts, negated tax rows.
    let (status, body) = app
        .submit_as(
            "owner",
            service_invoice(
                "SINV-RET1",
                json!({
                    "is_return": 1,
                    "return_against": "SINV-ZR1",
                    "taxes": [{
                        "tax_type": "VAT",
                        "tax_code": "VAT-15",
                        "tax_account": "VAT Output",
                        "rate": 15,
                        "taxable_amount": -60,
                        "tax_amount": -9
                    }]
                }),
                json!([{ "item": "SVC-1", "qty": -3, "rate": 20 }]),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "return submit failed: {body}");
    // A return must carry its tax negated, never positive.
    expect_422(
        &mut app,
        "owner",
        service_invoice(
            "SINV-RET2",
            json!({
                "is_return": 1,
                "taxes": [{
                    "tax_type": "VAT",
                    "tax_account": "VAT Output",
                    "rate": 15,
                    "taxable_amount": 60,
                    "tax_amount": 9
                }]
            }),
            json!([{ "item": "SVC-1", "qty": -3, "rate": 20 }]),
        ),
        "taxable_amount 60",
    )
    .await;
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
