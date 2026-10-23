//! Tenant isolation, proven negatively: a user or device belonging only to
//! Company B must not be able to read, mutate, enumerate or infer Company
//! A's protected data (commercialisation plan §M4.5 hard release invariant).
//!
//! Every company-scoped route of the router is attacked with Company B's
//! credentials — its owner's user token and a device token — both against
//! Company A's path (direct attack) and through Company B's own path with
//! Company A's resource ids (confused deputy). Afterwards Company A's log,
//! ledger and audit are checked for any trace of the attempts.
//!
//! Runs over MemStore by default and over PgStore with
//! `ATLAS_TEST_DATABASE_URL` (CI runs both).

mod support;

use std::sync::Arc;

use atlas_team_backend::store::Store;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct App {
    router: Router,
    store: Arc<dyn Store>,
    _db: Option<support::PgTestDb>,
}

struct Tenant {
    company_id: String,
    owner_token: String,
    owner_user_id: String,
    device_id: String,
    device_token: String,
}

impl App {
    async fn new() -> Self {
        let (store, db) = support::test_store().await;
        let router = atlas_team_backend::router_with(store.clone(), support::test_config());
        Self {
            router,
            store,
            _db: db,
        }
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Body,
        content_type: &str,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn json(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let (status, bytes) = self
            .send(
                method,
                uri,
                token,
                Body::from(body.to_string()),
                "application/json",
            )
            .await;
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn tenant(&self, name: &str, email: &str) -> Tenant {
        let (status, body) = self
            .json(
                Method::POST,
                "/companies",
                None,
                json!({ "name": name, "owner_email": email, "owner_name": name }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "bootstrap failed: {body}");
        let company_id = body["company"]["id"].as_str().unwrap().to_string();
        let owner_token = body["token"].as_str().unwrap().to_string();
        let owner_user_id = body["userId"].as_str().unwrap().to_string();
        let (status, body) = self
            .json(
                Method::POST,
                &format!("/companies/{company_id}/devices"),
                Some(&owner_token),
                json!({ "name": format!("{name} till") }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "device failed: {body}");
        Tenant {
            company_id,
            owner_token,
            owner_user_id,
            device_id: body["deviceId"].as_str().unwrap().to_string(),
            device_token: body["deviceToken"].as_str().unwrap().to_string(),
        }
    }
}

fn customer_mutation(id: &str, device_id: &str) -> Value {
    json!({
        "id": id,
        "type": "createDocument",
        "docType": "Customer",
        "documentId": id,
        "payload": { "id": id, "doctype": "Customer", "docstatus": 0,
                     "payload": "{\"customer_name\":\"Secret Customer\"}" },
        "deviceId": device_id,
        "userId": "u",
        "localTimestamp": 1i64,
        "syncVersion": null,
        "status": "pending"
    })
}

/// Company A with some protected state: a synced customer, a blob, an
/// official sale, a portal link and a pay link.
struct Seeded {
    a: Tenant,
    b: Tenant,
    blob_sha: String,
    portal_link_id: String,
    pay_link_id: String,
}

async fn seeded(app: &App) -> Seeded {
    let a = app.tenant("Alpha Ltd", "alpha@example.com").await;
    let b = app.tenant("Bravo Ltd", "bravo@example.com").await;

    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/push", a.company_id),
            Some(&a.device_token),
            json!({ "mutations": [customer_mutation("CUST-A-SECRET", "dev-a")] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let bytes = b"alpha confidential attachment".to_vec();
    let blob_sha = hex::encode(Sha256::digest(&bytes));
    let (status, _) = app
        .send(
            Method::PUT,
            &format!("/companies/{}/blobs/{blob_sha}", a.company_id),
            Some(&a.device_token),
            Body::from(bytes),
            "application/octet-stream",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/commands/submit-document", a.company_id),
            Some(&a.device_token),
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-A-1",
                "payload": { "customer": "CUST-A-SECRET", "posting_date": "2026-03-01",
                             "receivable_account": "Debtors", "income_account": "Sales" },
                "items": [{ "item": "SVC", "qty": 1, "rate": 100, "amount": 100 }],
                "idempotency_key": "alpha-sinv-1"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "seed invoice failed: {body}");

    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/portal-links", a.company_id),
            Some(&a.owner_token),
            json!({ "kind": "accountant", "label": "Alpha accountant" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "portal link failed: {body}");
    let (status, links) = app
        .json(
            Method::GET,
            &format!("/companies/{}/portal-links", a.company_id),
            Some(&a.owner_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let portal_link_id = links["links"][0]["id"].as_str().unwrap().to_string();

    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/pay-links", a.company_id),
            Some(&a.owner_token),
            json!({ "invoice_id": "SINV-A-1" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "pay link failed: {body}");
    let (status, links) = app
        .json(
            Method::GET,
            &format!("/companies/{}/pay-links", a.company_id),
            Some(&a.owner_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let pay_link_id = links["links"][0]["id"].as_str().unwrap().to_string();

    Seeded {
        a,
        b,
        blob_sha,
        portal_link_id,
        pay_link_id,
    }
}

/// Every company-scoped route, with a body that would succeed for a member.
fn attack_surface(company: &str, s: &Seeded) -> Vec<(Method, String, Value)> {
    let a = &s.a;
    vec![
        (
            Method::POST,
            format!("/companies/{company}/invitations"),
            json!({ "email": "x@example.com", "role": "admin" }),
        ),
        (
            Method::POST,
            format!("/companies/{company}/devices"),
            json!({ "name": "intruder" }),
        ),
        (
            Method::GET,
            format!("/companies/{company}/devices"),
            Value::Null,
        ),
        (
            Method::POST,
            format!("/companies/{company}/devices/{}/revoke", a.device_id),
            Value::Null,
        ),
        (
            Method::POST,
            format!("/companies/{company}/tokens/rotate"),
            Value::Null,
        ),
        (
            Method::GET,
            format!("/companies/{company}/members"),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("/companies/{company}/members/{}", a.owner_user_id),
            Value::Null,
        ),
        (
            Method::POST,
            format!("/companies/{company}/members/{}/role", a.owner_user_id),
            json!({ "role": "advisor" }),
        ),
        (
            Method::POST,
            format!("/companies/{company}/sync/push"),
            json!({ "mutations": [customer_mutation("CUST-INJECTED", "dev-b")] }),
        ),
        (
            Method::GET,
            format!("/companies/{company}/sync/pull?after=0"),
            Value::Null,
        ),
        (
            Method::POST,
            format!("/companies/{company}/sync/ack"),
            json!({ "ids": ["CUST-A-SECRET"] }),
        ),
        (
            Method::GET,
            format!("/companies/{company}/blobs/{}", s.blob_sha),
            Value::Null,
        ),
        (
            Method::HEAD,
            format!("/companies/{company}/blobs/{}", s.blob_sha),
            Value::Null,
        ),
        (
            Method::GET,
            format!("/companies/{company}/audit"),
            Value::Null,
        ),
        (
            Method::GET,
            format!("/companies/{company}/settings"),
            Value::Null,
        ),
        (
            Method::PUT,
            format!("/companies/{company}/settings"),
            json!({ "allow_negative_stock": true }),
        ),
        (
            Method::POST,
            format!("/companies/{company}/items"),
            json!({ "id": "WIDGET", "item_type": "Stock" }),
        ),
        (
            Method::POST,
            format!("/companies/{company}/commands/submit-document"),
            json!({
            "doctype": "Sales Invoice", "document_id": "SINV-INJECTED",
            "payload": { "customer": "X", "receivable_account": "Debtors", "income_account": "Sales" },
            "items": [{ "item": "SVC", "qty": 1, "rate": 1, "amount": 1 }] }),
        ),
        (
            Method::POST,
            format!("/companies/{company}/commands/cancel-document"),
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-A-1" }),
        ),
        (
            Method::POST,
            format!("/companies/{company}/portal-links"),
            json!({ "kind": "accountant" }),
        ),
        (
            Method::GET,
            format!("/companies/{company}/portal-links"),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("/companies/{company}/portal-links/{}", s.portal_link_id),
            Value::Null,
        ),
        (
            Method::POST,
            format!("/companies/{company}/pay-links"),
            json!({ "invoice_id": "SINV-A-1" }),
        ),
        (
            Method::GET,
            format!("/companies/{company}/pay-links"),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("/companies/{company}/pay-links/{}", s.pay_link_id),
            Value::Null,
        ),
    ]
}

/// Company A's protected state is exactly what the seed put there.
async fn assert_company_a_untouched(app: &App, s: &Seeded) {
    let a = &s.a;
    // Sync log: only A's own customer + the backend's posting replication.
    let (status, page) = app
        .json(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", a.company_id),
            Some(&a.device_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mutations = page["mutations"].as_array().unwrap();
    assert!(mutations
        .iter()
        .all(|m| m["documentId"] != "CUST-INJECTED" && m["deviceId"] != "dev-b"));
    // The official sale is still submitted, nothing else was posted.
    let doc = app
        .store
        .posted_document(a.company_id.parse().unwrap(), "Sales Invoice", "SINV-A-1")
        .await
        .unwrap()
        .expect("seeded invoice");
    assert_eq!(doc.docstatus, 1);
    assert!(app
        .store
        .posted_document(
            a.company_id.parse().unwrap(),
            "Sales Invoice",
            "SINV-INJECTED"
        )
        .await
        .unwrap()
        .is_none());
    // Membership, devices, settings and links are unchanged.
    let (_, members) = app
        .json(
            Method::GET,
            &format!("/companies/{}/members", a.company_id),
            Some(&a.owner_token),
            Value::Null,
        )
        .await;
    assert_eq!(members["members"].as_array().unwrap().len(), 1);
    assert_eq!(members["members"][0]["role"], "owner");
    let (_, devices) = app
        .json(
            Method::GET,
            &format!("/companies/{}/devices", a.company_id),
            Some(&a.owner_token),
            Value::Null,
        )
        .await;
    let devices = devices["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert!(devices[0]["revokedAt"].is_null());
    let (_, settings) = app
        .json(
            Method::GET,
            &format!("/companies/{}/settings", a.company_id),
            Some(&a.owner_token),
            Value::Null,
        )
        .await;
    assert_ne!(settings["allow_negative_stock"], json!(true));
    for kind in ["portal-links", "pay-links"] {
        let (_, links) = app
            .json(
                Method::GET,
                &format!("/companies/{}/{kind}", a.company_id),
                Some(&a.owner_token),
                Value::Null,
            )
            .await;
        let links = links["links"].as_array().unwrap();
        assert_eq!(links.len(), 1, "{kind}: {links:?}");
        assert!(
            links[0]["revokedAt"].is_null() && links[0]["revoked"] != json!(true),
            "{kind} revoked: {links:?}"
        );
    }
    // A's audit trail carries no action by B's user.
    let (_, audit) = app
        .json(
            Method::GET,
            &format!("/companies/{}/audit?limit=500", a.company_id),
            Some(&a.owner_token),
            Value::Null,
        )
        .await;
    let rows = audit["entries"]
        .as_array()
        .or_else(|| audit["audit"].as_array())
        .or_else(|| audit.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(!rows.is_empty(), "audit feed shape: {audit}");
    assert!(rows
        .iter()
        .all(|row| row["userId"] != json!(s.b.owner_user_id)));
}

#[tokio::test]
async fn company_b_credentials_cannot_touch_company_a_routes() {
    let app = App::new().await;
    let s = seeded(&app).await;

    for token in [&s.b.owner_token, &s.b.device_token] {
        for (method, uri, body) in attack_surface(&s.a.company_id, &s) {
            let (status, bytes) = app
                .send(
                    method.clone(),
                    &uri,
                    Some(token),
                    Body::from(if body.is_null() {
                        String::new()
                    } else {
                        body.to_string()
                    }),
                    "application/json",
                )
                .await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{method} {uri} with a Company B credential: {}",
                String::from_utf8_lossy(&bytes)
            );
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains("Secret Customer") && !text.contains("SINV-A-1"),
                "{method} {uri} leaked Company A data: {text}"
            );
        }
    }
    assert_company_a_untouched(&app, &s).await;
}

#[tokio::test]
async fn company_a_ids_through_company_b_paths_never_resolve() {
    let app = App::new().await;
    let s = seeded(&app).await;
    let b = &s.b;

    // Confused deputy: B acting legitimately on its OWN company path, but
    // naming Company A's device / member / link / blob / document ids.
    let probes = [
        (
            Method::POST,
            format!(
                "/companies/{}/devices/{}/revoke",
                b.company_id, s.a.device_id
            ),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("/companies/{}/members/{}", b.company_id, s.a.owner_user_id),
            Value::Null,
        ),
        (
            Method::POST,
            format!(
                "/companies/{}/members/{}/role",
                b.company_id, s.a.owner_user_id
            ),
            json!({ "role": "advisor" }),
        ),
        (
            Method::DELETE,
            format!(
                "/companies/{}/portal-links/{}",
                b.company_id, s.portal_link_id
            ),
            Value::Null,
        ),
        (
            Method::DELETE,
            format!("/companies/{}/pay-links/{}", b.company_id, s.pay_link_id),
            Value::Null,
        ),
        (
            Method::GET,
            format!("/companies/{}/blobs/{}", b.company_id, s.blob_sha),
            Value::Null,
        ),
        (
            Method::HEAD,
            format!("/companies/{}/blobs/{}", b.company_id, s.blob_sha),
            Value::Null,
        ),
        (
            Method::POST,
            format!("/companies/{}/pay-links", b.company_id),
            json!({ "invoice_id": "SINV-A-1" }),
        ),
    ];
    for (method, uri, body) in probes {
        let token = if uri.contains("/blobs/") {
            &b.device_token
        } else {
            &b.owner_token
        };
        let (status, bytes) = app
            .send(
                method.clone(),
                &uri,
                Some(token),
                Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }),
                "application/json",
            )
            .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {uri}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    // Cancelling A's document id inside B's company finds nothing.
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/commands/cancel-document", b.company_id),
            Some(&b.device_token),
            json!({ "doctype": "Sales Invoice", "document_id": "SINV-A-1" }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Reusing A's idempotency key in B neither replays A's response nor
    // collides with it: keys are per company.
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/commands/submit-document", b.company_id),
            Some(&b.device_token),
            json!({
                "doctype": "Sales Invoice",
                "document_id": "SINV-B-1",
                "payload": { "customer": "CUST-B", "posting_date": "2026-03-01",
                             "receivable_account": "Debtors", "income_account": "Sales" },
                "items": [{ "item": "SVC", "qty": 1, "rate": 5, "amount": 5 }],
                "idempotency_key": "alpha-sinv-1"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["document_id"], "SINV-B-1");
    // Official numbering is per company: B's first invoice is number 1 too.
    assert_eq!(body["number"], "SINV-00001");

    // B's pull never contains A's rows.
    let (_, page) = app
        .json(
            Method::GET,
            &format!("/companies/{}/sync/pull?after=0", b.company_id),
            Some(&b.device_token),
            Value::Null,
        )
        .await;
    let text = page.to_string();
    assert!(!text.contains("CUST-A-SECRET") && !text.contains("SINV-A-1"));

    assert_company_a_untouched(&app, &s).await;
}

#[tokio::test]
async fn a_device_token_is_bound_to_the_company_it_was_registered_in() {
    let app = App::new().await;
    let a = app.tenant("Alpha Ltd", "alpha@example.com").await;
    let b = app.tenant("Bravo Ltd", "bravo@example.com").await;

    // Alpha's owner is also invited into Bravo (a bookkeeper serving both).
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/invitations", b.company_id),
            Some(&b.owner_token),
            json!({ "email": "alpha@example.com", "role": "accountant" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let invitation = body["token"].as_str().unwrap().to_string();
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/invitations/{invitation}/accept"),
            None,
            json!({ "display_name": "Alpha Owner" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    // The Alpha till's device token must not open Bravo's books, even
    // though its user is now a Bravo member.
    for (method, uri) in [
        (
            Method::GET,
            format!("/companies/{}/sync/pull", b.company_id),
        ),
        (
            Method::POST,
            format!("/companies/{}/sync/push", b.company_id),
        ),
        (Method::GET, format!("/companies/{}/members", b.company_id)),
    ] {
        let body = if method == Method::POST {
            json!({ "mutations": [] })
        } else {
            Value::Null
        };
        let (status, _) = app
            .json(method.clone(), &uri, Some(&a.device_token), body)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // …while it still works for Alpha.
    let (status, _) = app
        .json(
            Method::GET,
            &format!("/companies/{}/sync/pull", a.company_id),
            Some(&a.device_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn revocation_and_role_changes_take_effect_on_the_next_request() {
    let mut app = support::TestApp::new().await;
    let sales = app.device_token("sales").await;
    let company = app.company_id.clone();

    let invoice = |id: &str| {
        json!({
            "doctype": "Sales Invoice",
            "document_id": id,
            "payload": { "customer": "C1", "posting_date": "2026-03-01",
                         "receivable_account": "Debtors", "income_account": "Sales" },
            "items": [{ "item": "SVC", "qty": 1, "rate": 10, "amount": 10 }]
        })
    };
    let submit = |token: String, body: Value| {
        let app = &app;
        let company = company.clone();
        async move {
            app.request(
                Method::POST,
                &format!("/companies/{company}/commands/submit-document"),
                Some(&token),
                body,
            )
            .await
        }
    };

    let (status, _) = submit(sales.clone(), invoice("SINV-R1")).await;
    assert_eq!(status, StatusCode::OK);

    // Demote the sales user to read-only advisor: their next submit is 403.
    let (_, members) = app
        .request(
            Method::GET,
            &format!("/companies/{company}/members"),
            Some(&app.owner_token),
            Value::Null,
        )
        .await;
    let sales_user = members["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "sales")
        .unwrap()["userId"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/companies/{company}/members/{sales_user}/role"),
            Some(&app.owner_token),
            json!({ "role": "advisor" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = submit(sales.clone(), invoice("SINV-R2")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Remove the member: the device token stops authenticating at all.
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/companies/{company}/members/{sales_user}"),
            Some(&app.owner_token),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = submit(sales.clone(), invoice("SINV-R3")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/companies/{company}/sync/pull"),
            Some(&sales),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
