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
    bootstrap_named(app, "Busuttil Trading Ltd", "owner@example.com").await
}

async fn bootstrap_named(app: &App, name: &str, owner_email: &str) -> Boot {
    let (status, body) = app
        .json(
            Method::POST,
            "/companies",
            None,
            json!({
                "name": name,
                "owner_email": owner_email,
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

/// Pushes `count` mutations (ids `m-1..m-count`) in one batch.
async fn push_numbered_mutations(
    app: &App,
    boot: &Boot,
    device_id: &str,
    token: &str,
    count: usize,
) {
    let mutations: Vec<Value> = (1..=count)
        .map(|n| {
            mutation(
                &format!("m-{n}"),
                &format!("CUST-{n:03}"),
                device_id,
                &boot.owner_user_id,
            )
        })
        .collect();
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/push", boot.company_id),
            Some(token),
            json!({ "mutations": mutations }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "push failed: {body}");
}

/// One pull page as (ids, syncVersions, hasMore).
async fn pull_page(
    app: &App,
    boot: &Boot,
    token: &str,
    after: i64,
    limit: Option<i64>,
) -> (Vec<String>, Vec<i64>, bool) {
    let mut uri = format!("/companies/{}/sync/pull?after={after}", boot.company_id);
    if let Some(limit) = limit {
        uri.push_str(&format!("&limit={limit}"));
    }
    let (status, body) = app.get(&uri, Some(token)).await;
    assert_eq!(status, StatusCode::OK, "pull failed: {body}");
    let mutations = body["mutations"].as_array().unwrap();
    let ids = mutations
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    let versions = mutations
        .iter()
        .map(|m| m["syncVersion"].as_str().unwrap().parse::<i64>().unwrap())
        .collect();
    (ids, versions, body["hasMore"].as_bool().unwrap())
}

#[tokio::test]
async fn pull_pages_honor_client_limit_and_walk_the_whole_log() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_a, token_a) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Device A").await;
    push_numbered_mutations(&app, &boot, &device_a, &token_a, 5).await;

    // Page 1: exactly the page size, hasMore true.
    let (ids, versions, has_more) = pull_page(&app, &boot, &token_a, 0, Some(2)).await;
    assert_eq!(ids, vec!["m-1", "m-2"]);
    assert_eq!(versions, vec![1, 2]);
    assert!(has_more);

    // Walk the whole log with after = last returned version; versions stay
    // strictly ascending across pages, no record skipped or duplicated.
    let mut after = 0;
    let mut all_ids = Vec::new();
    let mut all_versions = Vec::new();
    let mut pages = 0;
    loop {
        let (ids, versions, has_more) = pull_page(&app, &boot, &token_a, after, Some(2)).await;
        assert!(ids.len() <= 2);
        all_ids.extend(ids);
        all_versions.extend(versions);
        pages += 1;
        if !has_more {
            break;
        }
        after = *all_versions.last().unwrap();
    }
    assert_eq!(pages, 3);
    assert_eq!(all_ids, vec!["m-1", "m-2", "m-3", "m-4", "m-5"]);
    assert_eq!(all_versions, vec![1, 2, 3, 4, 5]);
    // The final page reported hasMore=false and was short.
    assert_eq!(all_ids.len() % 2, 1);
}

#[tokio::test]
async fn pull_limit_absent_zero_or_oversized_uses_the_server_max() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_a, token_a) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Device A").await;
    // 5 past the default server max of 200.
    push_numbered_mutations(&app, &boot, &device_a, &token_a, 205).await;

    // No limit → server max, hasMore exact.
    let (ids, versions, has_more) = pull_page(&app, &boot, &token_a, 0, None).await;
    assert_eq!(ids.len(), 200);
    assert_eq!(ids[0], "m-1");
    assert_eq!(ids[199], "m-200");
    assert_eq!(versions[199], 200);
    assert!(has_more);

    // limit=0 and a limit past the server max both clamp to the server max.
    let (ids, _, has_more) = pull_page(&app, &boot, &token_a, 0, Some(0)).await;
    assert_eq!(ids.len(), 200);
    assert!(has_more);
    let (ids, _, has_more) = pull_page(&app, &boot, &token_a, 0, Some(100_000)).await;
    assert_eq!(ids.len(), 200);
    assert!(has_more);

    // The final page.
    let (ids, versions, has_more) = pull_page(&app, &boot, &token_a, 200, None).await;
    assert_eq!(ids, vec!["m-201", "m-202", "m-203", "m-204", "m-205"]);
    assert_eq!(versions, vec![201, 202, 203, 204, 205]);
    assert!(!has_more);
}

