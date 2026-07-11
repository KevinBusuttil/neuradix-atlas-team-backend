//! Abuse protection (Phase 0 increment 0.9): the bootstrap gate on company
//! creation and the token-bucket rate limits on the unauthenticated
//! surfaces. Runs over `MemStore` by default; set `ATLAS_TEST_DATABASE_URL`
//! to run the same tests over `PgStore` (see `tests/support/mod.rs`).
//!
//! These tests build their own routers with explicit [`AppConfig`]s instead
//! of the shared harness — the knobs under test are exactly the ones the
//! shared harness turns off.

mod support;

use std::sync::Arc;

use atlas_team_backend::store::Store;
use atlas_team_backend::AppConfig;
use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

struct App {
    router: Router,
    store: Arc<dyn Store>,
    /// Keeps the per-test Postgres database alive for the test's lifetime.
    _db: Option<support::PgTestDb>,
}

async fn app_with(config: AppConfig) -> App {
    let (store, db) = support::test_store().await;
    App {
        router: atlas_team_backend::router_with(store.clone(), config),
        store,
        _db: db,
    }
}

/// One request with arbitrary extra headers; returns status, response
/// headers and the JSON body (Null when empty or not JSON).
async fn send(
    app: &App,
    method: Method,
    uri: &str,
    extra_headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body.to_string())).unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

fn company_body(name: &str) -> Value {
    json!({
        "name": name,
        "owner_email": format!("{}@example.com", name.to_lowercase().replace(' ', ".")),
        "owner_name": "Olivia Owner"
    })
}

async fn create_company(
    app: &App,
    extra_headers: &[(&str, &str)],
    name: &str,
) -> (StatusCode, HeaderMap, Value) {
    send(
        app,
        Method::POST,
        "/companies",
        extra_headers,
        company_body(name),
    )
    .await
}

// ---------------------------------------------------------------------------
// Bootstrap gate (ATLAS_BOOTSTRAP_TOKEN / X-Atlas-Bootstrap-Token)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn company_creation_stays_open_without_bootstrap_token() {
    // The self-hoster default: no configured token, no header required.
    let app = app_with(support::test_config()).await;
    let (status, _, body) = create_company(&app, &[], "Open Instance Ltd").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["token"].is_string());
}

#[tokio::test]
async fn bootstrap_gate_rejects_missing_header() {
    let app = app_with(AppConfig {
        bootstrap_token: Some("s3cret-bootstrap".into()),
        ..support::test_config()
    })
    .await;
    let (status, _, body) = create_company(&app, &[], "Gated Ltd").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // The error must tell the caller the instance is gated and how.
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("gated"), "unhelpful error: {error}");
    assert!(
        error.contains("X-Atlas-Bootstrap-Token"),
        "unhelpful error: {error}"
    );
}

