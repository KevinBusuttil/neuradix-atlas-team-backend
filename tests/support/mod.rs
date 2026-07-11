//! Shared harness for the integration tests: the full router over either
//! store, a bootstrapped company, per-role members with device tokens,
//! command senders and ledger inspection helpers.
//!
//! # Store parameterization
//!
//! By default every test runs against [`MemStore`] — fast, no external
//! dependencies. Setting `ATLAS_TEST_DATABASE_URL` (an admin URL of a
//! disposable PostgreSQL server, e.g. `postgres://atlas:atlas@localhost/postgres`)
//! makes the SAME tests run against [`PgStore`] instead: each test creates
//! its own uniquely named database (so parallel tests stay isolated), runs
//! the embedded migrations through `PgStore::connect`, and drops the
//! database again when the test finishes.

// Each test binary compiles this module independently and uses a different
// subset of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use atlas_team_backend::posting::model::{
    Bin, GlEntry, PartyTransaction, StockLedgerEntry, TaxTransaction,
};
use atlas_team_backend::store::{MemStore, PgStore, Store};
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::Connection;
use tower::ServiceExt;
use uuid::Uuid;

/// Admin database URL that switches the suite from MemStore to PgStore.
pub const TEST_DB_ENV: &str = "ATLAS_TEST_DATABASE_URL";

/// Numeric assertions tolerate sub-cent float noise (0.005, the same
/// tolerance as the engine's JE balance guard).
pub const TOLERANCE: f64 = 0.005;

/// Guard for a per-test PostgreSQL database; dropping it drops the database.
pub struct PgTestDb {
    admin_url: String,
    name: String,
}

impl Drop for PgTestDb {
    fn drop(&mut self) {
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        // Drop needs its own runtime: the test's runtime is unusable from a
        // synchronous Drop (and may itself be shutting down).
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("cleanup runtime");
            runtime.block_on(async move {
                if let Ok(mut conn) = sqlx::postgres::PgConnection::connect(&admin_url).await {
                    // FORCE kills any connection the pool still holds.
                    let _ = sqlx::query(&format!("drop database \"{name}\" with (force)"))
                        .execute(&mut conn)
                        .await;
                    let _ = conn.close().await;
                }
            });
        });
        let _ = handle.join();
    }
}

/// Baseline configuration for the shared harness: no bootstrap gate, no
/// Stripe secret, and every rate limit at 0 (disabled) — the suite runs
/// massively in parallel and in-process requests all share one limiter key,
/// so real limits would trip constantly. Dedicated tests (`tests/abuse.rs`)
/// construct apps with tiny limits instead.
pub fn test_config() -> atlas_team_backend::AppConfig {
    atlas_team_backend::AppConfig {
        rl_auth_per_min: 0,
        rl_webhook_per_min: 0,
        rl_public_per_min: 0,
        webhook_backlog_max: 0,
        ..atlas_team_backend::AppConfig::default()
    }
}

/// Builds the store under test: MemStore by default, or a PgStore over a
/// fresh, isolated, fully migrated database when `ATLAS_TEST_DATABASE_URL`
/// is set.
pub async fn test_store() -> (Arc<dyn Store>, Option<PgTestDb>) {
    match std::env::var(TEST_DB_ENV) {
        Ok(admin_url) if !admin_url.trim().is_empty() => {
            let (store, guard) = pg_test_store(admin_url.trim()).await;
            (store, Some(guard))
        }
        _ => (Arc::new(MemStore::new()), None),
    }
}

async fn pg_test_store(admin_url: &str) -> (Arc<dyn Store>, PgTestDb) {
    let name = format!("atlas_test_{}", Uuid::new_v4().simple());
    let mut conn = sqlx::postgres::PgConnection::connect(admin_url)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to {TEST_DB_ENV}: {e}"));
    // Postgres serializes CREATE DATABASE on the shared template; parallel
    // tests briefly collide, so retry.
    let mut attempts = 0;
    loop {
        match sqlx::query(&format!("create database \"{name}\""))
            .execute(&mut conn)
            .await
        {
            Ok(_) => break,
            Err(e) if attempts < 50 => {
                attempts += 1;
                let _ = e;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => panic!("cannot create test database {name}: {e}"),
        }
    }
    let _ = conn.close().await;
    let guard = PgTestDb {
        admin_url: admin_url.to_string(),
        name: name.clone(),
    };
    let store = PgStore::connect(&database_url(admin_url, &name))
        .await
        .unwrap_or_else(|e| panic!("cannot migrate test database {name}: {e}"));
    (Arc::new(store), guard)
}

/// The admin URL with its database path swapped for `name` (query string,
/// if any, preserved).
fn database_url(admin_url: &str, name: &str) -> String {
    let (base, query) = match admin_url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (admin_url, None),
    };
    let authority_start = base.find("://").map(|i| i + 3).unwrap_or(0);
    let base = match base[authority_start..].rfind('/') {
        Some(i) => &base[..authority_start + i],
        None => base,
    };
    match query {
        Some(query) => format!("{base}/{name}?{query}"),
        None => format!("{base}/{name}"),
    }
}

