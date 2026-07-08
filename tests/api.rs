//! Integration tests: the full router over `MemStore`, driven with
//! `tower::ServiceExt::oneshot`. No database, no network.

use std::sync::Arc;

use atlas_team_backend::store::MemStore;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct App {
    router: Router,
    store: Arc<MemStore>,
}

impl App {
    fn new() -> Self {
        let store = Arc::new(MemStore::new());
        let router = atlas_team_backend::router(store.clone());
        Self { router, store }
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Body>,
        content_type: Option<&str>,
    ) -> (StatusCode, Vec<u8>) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let request = builder.body(body.unwrap_or_else(Body::empty)).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
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
                Some(Body::from(body.to_string())),
                Some("application/json"),
            )
            .await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    async fn get(&self, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let (status, bytes) = self.send(Method::GET, uri, token, None, None).await;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }
}

struct Boot {
    company_id: String,
    owner_token: String,
    owner_user_id: String,
}

async fn bootstrap(app: &App) -> Boot {
    let (status, body) = app
        .json(
            Method::POST,
            "/companies",
            None,
            json!({
                "name": "Busuttil Trading Ltd",
                "owner_email": "owner@example.com",
                "owner_name": "Olivia Owner"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "bootstrap failed: {body}");
    Boot {
        company_id: body["company"]["id"].as_str().unwrap().to_string(),
        owner_token: body["token"].as_str().unwrap().to_string(),
        owner_user_id: body["userId"].as_str().unwrap().to_string(),
    }
}

/// Invite `email` with `role` (as owner) and accept the invitation.
/// Returns (user_id, user_token).
async fn join_member(app: &App, boot: &Boot, email: &str, role: &str) -> (String, String) {
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/invitations", boot.company_id),
            Some(&boot.owner_token),
            json!({ "email": email, "role": role }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "invite failed: {body}");
    let invitation_token = body["token"].as_str().unwrap().to_string();
    assert!(body["expiresAt"].is_string());

    let (status, body) = app
        .json(
            Method::POST,
            &format!("/invitations/{invitation_token}/accept"),
            None,
            json!({ "display_name": "Sam Second" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "accept failed: {body}");
    assert_eq!(body["companyId"].as_str().unwrap(), boot.company_id);
    assert_eq!(body["role"].as_str().unwrap(), role);
    (
        body["userId"].as_str().unwrap().to_string(),
        body["token"].as_str().unwrap().to_string(),
    )
}

/// Returns (device_id, device_token).
async fn register_device(
    app: &App,
    company_id: &str,
    user_token: &str,
    name: &str,
) -> (String, String) {
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{company_id}/devices"),
            Some(user_token),
            json!({ "name": name }),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "device registration failed: {body}"
    );
    (
        body["deviceId"].as_str().unwrap().to_string(),
        body["deviceToken"].as_str().unwrap().to_string(),
    )
}

/// A wire-format mutation exactly as the Dart client's `MutationRecord`
/// serializes (camelCase keys, Dart enum names, millisecond timestamp).
fn mutation(id: &str, document_id: &str, device_id: &str, user_id: &str) -> Value {
    json!({
        "id": id,
        "type": "updateDocument",
        "docType": "Customer",
        "documentId": document_id,
        "payload": { "name": "ACME Ltd", "creditLimit": 5000 },
        "deviceId": device_id,
        "userId": user_id,
        "localTimestamp": 1751791234567i64,
        "syncVersion": null,
        "status": "pending"
    })
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_needs_no_auth() {
    let app = App::new();
    let (status, body) = app.get("/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "status": "ok" }));
}

#[tokio::test]
async fn bootstrap_creates_company_owner_and_token() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    assert!(!boot.company_id.is_empty());
    assert!(!boot.owner_user_id.is_empty());
    // The bootstrap token authenticates company-scoped routes.
    let (status, body) = app
        .get(
            &format!("/companies/{}/audit", boot.company_id),
            Some(&boot.owner_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "audit read failed: {body}");
}

#[tokio::test]
async fn invitation_flow_lets_second_user_join_and_register_devices() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (second_user_id, second_token) =
        join_member(&app, &boot, "second@example.com", "sales").await;
    assert_ne!(second_user_id, boot.owner_user_id);

    // Both users register a device (roadmap criterion 3).
    let (device_a, _) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Front desk").await;
    let (device_b, _) = register_device(&app, &boot.company_id, &second_token, "Sam's phone").await;
    assert_ne!(device_a, device_b);
}

#[tokio::test]
async fn invitation_cannot_be_accepted_twice() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/invitations", boot.company_id),
            Some(&boot.owner_token),
            json!({ "email": "once@example.com", "role": "stock" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let invitation_token = body["token"].as_str().unwrap().to_string();

    let accept = json!({ "display_name": "Once Only" });
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/invitations/{invitation_token}/accept"),
            None,
            accept.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/invitations/{invitation_token}/accept"),
            None,
            accept,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn push_assigns_monotonic_versions_and_is_idempotent() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_a, token_a) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Device A").await;

    let mutations = json!({ "mutations": [
        mutation("m-1", "CUST-001", &device_a, &boot.owner_user_id),
        mutation("m-2", "CUST-002", &device_a, &boot.owner_user_id),
        mutation("m-3", "ITEM-001", &device_a, &boot.owner_user_id),
    ]});
    let uri = format!("/companies/{}/sync/push", boot.company_id);
    let (status, body) = app
        .json(Method::POST, &uri, Some(&token_a), mutations.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "push failed: {body}");
    assert_eq!(body["versions"]["m-1"], json!(1));
    assert_eq!(body["versions"]["m-2"], json!(2));
    assert_eq!(body["versions"]["m-3"], json!(3));

    // Re-pushing the same ids returns the originally assigned versions and
    // does not grow the log (idempotency for flaky-connection retries).
    let (status, body) = app
        .json(Method::POST, &uri, Some(&token_a), mutations)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["versions"]["m-1"], json!(1));
    assert_eq!(body["versions"]["m-2"], json!(2));
    assert_eq!(body["versions"]["m-3"], json!(3));

    let (status, body) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&token_a),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mutations"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn pull_returns_camelcase_records_in_version_order() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_a, token_a) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Device A").await;
    let (second_user_id, second_token) =
        join_member(&app, &boot, "second@example.com", "sales").await;
    let (_device_b, token_b) =
        register_device(&app, &boot.company_id, &second_token, "Device B").await;

    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/push", boot.company_id),
            Some(&token_a),
            json!({ "mutations": [
                mutation("m-1", "CUST-001", &device_a, &boot.owner_user_id),
                mutation("m-2", "CUST-002", &device_a, &boot.owner_user_id),
                mutation("m-3", "ITEM-001", &device_a, &boot.owner_user_id),
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Device B (second user) sees everything from version 0, in order, in the
    // exact Dart wire format (roadmap criterion 4).
    let (status, body) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&token_b),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let pulled = body["mutations"].as_array().unwrap();
    assert_eq!(pulled.len(), 3);
    assert_eq!(
        pulled
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["m-1", "m-2", "m-3"]
    );
    let first = &pulled[0];
    assert_eq!(first["type"], json!("updateDocument"));
    assert_eq!(first["docType"], json!("Customer"));
    assert_eq!(first["documentId"], json!("CUST-001"));
    assert_eq!(first["payload"]["name"], json!("ACME Ltd"));
    assert_eq!(first["deviceId"], json!(device_a));
    assert_eq!(first["userId"], json!(boot.owner_user_id));
    assert_eq!(first["localTimestamp"], json!(1751791234567i64));
    assert_eq!(first["syncVersion"], json!("1"));
    assert_eq!(first["status"], json!("pending"));
    assert_eq!(pulled[2]["syncVersion"], json!("3"));

    // Incremental pull: after=2 sees only the third mutation.
    let (status, body) = app
        .get(
            &format!("/companies/{}/sync/pull?after=2", boot.company_id),
            Some(&token_b),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let pulled = body["mutations"].as_array().unwrap();
    assert_eq!(pulled.len(), 1);
    assert_eq!(pulled[0]["id"], json!("m-3"));
    assert_eq!(pulled[0]["syncVersion"], json!("3"));
    let _ = second_user_id;
}

#[tokio::test]
async fn ack_marks_mutations_acknowledged() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_a, token_a) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Device A").await;
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/push", boot.company_id),
            Some(&token_a),
            json!({ "mutations": [
                mutation("m-1", "CUST-001", &device_a, &boot.owner_user_id),
                mutation("m-2", "CUST-002", &device_a, &boot.owner_user_id),
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let uri = format!("/companies/{}/sync/ack", boot.company_id);
    let (status, body) = app
        .json(
            Method::POST,
            &uri,
            Some(&token_a),
            json!({ "ids": ["m-1", "m-2"] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["acknowledged"], json!(2));

    // Acking again matches nothing new.
    let (status, body) = app
        .json(
            Method::POST,
            &uri,
            Some(&token_a),
            json!({ "ids": ["m-1", "m-2"] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["acknowledged"], json!(0));
}

#[tokio::test]
async fn sync_requires_a_device_token() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    // A user token is authenticated but not a device — sync is device-only.
    let (status, _) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&boot.owner_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn blob_put_get_head_roundtrip_and_hash_check() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let bytes = b"scanned-receipt-bytes".to_vec();
    let sha = hex::encode(Sha256::digest(&bytes));
    let uri = format!("/companies/{}/blobs/{sha}", boot.company_id);

    // HEAD before upload → 404.
    let (status, _) = app
        .send(Method::HEAD, &uri, Some(&boot.owner_token), None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = app
        .send(
            Method::PUT,
            &uri,
            Some(&boot.owner_token),
            Some(Body::from(bytes.clone())),
            Some("application/octet-stream"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, returned) = app
        .send(Method::GET, &uri, Some(&boot.owner_token), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(returned, bytes);

    let (status, _) = app
        .send(Method::HEAD, &uri, Some(&boot.owner_token), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // Wrong hash in the path → 422, nothing stored.
    let wrong = hex::encode(Sha256::digest(b"different"));
    let wrong_uri = format!("/companies/{}/blobs/{wrong}", boot.company_id);
    let (status, _) = app
        .send(
            Method::PUT,
            &wrong_uri,
            Some(&boot.owner_token),
            Some(Body::from(bytes.clone())),
            Some("application/octet-stream"),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = app
        .send(Method::GET, &wrong_uri, Some(&boot.owner_token), None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webhooks_are_logged_without_auth() {
    // The generic intake routes stay log-only; `/webhooks/payments/stripe`
    // is now the signature-verified processing endpoint (tests/payments.rs).
    let app = App::new();
    let payload = json!({ "type": "payment_intent.succeeded", "id": "pi_123" });
    let (status, body) = app
        .json(
            Method::POST,
            "/webhooks/payments/paypal",
            None,
            payload.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "logged": true }));

    let (status, _) = app
        .json(
            Method::POST,
            "/webhooks/channels/shopify",
            None,
            json!({ "order": 42 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let events = app.store.webhook_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind.as_str(), "payment");
    assert_eq!(events[0].provider, "paypal");
    let stored: Value = serde_json::from_slice(&events[0].body).unwrap();
    assert_eq!(stored, payload);
    assert_eq!(events[0].headers["content-type"], json!("application/json"));
    assert_eq!(events[1].kind.as_str(), "channel");
    assert_eq!(events[1].provider, "shopify");
}

#[tokio::test]
async fn company_routes_reject_missing_or_bad_tokens_with_401() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let uri = format!("/companies/{}/devices", boot.company_id);

    let (status, _) = app
        .json(Method::POST, &uri, None, json!({ "name": "No token" }))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = app
        .json(
            Method::POST,
            &uri,
            Some("not-a-real-token"),
            json!({ "name": "Bad token" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn non_members_get_403() {
    let app = App::new();
    let boot_a = bootstrap(&app).await;
    // A second, unrelated company; its owner is not a member of company A.
    let (status, body) = app
        .json(
            Method::POST,
            "/companies",
            None,
            json!({
                "name": "Rival Co",
                "owner_email": "rival@example.com",
                "owner_name": "Rita Rival"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let rival_token = body["token"].as_str().unwrap().to_string();

    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/devices", boot_a.company_id),
            Some(&rival_token),
            json!({ "name": "Intruder" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn only_owner_or_admin_can_invite() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (_user_id, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/invitations", boot.company_id),
            Some(&sales_token),
            json!({ "email": "friend@example.com", "role": "admin" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn audit_feed_is_role_restricted() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (_user_id, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;
    let uri = format!("/companies/{}/audit", boot.company_id);

    let (status, _) = app.get(&uri, Some(&sales_token)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = app.get(&uri, Some(&boot.owner_token)).await;
    assert_eq!(status, StatusCode::OK, "owner audit read failed: {body}");
}

#[tokio::test]
async fn every_mutating_action_writes_an_audit_row() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (_second_user_id, second_token) =
        join_member(&app, &boot, "second@example.com", "accountant").await;
    let (device_id, device_token) =
        register_device(&app, &boot.company_id, &second_token, "Device B").await;

    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/push", boot.company_id),
            Some(&device_token),
            json!({ "mutations": [
                mutation("m-1", "CUST-001", &device_id, &boot.owner_user_id),
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/ack", boot.company_id),
            Some(&device_token),
            json!({ "ids": ["m-1"] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let bytes = b"attachment".to_vec();
    let sha = hex::encode(Sha256::digest(&bytes));
    let (status, _) = app
        .send(
            Method::PUT,
            &format!("/companies/{}/blobs/{sha}", boot.company_id),
            Some(&boot.owner_token),
            Some(Body::from(bytes)),
            Some("application/octet-stream"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = app
        .get(
            &format!("/companies/{}/audit?limit=50", boot.company_id),
            Some(&boot.owner_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();
    let actions: Vec<&str> = entries
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    for expected in [
        "company.create",
        "invitation.create",
        "invitation.accept",
        "device.register",
        "sync.push",
        "sync.ack",
        "blob.put",
    ] {
        assert!(
            actions.contains(&expected),
            "missing audit action {expected}; got {actions:?}"
        );
    }
    // Every row carries company + user + action + timestamp; device-scoped
    // actions carry the device too (roadmap criterion 8).
    for entry in entries {
        assert_eq!(entry["companyId"].as_str().unwrap(), boot.company_id);
        assert!(
            entry["userId"].is_string(),
            "audit row without user: {entry}"
        );
        assert!(entry["at"].is_string());
        assert!(entry["action"].is_string());
    }
    let push_row = entries
        .iter()
        .find(|e| e["action"] == json!("sync.push"))
        .unwrap();
    assert_eq!(push_row["deviceId"].as_str().unwrap(), device_id);
}