#[tokio::test]
async fn pull_pages_stay_consistent_when_mutations_arrive_between_pages() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_a, token_a) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Device A").await;
    push_numbered_mutations(&app, &boot, &device_a, &token_a, 4).await;

    let (ids, versions, has_more) = pull_page(&app, &boot, &token_a, 0, Some(2)).await;
    assert_eq!(ids, vec!["m-1", "m-2"]);
    assert!(has_more);
    let mut after = *versions.last().unwrap();

    // New mutations land between pages: the cursor keeps the walk exact —
    // nothing skipped, nothing duplicated, and the newcomers show up at the
    // tail in version order.
    let extra: Vec<Value> = (5..=6)
        .map(|n| {
            mutation(
                &format!("m-{n}"),
                &format!("CUST-{n:03}"),
                &device_a,
                &boot.owner_user_id,
            )
        })
        .collect();
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/sync/push", boot.company_id),
            Some(&token_a),
            json!({ "mutations": extra }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let mut all_ids = ids;
    loop {
        let (ids, versions, has_more) = pull_page(&app, &boot, &token_a, after, Some(2)).await;
        all_ids.extend(ids);
        if !has_more {
            break;
        }
        after = *versions.last().unwrap();
    }
    assert_eq!(all_ids, vec!["m-1", "m-2", "m-3", "m-4", "m-5", "m-6"]);
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

// ---------------------------------------------------------------------------
// Credential lifecycle (increment 0.5): device management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn device_list_scopes_to_own_devices_unless_owner_or_admin() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (sales_user_id, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;
    let (owner_device, _) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Front desk").await;
    let (sales_device, _) =
        register_device(&app, &boot.company_id, &sales_token, "Sam's phone").await;

    let uri = format!("/companies/{}/devices", boot.company_id);

    // A plain member sees only their own devices.
    let (status, body) = app.get(&uri, Some(&sales_token)).await;
    assert_eq!(status, StatusCode::OK, "sales device list failed: {body}");
    let devices = body["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["id"].as_str().unwrap(), sales_device);
    assert_eq!(devices[0]["userId"].as_str().unwrap(), sales_user_id);
    assert!(devices[0]["name"].is_string());
    assert!(devices[0]["createdAt"].is_string());
    assert!(devices[0]["revokedAt"].is_null());

    // Owner sees every device in the company.
    let (status, body) = app.get(&uri, Some(&boot.owner_token)).await;
    assert_eq!(status, StatusCode::OK);
    let devices = body["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    let ids: Vec<&str> = devices.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&owner_device.as_str()));
    assert!(ids.contains(&sales_device.as_str()));
}

#[tokio::test]
async fn device_auth_stamps_last_seen_at() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_id, device_token) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Front desk").await;

    // Fresh device: never seen.
    let uri = format!("/companies/{}/devices", boot.company_id);
    let (_, body) = app.get(&uri, Some(&boot.owner_token)).await;
    let device = &body["devices"].as_array().unwrap()[0];
    assert!(device["lastSeenAt"].is_null());

    // Any authenticated device request stamps last_seen_at.
    let (status, _) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&device_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = app.get(&uri, Some(&boot.owner_token)).await;
    let device = body["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"].as_str().unwrap() == device_id)
        .unwrap()
        .clone();
    assert!(device["lastSeenAt"].is_string(), "no lastSeenAt: {device}");
}

