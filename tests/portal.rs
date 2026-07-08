//! Portal plane: link management (create/list/revoke, role gate, distinct
//! token kind), the customer portal (strict per-customer scoping, quotation
//! accept/reject through the mutation log, HTML rendering with escaping), the
//! accountant portal (summary, GL CSV, audit feed) and the materialized
//! `company_documents` read model (incremental fold + rebuild equivalence).

mod support;

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use atlas_team_backend::auth::{generate_token, hash_token};
use atlas_team_backend::model::{PortalLink, PortalLinkKind};
use atlas_team_backend::store::Store;
use support::TestApp;

/// A raw request with an optional Accept header; returns the response bytes.
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

/// A client draft mutation in the Dart row-envelope wire shape: the mutation
/// payload is the envelope, whose inner `payload` is a JSON-encoded string of
/// the fields; `__children` rides inside the envelope.
fn envelope_mutation(
    mutation_id: &str,
    mutation_type: &str,
    doctype: &str,
    document_id: &str,
    docstatus: i64,
    fields: Value,
    children: Option<Value>,
) -> Value {
    let mut envelope = json!({
        "id": document_id,
        "doctype": doctype,
        "company": null,
        "docstatus": docstatus,
        "payload": fields.to_string(),
        "created_at": 1751791234567i64,
        "modified_at": 1751791234567i64,
        "sync_version": null,
        "sync_state": "synced",
        "amended_from": null,
    });
    if let Some(children) = children {
        envelope["__children"] = children;
    }
    json!({
        "id": mutation_id,
        "type": mutation_type,
        "docType": doctype,
        "documentId": document_id,
        "payload": envelope,
        "deviceId": "dev-test",
        "userId": "user-test",
        "localTimestamp": 1751791234567i64,
        "syncVersion": null,
        "status": "pending"
    })
}

fn quotation_mutation(document_id: &str, customer: &str) -> Value {
    envelope_mutation(
        &format!("m-{document_id}"),
        "createDocument",
        "Quotation",
        document_id,
        0,
        json!({ "customer": customer, "grand_total": 200.0 }),
        Some(json!([{ "item": "Widget", "qty": 2.0, "rate": 100.0, "amount": 200.0 }])),
    )
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

async fn pull(app: &mut TestApp) -> Vec<Value> {
    let token = app.device_token("owner").await;
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", app.company_id),
            Some(&token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "pull failed: {body}");
    body["mutations"].as_array().unwrap().clone()
}

/// Creates a portal link as the owner; returns the response body.
async fn create_link(app: &TestApp, body: Value) -> Value {
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/companies/{}/portal-links", app.company_id),
            Some(&app.owner_token),
            body,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "link create failed: {body}");
    body
}

async fn customer_link(app: &TestApp, party: &str) -> String {
    let body = create_link(app, json!({ "kind": "customer", "party": party })).await;
    body["token"].as_str().unwrap().to_string()
}

