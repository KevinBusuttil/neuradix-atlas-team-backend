//! Payment plane: pay-link management (create/list/revoke, role gate,
//! distinct token kind), the public pay page (JSON + escaped HTML, live
//! outstanding, paid state, Stripe card handoff vs manual instructions) and
//! the Stripe webhook processor (signature verification, official Payment
//! Entry posting through the engine, replay safety).

mod support;

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use atlas_team_backend::auth::{generate_token, hash_token};
use atlas_team_backend::model::PayLink;
use hmac::{Hmac, Mac};
use support::{approx, TestApp};

/// A raw request with optional Accept header; returns status/headers/bytes.
async fn raw(
    app: &TestApp,
    method: Method,
    uri: &str,
    token: Option<&str>,
    accept: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

/// Submits an official service Sales Invoice through the command API.
async fn submit_invoice(app: &mut TestApp, id: &str, customer: &str, rate: f64) {
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": id,
                "payload": { "customer": customer, "posting_date": "2026-07-06" },
                "items": [{ "item": "SVC-1", "qty": 1, "rate": rate }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
}

/// Creates a pay link as the owner; returns (link id, token).
async fn create_pay_link(app: &TestApp, invoice_id: &str) -> (String, String) {
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/pay-links", app.company_id),
            Some(&app.owner_token),
            json!({ "invoice_id": invoice_id }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "pay-link create failed: {body}"
    );
    (
        body["id"].as_str().unwrap().to_string(),
        body["token"].as_str().unwrap().to_string(),
    )
}

// ---------------------------------------------------------------------------
// Pay-link management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pay_link_create_list_revoke_and_role_gate() {
    let mut app = TestApp::new().await;
    submit_invoice(&mut app, "SINV-P1", "CUST-1", 150.0).await;
    let uri = format!("/companies/{}/pay-links", app.company_id);

    // An unknown invoice is a 404 — a link can only target a real submitted
    // Sales Invoice.
    let (status, body) = app
        .request(
            Method::POST,
            &uri,
            Some(&app.owner_token.clone()),
            json!({ "invoice_id": "SINV-MISSING" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (link_id, token) = create_pay_link(&app, "SINV-P1").await;

    // Default expiry is 60 days.
    let (status, body) = app
        .request(
            Method::GET,
            &uri,
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let links = body["links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["invoiceId"], json!("SINV-P1"));
    assert_eq!(links[0]["revoked"], json!(false));
    // Metadata only — never a token.
    assert!(links[0].get("token").is_none());
    assert!(links[0].get("tokenHash").is_none());

    // Role gate: sales and accountant may manage pay links; stock may not.
    let sales_token = app.device_token("sales").await;
    let (status, body) = app
        .request(
            Method::POST,
            &uri,
            Some(&sales_token),
            json!({ "invoice_id": "SINV-P1", "expires_days": 5 }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "sales blocked: {body}");
    let accountant_token = app.device_token("accountant").await;
    let (status, _) = app
        .request(Method::GET, &uri, Some(&accountant_token), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    let stock_token = app.device_token("stock").await;
    let (status, _) = app
        .request(
            Method::POST,
            &uri,
            Some(&stock_token),
            json!({ "invoice_id": "SINV-P1" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(Method::GET, &uri, Some(&stock_token), Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("{uri}/{link_id}"),
            Some(&stock_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A pay token authenticates no member/device endpoint.
    for member_uri in [
        format!("/companies/{}/audit", app.company_id),
        format!("/companies/{}/pay-links", app.company_id),
    ] {
        let (status, _, _) = raw(&app, Method::GET, &member_uri, Some(&token), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "leaked on {member_uri}");
    }
    // Member tokens never resolve on the pay plane.
    let (status, _, _) = raw(
        &app,
        Method::GET,
        &format!("/pay/{}", app.owner_token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The link works before revocation and 404s after.
    let (status, _, _) = raw(&app, Method::GET, &format!("/pay/{token}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = app
        .request(
            Method::DELETE,
            &format!("{uri}/{link_id}"),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = raw(&app, Method::GET, &format!("/pay/{token}"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Revoking an unknown link is a 404.
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("{uri}/{}", Uuid::new_v4()),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_pay_links_are_not_found() {
    let mut app = TestApp::new().await;
    submit_invoice(&mut app, "SINV-E1", "CUST-1", 90.0).await;

    // Inject timestamps directly through the store: one live, one expired.
    let make = |expires_at| {
        let token = generate_token();
        let link = PayLink {
            id: Uuid::new_v4(),
            company_id: app.company_uuid(),
            invoice_id: "SINV-E1".into(),
            token_hash: hash_token(&token),
            created_by: Uuid::new_v4(),
            created_at: Utc::now() - Duration::days(61),
            expires_at,
            revoked_at: None,
        };
        (token, link)
    };
    let (live_token, live_link) = make(Utc::now() + Duration::days(1));
    let (expired_token, expired_link) = make(Utc::now() - Duration::seconds(1));
    app.store.create_pay_link(live_link).await.unwrap();
    app.store.create_pay_link(expired_link).await.unwrap();

    let (status, _, _) = raw(&app, Method::GET, &format!("/pay/{live_token}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = raw(
        &app,
        Method::GET,
        &format!("/pay/{expired_token}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Pay page rendering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pay_page_renders_json_and_escaped_html_with_payment_handoff() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    // Hostile payload values must come out escaped.
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-H1",
                "payload": {
                    "customer": "CUST-1",
                    "customer_name": "<script>alert(1)</script>",
                    "posting_date": "2026-07-06"
                },
                "items": [{ "item": "Widget & Co", "qty": 2, "rate": 100 }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    let (_, token) = create_pay_link(&app, "SINV-H1").await;
    let uri = format!("/pay/{token}");

    // JSON is the default: live outstanding + totals.
    let (status, _, bytes) = raw(&app, Method::GET, &uri, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["company"], json!("Fixture Trading Ltd"));
    assert_eq!(body["invoice_id"], json!("SINV-H1"));
    assert_eq!(body["official_number"], json!("SINV-00001"));
    assert_eq!(body["posting_date"], json!("2026-07-06"));
    assert_eq!(body["grand_total"], json!(200.0));
    assert_eq!(body["outstanding_amount"], json!(200.0));
    assert_eq!(body["paid"], json!(false));
    assert_eq!(body["items"][0]["item"], json!("Widget & Co"));

    // HTML: manual-payment block when no Stripe URL is configured.
    app.put_settings(json!({ "payment_instructions": "IBAN MT00 & reference <invoice>" }))
        .await;
    let (status, headers, bytes) = raw(&app, Method::GET, &uri, None, Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let html = String::from_utf8(bytes).unwrap();
    assert!(html.contains("Fixture Trading Ltd"));
    assert!(html.contains("SINV-H1"));
    assert!(html.contains("SINV-00001"), "official number:\n{html}");
    assert!(html.contains("2026-07-06"));
    assert!(html.contains("Widget &amp; Co"), "line items:\n{html}");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;") || !html.contains("alert(1)"),
        "hostile payload not escaped:\n{html}"
    );
    assert!(!html.contains("<script>"), "unescaped payload:\n{html}");
    assert!(html.contains("Outstanding"));
    assert!(
        html.contains("Payment instructions"),
        "manual block:\n{html}"
    );
    assert!(
        html.contains("IBAN MT00 &amp; reference &lt;invoice&gt;"),
        "instructions escaped:\n{html}"
    );
    assert!(!html.contains("Pay by card"));

    // Card button iff stripe_payment_link_url is set — href carries the token
    // as client_reference_id, and the manual block gives way.
    app.put_settings(json!({ "stripe_payment_link_url": "https://buy.stripe.com/test_abc" }))
        .await;
    let (status, _, bytes) = raw(&app, Method::GET, &uri, None, Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(bytes).unwrap();
    assert!(html.contains("Pay by card"), "card button:\n{html}");
    assert!(
        html.contains(&format!(
            "https://buy.stripe.com/test_abc?client_reference_id={token}&amp;prefilled_email="
        )),
        "handoff URL:\n{html}"
    );
    assert!(!html.contains("Payment instructions"));

    // Settle the invoice in full: the page flips to the paid state.
    let (status, body) = app
        .submit_as(
            "accountant",
            json!({
                "doctype": "Payment Entry",
                "document_id": "PAY-M1",
                "payload": {
                    "payment_type": "Receive",
                    "party": "CUST-1",
                    "paid_amount": 200.0,
                    "posting_date": "2026-07-07",
                    "references": [{
                        "reference_doctype": "Sales Invoice",
                        "reference_name": "SINV-H1",
                        "allocated_amount": 200.0
                    }]
                },
                "items": []
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "payment failed: {body}");

    let (status, _, bytes) = raw(&app, Method::GET, &uri, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["outstanding_amount"], json!(0.0));
    assert_eq!(body["paid"], json!(true));

    let (status, _, bytes) = raw(&app, Method::GET, &uri, None, Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(bytes).unwrap();
    assert!(
        html.contains("Paid &mdash; thank you"),
        "paid state missing:\n{html}"
    );
    assert!(!html.contains("Pay by card"));
    assert!(!html.contains("Awaiting payment"));
}

// ---------------------------------------------------------------------------
// Stripe webhook processing
// ---------------------------------------------------------------------------

const WEBHOOK_SECRET: &str = "whsec_test_secret";

/// Signs a body the way Stripe does: `v1` = HMAC-SHA256 over `{t}.{body}`.
fn stripe_signature(secret: &str, t: i64, body: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
}

async fn post_webhook(app: &TestApp, body: &str, signature: Option<&str>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/webhooks/payments/stripe")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(signature) = signature {
        builder = builder.header("stripe-signature", signature);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn checkout_event(event_id: &str, token: &str, amount_cents: i64) -> String {
    json!({
        "id": event_id,
        "type": "checkout.session.completed",
        "created": Utc::now().timestamp(),
        "data": { "object": {
            "client_reference_id": token,
            "amount_total": amount_cents,
            "currency": "eur"
        }}
    })
    .to_string()
}

#[tokio::test]
async fn webhook_without_configured_secret_fails_closed() {
    // No STRIPE_WEBHOOK_SECRET: verification is required to fail closed.
    let app = TestApp::new().await;
    let body = checkout_event("evt_nosecret", "sometoken", 1000);
    let signature = stripe_signature("whsec_anything", Utc::now().timestamp(), &body);
    let (status, response) = post_webhook(&app, &body, Some(&signature)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{response}");
    assert_eq!(
        response["error"],
        json!("webhook secret not configured"),
        "{response}"
    );
    // The intake log still recorded the delivery.
    assert_eq!(app.store.webhook_events().await.unwrap().len(), 1);
}

#[tokio::test]
async fn webhook_rejects_bad_signatures_and_stale_timestamps() {
    let app = TestApp::with_stripe_secret(Some(WEBHOOK_SECRET)).await;
    let body = checkout_event("evt_sig", "sometoken", 1000);
    let now = Utc::now().timestamp();

    // Missing header.
    let (status, response) = post_webhook(&app, &body, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");

    // Garbage v1.
    let (status, response) = post_webhook(&app, &body, Some(&format!("t={now},v1=deadbeef"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");

    // Signed with the wrong secret.
    let wrong = stripe_signature("whsec_other", now, &body);
    let (status, _) = post_webhook(&app, &body, Some(&wrong)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Valid signature over a *different* body (tampering).
    let other = stripe_signature(WEBHOOK_SECRET, now, "{}");
    let (status, _) = post_webhook(&app, &body, Some(&other)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Correctly signed but stale (> 5 minutes skew).
    let stale = stripe_signature(WEBHOOK_SECRET, now - 4000, &body);
    let (status, response) = post_webhook(&app, &body, Some(&stale)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
    assert!(
        response["error"].as_str().unwrap().contains("stale"),
        "{response}"
    );

    // A valid signature passes the gate; the unknown token is then a
    // handled:false 200, never a retry-provoking error.
    let valid = stripe_signature(WEBHOOK_SECRET, now, &body);
    let (status, response) = post_webhook(&app, &body, Some(&valid)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["handled"], json!(false));
    assert_eq!(response["reason"], json!("unknown payment link"));

    // Unhandled event types stay log-only.
    let other_type = json!({
        "id": "evt_other",
        "type": "invoice.finalized",
        "data": { "object": {} }
    })
    .to_string();
    let signature = stripe_signature(WEBHOOK_SECRET, now, &other_type);
    let (status, response) = post_webhook(&app, &other_type, Some(&signature)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["handled"], json!(false));

    // Nothing was ever posted.
    assert_eq!(app.all_gl_entries().await.len(), 0);
}

#[tokio::test]
async fn webhook_posts_official_payment_entry_and_replays_idempotently() {
    let mut app = TestApp::with_stripe_secret(Some(WEBHOOK_SECRET)).await;
    submit_invoice(&mut app, "SINV-W1", "CUST-1", 150.0).await;
    let (_, token) = create_pay_link(&app, "SINV-W1").await;
    assert!(approx(
        app.outstanding("Sales Invoice", "SINV-W1").await.unwrap(),
        150.0
    ));

    // Happy path: checkout.session.completed for the full amount.
    let body = checkout_event("evt_W1", &token, 15000);
    let signature = stripe_signature(WEBHOOK_SECRET, Utc::now().timestamp(), &body);
    let (status, response) = post_webhook(&app, &body, Some(&signature)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["handled"], json!(true));
    assert_eq!(response["document_id"], json!("PAY-STRIPE-evt_W1"));
    assert_eq!(response["number"], json!("PAY-00001"));
    assert_eq!(response["replayed"], json!(false));

    // The engine settled the invoice: outstanding dropped, the settlement
    // exists, the official number was allocated.
    assert!(approx(
        app.outstanding("Sales Invoice", "SINV-W1").await.unwrap(),
        0.0
    ));
    assert_eq!(app.settlement_count("PAY-STRIPE-evt_W1").await, 1);
    let payment = app
        .store
        .posted_document(app.company_uuid(), "Payment Entry", "PAY-STRIPE-evt_W1")
        .await
        .unwrap()
        .expect("payment entry not posted");
    assert_eq!(payment.docstatus, 1);
    assert_eq!(payment.official_number.as_deref(), Some("PAY-00001"));
    assert_eq!(payment.payload["payment_type"], json!("Receive"));
    assert_eq!(payment.payload["party"], json!("CUST-1"));
    assert_eq!(payment.payload["paid_amount"], json!(150.0));

    // The posting replicated to the mutation log under the payments device.
    let device = app.device_token("owner").await;
    let (status, pulled) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", app.company_id),
            Some(&device),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mutations = pulled["mutations"].as_array().unwrap();
    let doc_mutation = mutations
        .iter()
        .find(|m| m["id"] == json!("postmut-PAY-STRIPE-evt_W1-doc"))
        .expect("payment document mutation missing");
    assert_eq!(doc_mutation["deviceId"], json!("atlas-payments"));
    assert_eq!(doc_mutation["type"], json!("submitDocument"));
    assert!(
        mutations.iter().any(
            |m| m["docType"] == json!("Settlement") && m["deviceId"] == json!("atlas-payments")
        ),
        "settlement mutation missing"
    );
    // The invoice submitted through the command API still replicates under
    // the default backend device id.
    assert!(mutations
        .iter()
        .any(|m| m["deviceId"] == json!("atlas-backend")));
    let mutation_count = mutations.len();

    // Redelivery of the same event id replays: nothing double-posts.
    let signature = stripe_signature(WEBHOOK_SECRET, Utc::now().timestamp(), &body);
    let (status, response) = post_webhook(&app, &body, Some(&signature)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["handled"], json!(true));
    assert_eq!(response["number"], json!("PAY-00001"));
    assert_eq!(response["replayed"], json!(true));
    assert_eq!(app.settlement_count("PAY-STRIPE-evt_W1").await, 1);
    assert_eq!(app.gl_count("PAY-STRIPE-evt_W1").await, 2);
    assert!(approx(
        app.outstanding("Sales Invoice", "SINV-W1").await.unwrap(),
        0.0
    ));
    let (_, pulled) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", app.company_id),
            Some(&device),
            Value::Null,
        )
        .await;
    assert_eq!(
        pulled["mutations"].as_array().unwrap().len(),
        mutation_count,
        "replay appended mutations"
    );

    // A *new* event against the now-settled invoice is refused politely.
    let body = checkout_event("evt_W2", &token, 15000);
    let signature = stripe_signature(WEBHOOK_SECRET, Utc::now().timestamp(), &body);
    let (status, response) = post_webhook(&app, &body, Some(&signature)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["handled"], json!(false));
    assert_eq!(
        response["reason"],
        json!("invoice has no outstanding amount")
    );

    // The replay burnt no number: the next payment takes PAY-00002.
    submit_invoice(&mut app, "SINV-W3", "CUST-2", 80.0).await;
    let (_, token3) = create_pay_link(&app, "SINV-W3").await;
    let body = checkout_event("evt_W3", &token3, 8000);
    let signature = stripe_signature(WEBHOOK_SECRET, Utc::now().timestamp(), &body);
    let (status, response) = post_webhook(&app, &body, Some(&signature)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["number"], json!("PAY-00002"));

    // Audit trail: the processed payments and the rejected redelivery.
    let (status, audit) = app
        .request(
            Method::GET,
            &format!("/companies/{}/audit?limit=200", app.company_id),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = audit["entries"].as_array().unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["action"] == json!("pay.webhook.payment"))
            .count(),
        3, // evt_W1, its replay, evt_W3
    );
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["action"] == json!("pay.webhook.rejected"))
            .count(),
        1, // evt_W2 against the settled invoice
    );
    // The system-posted command audit row carries no user (system actor).
    let posting = entries
        .iter()
        .find(|e| {
            e["action"] == json!("command.submit-document")
                && e["detail"]["documentId"] == json!("PAY-STRIPE-evt_W1")
        })
        .expect("posting audit row missing");
    assert!(posting["userId"].is_null(), "{posting}");
}