#[tokio::test]
async fn revoked_device_token_is_rejected_on_push_and_pull() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (device_id, device_token) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Front desk").await;

    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/devices/{device_id}/revoke", boot.company_id),
            Some(&boot.owner_token),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "revoke failed: {body}");
    assert_eq!(body["id"].as_str().unwrap(), device_id);
    let revoked_at = body["revokedAt"].as_str().unwrap().to_string();

    // The dead token no longer authenticates sync push...
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
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // ...nor sync pull.
    let (status, _) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&device_token),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Revoking again is idempotent: 200, same original revokedAt.
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/devices/{device_id}/revoke", boot.company_id),
            Some(&boot.owner_token),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revokedAt"].as_str().unwrap(), revoked_at);
}

#[tokio::test]
async fn device_revocation_permission_matrix() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (_, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;
    let (_, admin_token) = join_member(&app, &boot, "admin@example.com", "admin").await;
    let (owner_device, _) =
        register_device(&app, &boot.company_id, &boot.owner_token, "Front desk").await;
    let (sales_device_a, _) =
        register_device(&app, &boot.company_id, &sales_token, "Sam's phone").await;
    let (sales_device_b, _) =
        register_device(&app, &boot.company_id, &sales_token, "Sam's tablet").await;

    let revoke = |device: String, token: String| {
        let app = &app;
        let company_id = boot.company_id.clone();
        async move {
            app.json(
                Method::POST,
                &format!("/companies/{company_id}/devices/{device}/revoke"),
                Some(&token),
                json!({}),
            )
            .await
        }
    };

    // A plain member cannot revoke someone else's device.
    let (status, _) = revoke(owner_device.clone(), sales_token.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A plain member may revoke their own device.
    let (status, _) = revoke(sales_device_a.clone(), sales_token.clone()).await;
    assert_eq!(status, StatusCode::OK);

    // Admin may revoke anyone's device.
    let (status, _) = revoke(sales_device_b.clone(), admin_token.clone()).await;
    assert_eq!(status, StatusCode::OK);

    // A device that is not in this company is a 404, even for the owner.
    let rival = bootstrap_named(&app, "Rival Co", "rival@example.com").await;
    let (rival_device, _) =
        register_device(&app, &rival.company_id, &rival.owner_token, "Elsewhere").await;
    let (status, _) = revoke(rival_device, boot.owner_token.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Credential lifecycle (increment 0.5): member management
// ---------------------------------------------------------------------------

#[tokio::test]
async fn member_list_is_visible_to_every_member() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (sales_user_id, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;

    let uri = format!("/companies/{}/members", boot.company_id);
    let (status, body) = app.get(&uri, Some(&sales_token)).await;
    assert_eq!(status, StatusCode::OK, "member list failed: {body}");
    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    let owner = members
        .iter()
        .find(|m| m["userId"].as_str().unwrap() == boot.owner_user_id)
        .unwrap();
    assert_eq!(owner["email"].as_str().unwrap(), "owner@example.com");
    assert_eq!(owner["role"].as_str().unwrap(), "owner");
    assert!(owner["displayName"].is_string());
    assert!(owner["createdAt"].is_string());
    let sales = members
        .iter()
        .find(|m| m["userId"].as_str().unwrap() == sales_user_id)
        .unwrap();
    assert_eq!(sales["role"].as_str().unwrap(), "sales");
}

#[tokio::test]
async fn member_removal_permission_matrix() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (admin_a_id, admin_a_token) =
        join_member(&app, &boot, "admin.a@example.com", "admin").await;
    let (admin_b_id, _) = join_member(&app, &boot, "admin.b@example.com", "admin").await;
    let (sales_id, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;

    let remove = |user: String, token: String| {
        let app = &app;
        let company_id = boot.company_id.clone();
        async move {
            app.json(
                Method::DELETE,
                &format!("/companies/{company_id}/members/{user}"),
                Some(&token),
                json!({}),
            )
            .await
        }
    };

    // A plain member cannot remove anyone.
    let (status, _) = remove(admin_a_id.clone(), sales_token.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admins cannot remove owners or other admins.
    let (status, _) = remove(boot.owner_user_id.clone(), admin_a_token.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = remove(admin_b_id.clone(), admin_a_token.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An admin may remove a plain member.
    let (status, body) = remove(sales_id.clone(), admin_a_token.clone()).await;
    assert_eq!(status, StatusCode::OK, "admin removal failed: {body}");
    assert_eq!(body["removed"], json!(true));

    // Owner may remove an admin.
    let (status, _) = remove(admin_b_id.clone(), boot.owner_token.clone()).await;
    assert_eq!(status, StatusCode::OK);

    // Removing someone who is not a member is a 404.
    let (status, _) = remove(sales_id, boot.owner_token.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn last_owner_cannot_be_removed_or_demoted() {
    let app = App::new();
    let boot = bootstrap(&app).await;

    // The sole owner cannot self-remove.
    let (status, body) = app
        .json(
            Method::DELETE,
            &format!(
                "/companies/{}/members/{}",
                boot.company_id, boot.owner_user_id
            ),
            Some(&boot.owner_token),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "expected 409: {body}");

    // ...nor demote themself.
    let (status, body) = app
        .json(
            Method::POST,
            &format!(
                "/companies/{}/members/{}/role",
                boot.company_id, boot.owner_user_id
            ),
            Some(&boot.owner_token),
            json!({ "role": "admin" }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "expected 409: {body}");

    // With a second owner promoted, self-removal works.
    let (second_id, _) = join_member(&app, &boot, "second@example.com", "admin").await;
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/members/{second_id}/role", boot.company_id),
            Some(&boot.owner_token),
            json!({ "role": "owner" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "promotion failed: {body}");
    assert_eq!(body["role"].as_str().unwrap(), "owner");

    let (status, _) = app
        .json(
            Method::DELETE,
            &format!(
                "/companies/{}/members/{}",
                boot.company_id, boot.owner_user_id
            ),
            Some(&boot.owner_token),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn owners_cannot_be_removed_by_anyone_else() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (second_id, _) = join_member(&app, &boot, "second@example.com", "admin").await;
    // Promote to a second owner.
    let (status, _) = app
        .json(
            Method::POST,
            &format!("/companies/{}/members/{second_id}/role", boot.company_id),
            Some(&boot.owner_token),
            json!({ "role": "owner" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Even an owner cannot remove another owner — owners only leave by
    // their own hand.
    let (status, _) = app
        .json(
            Method::DELETE,
            &format!("/companies/{}/members/{second_id}", boot.company_id),
            Some(&boot.owner_token),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn role_change_is_owner_only_and_validates_the_role() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (_, admin_token) = join_member(&app, &boot, "admin@example.com", "admin").await;
    let (sales_id, _) = join_member(&app, &boot, "sales@example.com", "sales").await;
    let uri = format!("/companies/{}/members/{sales_id}/role", boot.company_id);

    // Admins cannot change roles — owner only.
    let (status, _) = app
        .json(
            Method::POST,
            &uri,
            Some(&admin_token),
            json!({ "role": "stock" }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Unknown role → 400.
    let (status, _) = app
        .json(
            Method::POST,
            &uri,
            Some(&boot.owner_token),
            json!({ "role": "superuser" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Owner changes the role.
    let (status, body) = app
        .json(
            Method::POST,
            &uri,
            Some(&boot.owner_token),
            json!({ "role": "accountant" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "role change failed: {body}");
    assert_eq!(body["userId"].as_str().unwrap(), sales_id);
    assert_eq!(body["role"].as_str().unwrap(), "accountant");
}

#[tokio::test]
async fn member_removal_revokes_their_devices() {
    let app = App::new();
    let boot = bootstrap(&app).await;
    let (sales_id, sales_token) = join_member(&app, &boot, "sales@example.com", "sales").await;
    let (_, sales_device_token) =
        register_device(&app, &boot.company_id, &sales_token, "Sam's phone").await;

    // The device token works before removal.
    let (status, _) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&sales_device_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = app
        .json(
            Method::DELETE,
            &format!("/companies/{}/members/{sales_id}", boot.company_id),
            Some(&boot.owner_token),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "removal failed: {body}");
    assert_eq!(body["revokedDevices"], json!(1));

    // The removed member's device token is dead (401 — the token itself no
    // longer resolves).
    let (status, _) = app
        .get(
            &format!("/companies/{}/sync/pull?after=0", boot.company_id),
            Some(&sales_device_token),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Their (account-global) user token still authenticates, but membership
    // is gone → 403 on company routes.
    let (status, _) = app
        .get(
            &format!("/companies/{}/members", boot.company_id),
            Some(&sales_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Credential lifecycle (increment 0.5): user-token expiry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expired_user_token_is_rejected_and_legacy_null_expiry_still_works() {
    use atlas_team_backend::store::Store;

    let app = App::new();
    let boot = bootstrap(&app).await;
    let user_id = uuid::Uuid::parse_str(&boot.owner_user_id).unwrap();
    let company_id = uuid::Uuid::parse_str(&boot.company_id).unwrap();
    let uri = format!("/companies/{}/members", boot.company_id);

    // A token that expired an hour ago, planted directly through the store.
    let expired_token = "expired-user-token";
    let hash = hex::encode(Sha256::digest(expired_token.as_bytes()));
    app.store
        .insert_user_token(
            &hash,
            user_id,
            company_id,
            Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        )
        .await
        .unwrap();
    let (status, _) = app.get(&uri, Some(expired_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A legacy token with a null expiry (pre-migration) is non-expiring.
    let legacy_token = "legacy-user-token";
    let hash = hex::encode(Sha256::digest(legacy_token.as_bytes()));
    app.store
        .insert_user_token(&hash, user_id, company_id, None)
        .await
        .unwrap();
    let (status, body) = app.get(&uri, Some(legacy_token)).await;
    assert_eq!(status, StatusCode::OK, "legacy token failed: {body}");

    // A token with a future expiry works.
    let fresh_token = "fresh-user-token";
    let hash = hex::encode(Sha256::digest(fresh_token.as_bytes()));
    app.store
        .insert_user_token(
            &hash,
            user_id,
            company_id,
            Some(chrono::Utc::now() + chrono::Duration::days(30)),
        )
        .await
        .unwrap();
    let (status, _) = app.get(&uri, Some(fresh_token)).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Credential lifecycle (increment 0.5): hashed invitation tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invitation_tokens_are_stored_hashed_and_wrong_tokens_401() {
    use atlas_team_backend::store::Store;

    let app = App::new();
    let boot = bootstrap(&app).await;
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/companies/{}/invitations", boot.company_id),
            Some(&boot.owner_token),
            json!({ "email": "hashed@example.com", "role": "sales" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "invite failed: {body}");
    let token = body["token"].as_str().unwrap().to_string();

    // The store holds only the SHA-256 hash of the token — looking up by the
    // plaintext finds nothing, looking up by the hash finds the invitation.
    assert!(app
        .store
        .invitation_by_hash(&token)
        .await
        .unwrap()
        .is_none());
    let hash = hex::encode(Sha256::digest(token.as_bytes()));
    let stored = app.store.invitation_by_hash(&hash).await.unwrap().unwrap();
    assert_eq!(stored.email, "hashed@example.com");
    assert_eq!(stored.token_hash, hash);

    // A wrong token is a 401.
    let (status, _) = app
        .json(
            Method::POST,
            "/invitations/not-the-real-token/accept",
            None,
            json!({ "display_name": "Impostor" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The real token still accepts (hashed lookup end to end).
    let (status, body) = app
        .json(
            Method::POST,
            &format!("/invitations/{token}/accept"),
            None,
            json!({ "display_name": "Hasheen Joiner" }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "accept failed: {body}");
    assert_eq!(body["role"].as_str().unwrap(), "sales");
}