pub struct TestApp {
    pub router: Router,
    pub store: Arc<dyn Store>,
    pub company_id: String,
    pub owner_token: String,
    pub owner_user_id: String,
    /// role -> device token (owner's created lazily too)
    device_tokens: HashMap<String, String>,
    /// Keeps the per-test Postgres database alive for the test's lifetime.
    _db: Option<PgTestDb>,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_stripe_secret(None).await
    }

    /// A TestApp whose Stripe webhook secret is configured explicitly
    /// (`None` = unset, the webhook endpoint fails closed).
    pub async fn with_stripe_secret(secret: Option<&str>) -> Self {
        let (store, db) = test_store().await;
        let config = atlas_team_backend::AppConfig {
            stripe_webhook_secret: secret.map(str::to_string),
            ..test_config()
        };
        let router = atlas_team_backend::router_with(store.clone(), config);
        let mut app = Self {
            router,
            store,
            company_id: String::new(),
            owner_token: String::new(),
            owner_user_id: String::new(),
            device_tokens: HashMap::new(),
            _db: db,
        };
        let (status, body) = app
            .request(
                Method::POST,
                "/companies",
                None,
                json!({
                    "name": "Fixture Trading Ltd",
                    "owner_email": "owner@example.com",
                    "owner_name": "Olivia Owner"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "bootstrap failed: {body}");
        app.company_id = body["company"]["id"].as_str().unwrap().to_string();
        app.owner_token = body["token"].as_str().unwrap().to_string();
        app.owner_user_id = body["userId"].as_str().unwrap().to_string();
        app
    }

    pub fn company_uuid(&self) -> Uuid {
        self.company_id.parse().unwrap()
    }

    pub fn owner_uuid(&self) -> Uuid {
        self.owner_user_id.parse().unwrap()
    }

    pub async fn request(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = builder.body(Body::from(body.to_string())).unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    /// A device token whose user holds `role` in the company. The owner's
    /// device belongs to the bootstrap owner; other roles get an invited
    /// member of that role, each with one registered device.
    pub async fn device_token(&mut self, role: &str) -> String {
        if let Some(token) = self.device_tokens.get(role) {
            return token.clone();
        }
        let user_token = if role == "owner" {
            self.owner_token.clone()
        } else {
            let (status, body) = self
                .request(
                    Method::POST,
                    &format!("/companies/{}/invitations", self.company_id),
                    Some(&self.owner_token.clone()),
                    json!({ "email": format!("{role}@example.com"), "role": role }),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "invite failed: {body}");
            let invitation = body["token"].as_str().unwrap().to_string();
            let (status, body) = self
                .request(
                    Method::POST,
                    &format!("/invitations/{invitation}/accept"),
                    None,
                    json!({ "display_name": format!("{role} user") }),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "accept failed: {body}");
            body["token"].as_str().unwrap().to_string()
        };
        let (status, body) = self
            .request(
                Method::POST,
                &format!("/companies/{}/devices", self.company_id),
                Some(&user_token),
                json!({ "name": format!("{role} device") }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "device failed: {body}");
        let token = body["deviceToken"].as_str().unwrap().to_string();
        self.device_tokens.insert(role.to_string(), token.clone());
        token
    }

    pub async fn put_settings(&self, patch: Value) {
        let (status, body) = self
            .request(
                Method::PUT,
                &format!("/companies/{}/settings", self.company_id),
                Some(&self.owner_token),
                patch,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "settings failed: {body}");
    }

    pub async fn upsert_item(&self, item: Value) {
        let (status, body) = self
            .request(
                Method::POST,
                &format!("/companies/{}/items", self.company_id),
                Some(&self.owner_token),
                item,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "item upsert failed: {body}");
    }

    pub async fn submit_as(&mut self, role: &str, body: Value) -> (StatusCode, Value) {
        let token = self.device_token(role).await;
        self.request(
            Method::POST,
            &format!("/companies/{}/commands/submit-document", self.company_id),
            Some(&token),
            body,
        )
        .await
    }

    pub async fn cancel_as(&mut self, role: &str, body: Value) -> (StatusCode, Value) {
        let token = self.device_token(role).await;
        self.request(
            Method::POST,
            &format!("/companies/{}/commands/cancel-document", self.company_id),
            Some(&token),
            body,
        )
        .await
    }

    // -- ledger inspection through the Store trait ---------------------------
    //
    // Everything goes through trait methods, so the same assertions verify
    // whichever store the suite is parameterized over.

    /// Every GL entry of the company.
    pub async fn all_gl_entries(&self) -> Vec<GlEntry> {
        self.store
            .gl_entries_ordered(self.company_uuid())
            .await
            .unwrap()
    }

    /// Σ(debit − credit) for an account across every posted GL entry.
    pub async fn account_balance(&self, account: &str) -> f64 {
        self.all_gl_entries()
            .await
            .iter()
            .filter(|entry| entry.account == account)
            .map(|entry| entry.debit - entry.credit)
            .sum()
    }

    /// Σ(debit − credit) for an account scoped to one voucher.
    pub async fn voucher_account(&self, account: &str, voucher_no: &str) -> f64 {
        self.store
            .gl_for_voucher(self.company_uuid(), voucher_no)
            .await
            .unwrap()
            .iter()
            .filter(|entry| entry.account == account)
            .map(|entry| entry.debit - entry.credit)
            .sum()
    }

    pub async fn gl_count(&self, voucher_no: &str) -> usize {
        self.store
            .gl_for_voucher(self.company_uuid(), voucher_no)
            .await
            .unwrap()
            .len()
    }

    pub async fn sle_count(&self, voucher_no: &str) -> usize {
        self.store
            .sles_for_voucher(self.company_uuid(), voucher_no)
            .await
            .unwrap()
            .len()
    }

    /// A GL entry by its deterministic id.
    pub async fn gl_entry(&self, id: &str) -> Option<GlEntry> {
        self.all_gl_entries()
            .await
            .into_iter()
            .find(|entry| entry.id == id)
    }

    /// A stock ledger entry by its deterministic id.
    pub async fn stock_ledger_entry(&self, id: &str) -> Option<StockLedgerEntry> {
        self.store
            .all_stock_ledger_entries(self.company_uuid())
            .await
            .unwrap()
            .into_iter()
            .find(|sle| sle.id == id)
    }

    /// A customer/supplier subledger row by its deterministic id.
    pub async fn party_transaction(&self, id: &str) -> Option<PartyTransaction> {
        self.store
            .all_party_transactions(self.company_uuid())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
    }

    /// A tax subledger row by its deterministic id.
    pub async fn tax_transaction(&self, id: &str) -> Option<TaxTransaction> {
        self.store
            .all_tax_transactions(self.company_uuid())
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.id == id)
    }

    pub async fn party_transaction_count(&self, voucher_no: &str) -> usize {
        self.store
            .party_transactions_for_voucher(self.company_uuid(), voucher_no)
            .await
            .unwrap()
            .len()
    }

    pub async fn tax_transaction_count(&self, voucher_no: &str) -> usize {
        self.store
            .tax_transactions_for_voucher(self.company_uuid(), voucher_no)
            .await
            .unwrap()
            .len()
    }

    pub async fn settlement_count(&self, payment_no: &str) -> usize {
        self.store
            .settlements_for_payment(self.company_uuid(), payment_no)
            .await
            .unwrap()
            .len()
    }

    pub async fn bin(&self, item: &str, warehouse: &str) -> Option<Bin> {
        self.store
            .all_bins(self.company_uuid())
            .await
            .unwrap()
            .into_iter()
            .find(|bin| bin.item == item && bin.warehouse == warehouse)
    }

    pub async fn outstanding(&self, doctype: &str, id: &str) -> Option<f64> {
        self.store
            .posted_document(self.company_uuid(), doctype, id)
            .await
            .unwrap()
            .and_then(|doc| {
                doc.payload
                    .get("outstanding_amount")
                    .and_then(Value::as_f64)
            })
    }
}

pub fn approx(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= TOLERANCE
}
