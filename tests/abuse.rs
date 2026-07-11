//! Abuse protection (Phase 0 increment 0.9): the bootstrap gate on company
//! creation. Runs over `MemStore` by default; set `ATLAS_TEST_DATABASE_URL`
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