#[tokio::test]
async fn bootstrap_gate_rejects_wrong_header() {
    let app = app_with(AppConfig {
        bootstrap_token: Some("s3cret-bootstrap".into()),
        ..support::test_config()
    })
    .await;
    let (status, _, body) = create_company(
        &app,
        &[("X-Atlas-Bootstrap-Token", "wrong-value")],
        "Gated Ltd",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"].as_str().unwrap().contains("gated"));
}

#[tokio::test]
async fn bootstrap_gate_accepts_the_configured_token() {
    let app = app_with(AppConfig {
        bootstrap_token: Some("s3cret-bootstrap".into()),
        ..support::test_config()
    })
    .await;
    let (status, _, body) = create_company(
        &app,
        &[("X-Atlas-Bootstrap-Token", "s3cret-bootstrap")],
        "Gated Ltd",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["token"].is_string());
    // The company really exists.
    let company_id = body["company"]["id"].as_str().unwrap().parse().unwrap();
    assert!(app.store.company(company_id).await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// Rate limiting (token buckets; see src/limit.rs)
// ---------------------------------------------------------------------------

/// Asserts the 429 contract: JSON `{"error": ...}` body plus a positive
/// integer `Retry-After` header.
fn assert_rate_limited(status: StatusCode, headers: &HeaderMap, body: &Value) {
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("rate limit"),
        "unhelpful error: {body}"
    );
    let retry_after: u64 = headers
        .get(header::RETRY_AFTER)
        .expect("429 must carry Retry-After")
        .to_str()
        .unwrap()
        .parse()
        .expect("Retry-After must be integer seconds");
    assert!(retry_after >= 1, "Retry-After must be at least 1 second");
}

#[tokio::test]
async fn auth_rate_limit_empties_to_429_with_retry_after() {
    let app = app_with(AppConfig {
        rl_auth_per_min: 2,
        ..support::test_config()
    })
    .await;
    let client = [("X-Forwarded-For", "203.0.113.7")];
    for i in 0..2 {
        let (status, _, body) = create_company(&app, &client, &format!("Bucket {i}")).await;
        assert_eq!(status, StatusCode::CREATED, "request {i}: {body}");
    }
    let (status, headers, body) = create_company(&app, &client, "Bucket 2").await;
    assert_rate_limited(status, &headers, &body);
}

#[tokio::test]
async fn different_forwarded_ips_do_not_share_a_bucket() {
    let app = app_with(AppConfig {
        rl_auth_per_min: 1,
        ..support::test_config()
    })
    .await;
    // First client drains its bucket...
    let (status, _, _) =
        create_company(&app, &[("X-Forwarded-For", "203.0.113.7")], "First A").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, headers, body) =
        create_company(&app, &[("X-Forwarded-For", "203.0.113.7")], "First B").await;
    assert_rate_limited(status, &headers, &body);
    // ...a different client is unaffected. (Also covers proxy chains: only
    // the FIRST X-Forwarded-For entry is the client.)
    let (status, _, body) = create_company(
        &app,
        &[("X-Forwarded-For", "198.51.100.9, 203.0.113.7")],
        "Second A",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn forwarded_header_is_ignored_when_proxy_is_untrusted() {
    // ATLAS_TRUST_PROXY=0: without a proxy in front, X-Forwarded-For is
    // client-controlled and must not open fresh buckets.
    let app = app_with(AppConfig {
        rl_auth_per_min: 1,
        trust_proxy: false,
        ..support::test_config()
    })
    .await;
    let (status, _, _) =
        create_company(&app, &[("X-Forwarded-For", "203.0.113.7")], "Forged A").await;
    assert_eq!(status, StatusCode::CREATED);
    // A "different" forged address still lands in the same (peer-keyed) bucket.
    let (status, headers, body) =
        create_company(&app, &[("X-Forwarded-For", "198.51.100.9")], "Forged B").await;
    assert_rate_limited(status, &headers, &body);
}

#[tokio::test]
async fn invitation_accept_shares_the_auth_class() {
    let app = app_with(AppConfig {
        rl_auth_per_min: 2,
        ..support::test_config()
    })
    .await;
    let client = [("X-Forwarded-For", "203.0.113.7")];
    // Bootstrap takes the first token; a bogus invitation accept takes the
    // second (401 — the limiter charged it before token resolution).
    let (status, _, _) = create_company(&app, &client, "Accept Co").await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send(
        &app,
        Method::POST,
        "/invitations/not-a-real-token/accept",
        &client,
        json!({ "display_name": "X" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, headers, body) = send(
        &app,
        Method::POST,
        "/invitations/not-a-real-token/accept",
        &client,
        json!({ "display_name": "X" }),
    )
    .await;
    assert_rate_limited(status, &headers, &body);
}

#[tokio::test]
async fn webhook_rate_limit_is_per_provider_path() {
    let app = app_with(AppConfig {
        rl_webhook_per_min: 1,
        ..support::test_config()
    })
    .await;
    let (status, _, body) = send(
        &app,
        Method::POST,
        "/webhooks/payments/paypal",
        &[],
        json!({ "n": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = send(
        &app,
        Method::POST,
        "/webhooks/payments/paypal",
        &[],
        json!({ "n": 2 }),
    )
    .await;
    assert_rate_limited(status, &headers, &body);
    // A different provider path has its own bucket.
    let (status, _, body) = send(
        &app,
        Method::POST,
        "/webhooks/channels/shopify",
        &[],
        json!({ "n": 3 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn public_portal_and_pay_pages_are_rate_limited() {
    let app = app_with(AppConfig {
        rl_public_per_min: 2,
        ..support::test_config()
    })
    .await;
    let client = [("X-Forwarded-For", "203.0.113.7")];
    // Unknown tokens 404/401 — but they still consume the client's budget,
    // which is the point: token scanning gets throttled.
    for uri in ["/portal/some-token", "/pay/some-token"] {
        let (status, _, _) = send(&app, Method::GET, uri, &client, Value::Null).await;
        assert_ne!(status, StatusCode::TOO_MANY_REQUESTS);
    }
    let (status, headers, body) = send(
        &app,
        Method::GET,
        "/portal/some-token",
        &client,
        Value::Null,
    )
    .await;
    assert_rate_limited(status, &headers, &body);
}

#[tokio::test]
async fn health_and_authenticated_traffic_are_never_rate_limited() {
    // Everything throttled to 1/min — the tightest realistic misconfiguration.
    let app = app_with(AppConfig {
        rl_auth_per_min: 1,
        rl_webhook_per_min: 1,
        rl_public_per_min: 1,
        ..support::test_config()
    })
    .await;
    let client = [("X-Forwarded-For", "203.0.113.7")];
    // /health: unlimited.
    for _ in 0..5 {
        let (status, _, body) = send(&app, Method::GET, "/health", &client, Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // One company creation uses up the whole auth budget...
    let (status, _, body) = create_company(&app, &client, "Sync Co").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = body["token"].as_str().unwrap().to_string();
    let company_id = body["company"]["id"].as_str().unwrap().to_string();
    // ...but authenticated endpoints keep answering: devices poll sync
    // frequently by design and are attributable per token, so they are
    // deliberately outside the limiter.
    let bearer = format!("Bearer {token}");
    for i in 0..5 {
        let (status, _, body) = send(
            &app,
            Method::GET,
            &format!("/companies/{company_id}/members"),
            &[
                ("X-Forwarded-For", "203.0.113.7"),
                ("Authorization", bearer.as_str()),
            ],
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "authenticated call {i}: {body}");
    }
}