/// Submits an official service Sales Invoice through the command API (the
/// replication path feeds the read model).
async fn submit_service_invoice(
    app: &mut TestApp,
    id: &str,
    customer: &str,
    rate: f64,
    date: &str,
) {
    let (status, body) = app
        .submit_as(
            "owner",
            json!({
                "doctype": "Sales Invoice",
                "document_id": id,
                "payload": { "customer": customer, "posting_date": date },
                "items": [{ "item": "SVC-1", "qty": 1, "rate": rate }]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
}

fn ids(docs: &Value) -> Vec<&str> {
    docs.as_array()
        .unwrap()
        .iter()
        .map(|doc| doc["id"].as_str().unwrap())
        .collect()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn portal_link_create_list_revoke_and_role_gate() {
    let mut app = TestApp::new().await;
    let uri = format!("/companies/{}/portal-links", app.company_id);

    // Customer links require a party.
    let (status, body) = app
        .request(
            Method::POST,
            &uri,
            Some(&app.owner_token.clone()),
            json!({ "kind": "customer" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let created = create_link(
        &app,
        json!({ "kind": "customer", "party": "CUST-1", "label": "Acme", "expires_days": 30 }),
    )
    .await;
    let token = created["token"].as_str().unwrap().to_string();
    assert_eq!(
        created["url_path"].as_str().unwrap(),
        format!("/portal/{token}")
    );
    assert!(created["expiresAt"].is_string());
    let link_id = created["id"].as_str().unwrap().to_string();

    // List exposes metadata only — never a token.
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
    assert_eq!(links[0]["kind"], json!("customer"));
    assert_eq!(links[0]["party"], json!("CUST-1"));
    assert_eq!(links[0]["label"], json!("Acme"));
    assert_eq!(links[0]["revoked"], json!(false));
    assert!(links[0].get("token").is_none());
    assert!(links[0].get("tokenHash").is_none());

    // Role gate: a sales member may not manage portal links.
    let sales_token = app.device_token("sales").await;
    let (status, _) = app
        .request(
            Method::POST,
            &uri,
            Some(&sales_token),
            json!({ "kind": "accountant" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(Method::GET, &uri, Some(&sales_token), Value::Null)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("{uri}/{link_id}"),
            Some(&sales_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // The link works before revocation…
    let (status, _, _) = raw(&app, Method::GET, &format!("/portal/{token}"), None, None).await;
    assert_eq!(status, StatusCode::OK);

    // …and 404s after (revocation is also visible in the list).
    let (status, body) = app
        .request(
            Method::DELETE,
            &format!("{uri}/{link_id}"),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revoked"], json!(true));
    let (status, _, _) = raw(&app, Method::GET, &format!("/portal/{token}"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = app
        .request(
            Method::GET,
            &uri,
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["links"][0]["revoked"], json!(true));

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
async fn portal_tokens_are_a_distinct_token_kind() {
    let mut app = TestApp::new().await;
    let created = create_link(&app, json!({ "kind": "accountant" })).await;
    let portal_token = created["token"].as_str().unwrap().to_string();

    // A portal token authenticates no member/device endpoint.
    for uri in [
        format!("/companies/{}/audit", app.company_id),
        format!("/companies/{}/sync/pull?after=0", app.company_id),
        format!("/companies/{}/portal-links", app.company_id),
    ] {
        let (status, _, _) = raw(&app, Method::GET, &uri, Some(&portal_token), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "leaked on {uri}");
    }

    // Member/device tokens never resolve on the portal plane (404, not 403).
    let device_token = app.device_token("owner").await;
    for bearer in [device_token, app.owner_token.clone()] {
        let (status, _, _) = raw(&app, Method::GET, &format!("/portal/{bearer}"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn customer_summary_is_scoped_strictly_to_its_customer() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    push(
        &mut app,
        vec![
            quotation_mutation("Q-A", "CUST-A"),
            quotation_mutation("Q-B", "CUST-B"),
        ],
    )
    .await;
    submit_service_invoice(&mut app, "SINV-A", "CUST-A", 120.0, "2026-07-06").await;
    submit_service_invoice(&mut app, "SINV-B", "CUST-B", 80.0, "2026-07-06").await;

    let token_a = customer_link(&app, "CUST-A").await;
    let token_b = customer_link(&app, "CUST-B").await;

    let (status, _, bytes) =
        raw(&app, Method::GET, &format!("/portal/{token_a}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let summary: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(summary["company"], json!("Fixture Trading Ltd"));
    assert_eq!(summary["customer"], json!("CUST-A"));
    assert_eq!(ids(&summary["quotations"]), vec!["Q-A"]);
    assert_eq!(ids(&summary["invoices"]), vec!["SINV-A"]);
    assert_eq!(
        summary["invoices"][0]["payload"]["outstanding_amount"],
        json!(120.0)
    );

    let (status, _, bytes) =
        raw(&app, Method::GET, &format!("/portal/{token_b}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let summary: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(ids(&summary["quotations"]), vec!["Q-B"]);
    assert_eq!(ids(&summary["invoices"]), vec!["SINV-B"]);

    // The other customer's document ids are 404 — never a 403 leak.
    for uri in [
        format!("/portal/{token_a}/documents/Quotation/Q-B"),
        format!("/portal/{token_a}/documents/Sales%20Invoice/SINV-B"),
    ] {
        let (status, _, _) = raw(&app, Method::GET, &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "leaked {uri}");
    }

    // Own documents resolve, with children alongside.
    let (status, _, bytes) = raw(
        &app,
        Method::GET,
        &format!("/portal/{token_a}/documents/Quotation/Q-A"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let doc: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(doc["payload"]["customer"], json!("CUST-A"));
    assert_eq!(doc["children"][0]["item"], json!("Widget"));

    // Only Quotation and Sales Invoice are exposed.
    let (status, _, _) = raw(
        &app,
        Method::GET,
        &format!("/portal/{token_a}/documents/Customer/CUST-A"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Accountant-only endpoints do not exist for a customer link.
    for uri in [
        format!("/portal/{token_a}/gl.csv"),
        format!("/portal/{token_a}/audit"),
    ] {
        let (status, _, _) = raw(&app, Method::GET, &uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "customer reached {uri}");
    }
}

#[tokio::test]
async fn quotation_accept_and_reject_write_mutations_and_are_idempotent() {
    let mut app = TestApp::new().await;
    push(
        &mut app,
        vec![
            quotation_mutation("Q-1", "CUST-A"),
            quotation_mutation("Q-2", "CUST-A"),
            quotation_mutation("Q-OTHER", "CUST-B"),
        ],
    )
    .await;
    let token = customer_link(&app, "CUST-A").await;
    let today = Utc::now().date_naive().to_string();

    // Accept writes a system mutation, visible through a normal sync pull.
    let (status, _, bytes) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-1/accept"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["accepted_on"], json!(today));

    let mutations = pull(&mut app).await;
    let accept = mutations
        .iter()
        .find(|m| m["id"] == json!("portal-Q-1-accept"))
        .expect("portal mutation missing from the log");
    assert_eq!(accept["deviceId"], json!("atlas-portal"));
    assert_eq!(accept["type"], json!("updateDocument"));
    assert_eq!(accept["docType"], json!("Quotation"));
    let count_after_first = mutations.len();

    // The read model reflects the decision.
    let doc = app
        .store
        .company_document(app.company_uuid(), "Quotation", "Q-1")
        .await
        .unwrap()
        .expect("read model row missing");
    assert_eq!(doc.payload["accepted_on"], json!(today));
    // Children survive the header-only decision update.
    assert_eq!(doc.children.unwrap()[0]["item"], json!("Widget"));

    // A second accept replays: 200, no new mutation, no second audit row.
    let (status, _, _) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-1/accept"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pull(&mut app).await.len(), count_after_first);

    // Rejecting an accepted quotation is a conflict.
    let (status, _, _) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-1/reject"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Reject works the same way (and accept-after-reject conflicts).
    let (status, _, bytes) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-2/reject"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["rejected_on"], json!(today));
    let (status, _, _) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-2/accept"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Another customer's quotation (or an unknown one) is a 404.
    for id in ["Q-OTHER", "Q-MISSING"] {
        let (status, _, _) = raw(
            &app,
            Method::POST,
            &format!("/portal/{token}/quotations/{id}/accept"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // Audit: exactly one accept row (idempotent) and one reject row.
    let (status, body) = app
        .request(
            Method::GET,
            &format!("/companies/{}/audit?limit=100", app.company_id),
            Some(&app.owner_token.clone()),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    let accepts: Vec<&Value> = entries
        .iter()
        .filter(|e| e["action"] == json!("portal.quote.accept"))
        .collect();
    assert_eq!(accepts.len(), 1);
    assert_eq!(accepts[0]["detail"]["quotation"], json!("Q-1"));
    assert_eq!(accepts[0]["detail"]["customer"], json!("CUST-A"));
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["action"] == json!("portal.quote.reject"))
            .count(),
        1
    );
}

#[tokio::test]
async fn expired_links_are_not_found() {
    let app = TestApp::new().await;

    // Inject timestamps directly through the store: one live link, one whose
    // expiry has already passed.
    let make = |expires_at| {
        let token = generate_token();
        let link = PortalLink {
            id: Uuid::new_v4(),
            company_id: app.company_uuid(),
            kind: PortalLinkKind::Customer,
            party: Some("CUST-A".into()),
            label: None,
            token_hash: hash_token(&token),
            created_by: Uuid::new_v4(),
            created_at: Utc::now() - Duration::days(91),
            expires_at,
            revoked_at: None,
        };
        (token, link)
    };
    let (live_token, live_link) = make(Utc::now() + Duration::days(1));
    let (expired_token, expired_link) = make(Utc::now() - Duration::seconds(1));
    app.store.create_portal_link(live_link).await.unwrap();
    app.store.create_portal_link(expired_link).await.unwrap();

    let (status, _, _) = raw(
        &app,
        Method::GET,
        &format!("/portal/{live_token}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for uri in [
        format!("/portal/{expired_token}"),
        format!("/portal/{expired_token}/documents/Quotation/Q-1"),
        format!("/portal/{expired_token}/quotations/Q-1/accept"),
    ] {
        let method = if uri.ends_with("/accept") {
            Method::POST
        } else {
            Method::GET
        };
        let (status, _, _) = raw(&app, method, &uri, None, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "expired token worked on {uri}"
        );
    }
}

#[tokio::test]
async fn accountant_portal_serves_summary_gl_csv_and_audit() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    // Submitted out of date order: the CSV must sort by posting date.
    submit_service_invoice(&mut app, "SINV-2", "CUST-A", 120.0, "2026-07-02").await;
    submit_service_invoice(&mut app, "SINV-1", "CUST-B", 80.0, "2026-07-01").await;

    let created = create_link(&app, json!({ "kind": "accountant", "label": "Bookkeeper" })).await;
    let token = created["token"].as_str().unwrap().to_string();

    let (status, _, bytes) = raw(&app, Method::GET, &format!("/portal/{token}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    let summary: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(summary["company"], json!("Fixture Trading Ltd"));
    assert_eq!(summary["documentCounts"]["Sales Invoice"], json!(2));
    assert_eq!(summary["glEntryCount"], json!(4));

    let (status, headers, bytes) = raw(
        &app,
        Method::GET,
        &format!("/portal/{token}/gl.csv"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/csv"));
    let csv = String::from_utf8(bytes).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "posting_date,voucher_type,voucher_no,account,debit,credit,party_type,party,is_reversal"
    );
    assert_eq!(lines.len(), 5, "expected 4 GL rows:\n{csv}");
    // Ordered by posting date then voucher: SINV-1 (07-01) before SINV-2.
    assert!(lines[1].starts_with("2026-07-01,Sales Invoice,SINV-1,"));
    assert!(lines[3].starts_with("2026-07-02,Sales Invoice,SINV-2,"));
    assert!(csv.contains("Debtors,120,0,Customer,CUST-A"));

    let (status, _, bytes) = raw(
        &app,
        Method::GET,
        &format!("/portal/{token}/audit?limit=3"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let entries = body["entries"].as_array().unwrap();
    assert!(!entries.is_empty() && entries.len() <= 3);
    assert!(entries
        .iter()
        .any(|e| e["action"] == json!("command.submit-document")));

    // Customer-only endpoints do not exist for an accountant link.
    let (status, _, _) = raw(
        &app,
        Method::GET,
        &format!("/portal/{token}/documents/Sales%20Invoice/SINV-1"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-1/accept"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn html_pages_render_with_escaped_content() {
    let mut app = TestApp::new().await;
    push(
        &mut app,
        vec![envelope_mutation(
            "m-Q-EVIL",
            "createDocument",
            "Quotation",
            "Q-EVIL",
            0,
            json!({
                "customer": "CUST-A",
                "customer_name": "<script>alert(1)</script>",
                "grand_total": 200.0
            }),
            Some(json!([{ "item": "Widget & Co", "qty": 2.0, "rate": 100.0, "amount": 200.0 }])),
        )],
    )
    .await;
    let token = customer_link(&app, "CUST-A").await;

    // Summary: HTML when asked, JSON otherwise.
    let (status, headers, bytes) = raw(
        &app,
        Method::GET,
        &format!("/portal/{token}"),
        None,
        Some("text/html"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let html = String::from_utf8(bytes).unwrap();
    assert!(html.contains("Fixture Trading Ltd"));
    assert!(html.contains(&format!("/portal/{token}/documents/Quotation/Q-EVIL")));

    let (status, _, bytes) = raw(&app, Method::GET, &format!("/portal/{token}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        serde_json::from_slice::<Value>(&bytes).is_ok(),
        "JSON stays the default"
    );

    // Document page: payload is user data — everything escaped.
    let doc_uri = format!("/portal/{token}/documents/Quotation/Q-EVIL");
    let (status, _, bytes) = raw(&app, Method::GET, &doc_uri, None, Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(bytes).unwrap();
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "expected escaped script tag:\n{html}"
    );
    assert!(
        !html.contains("<script>"),
        "unescaped payload leaked:\n{html}"
    );
    assert!(
        html.contains("Widget &amp; Co"),
        "line items missing:\n{html}"
    );
    assert!(html.contains("Accept quotation"));
    assert!(html.contains("Reject quotation"));

    // HTML form accept: redirect back to the document page…
    let (status, headers, _) = raw(
        &app,
        Method::POST,
        &format!("/portal/{token}/quotations/Q-EVIL/accept"),
        None,
        Some("text/html"),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        headers.get(header::LOCATION).unwrap().to_str().unwrap(),
        format!("/portal/{token}/documents/Quotation/Q-EVIL")
    );

    // …which now shows the accepted state and no more decision forms.
    let (status, _, bytes) = raw(&app, Method::GET, &doc_uri, None, Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(bytes).unwrap();
    assert!(
        html.contains("Accepted on"),
        "accepted state missing:\n{html}"
    );
    assert!(!html.contains("Accept quotation"));
}

#[tokio::test]
async fn projection_follows_the_log_and_rebuild_is_equivalent() {
    let mut app = TestApp::new().await;
    app.upsert_item(json!({ "id": "SVC-1", "item_type": "Service" }))
        .await;
    let company = app.company_uuid();

    // Envelope create with children.
    push(
        &mut app,
        vec![envelope_mutation(
            "m-c9-1",
            "createDocument",
            "Customer",
            "CUST-9",
            0,
            json!({ "name": "Nine Ltd", "credit_limit": 1000 }),
            Some(json!([{ "contact": "Nina" }])),
        )],
    )
    .await;
    let doc = app
        .store
        .company_document(company, "Customer", "CUST-9")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.payload["name"], json!("Nine Ltd"));
    assert_eq!(doc.docstatus, 0);
    assert_eq!(doc.children.as_ref().unwrap()[0]["contact"], json!("Nina"));

    // Envelope update without __children replaces the payload, keeps children.
    push(
        &mut app,
        vec![envelope_mutation(
            "m-c9-2",
            "updateDocument",
            "Customer",
            "CUST-9",
            0,
            json!({ "name": "Nine Updated Ltd" }),
            None,
        )],
    )
    .await;
    let doc = app
        .store
        .company_document(company, "Customer", "CUST-9")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.payload["name"], json!("Nine Updated Ltd"));
    assert!(doc.payload.get("credit_limit").is_none());
    assert_eq!(doc.children.as_ref().unwrap()[0]["contact"], json!("Nina"));

    // A plain (non-envelope) payload folds as the fields directly.
    push(
        &mut app,
        vec![json!({
            "id": "m-c10",
            "type": "createDocument",
            "docType": "Customer",
            "documentId": "CUST-10",
            "payload": { "name": "Ten Ltd" },
            "deviceId": "dev-test",
            "userId": "user-test",
            "localTimestamp": 1751791234567i64,
            "syncVersion": null,
            "status": "pending"
        })],
    )
    .await;
    let doc = app
        .store
        .company_document(company, "Customer", "CUST-10")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.payload["name"], json!("Ten Ltd"));

    // deleteDocument removes the row.
    push(
        &mut app,
        vec![json!({
            "id": "m-c10-del",
            "type": "deleteDocument",
            "docType": "Customer",
            "documentId": "CUST-10",
            "payload": {},
            "deviceId": "dev-test",
            "userId": "user-test",
            "localTimestamp": 1751791234567i64,
            "syncVersion": null,
            "status": "pending"
        })],
    )
    .await;
    assert!(app
        .store
        .company_document(company, "Customer", "CUST-10")
        .await
        .unwrap()
        .is_none());

    // The replication path (system mutations inside posting_commit) feeds the
    // projection too: an official submit lands with docstatus 1.
    submit_service_invoice(&mut app, "SINV-P", "CUST-9", 50.0, "2026-07-06").await;
    let doc = app
        .store
        .company_document(company, "Sales Invoice", "SINV-P")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(doc.docstatus, 1);
    assert_eq!(doc.payload["outstanding_amount"], json!(50.0));
    assert!(doc.payload["official_number"].is_string());

    // Rebuild from scratch reproduces the incrementally maintained state.
    let snapshot = |docs: Vec<atlas_team_backend::projection::CompanyDocument>| {
        docs.into_iter()
            .map(|d| (d.doctype, d.document_id, d.docstatus, d.payload, d.children))
            .collect::<Vec<_>>()
    };
    let mut before = Vec::new();
    for doctype in ["Customer", "Quotation", "Sales Invoice", "GL Entry"] {
        before.extend(snapshot(
            app.store.company_documents(company, doctype).await.unwrap(),
        ));
    }
    assert!(!before.is_empty());
    app.store.rebuild_projection(company).await.unwrap();
    let mut after = Vec::new();
    for doctype in ["Customer", "Quotation", "Sales Invoice", "GL Entry"] {
        after.extend(snapshot(
            app.store.company_documents(company, doctype).await.unwrap(),
        ));
    }
    before.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    after.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    assert_eq!(before, after);
}
