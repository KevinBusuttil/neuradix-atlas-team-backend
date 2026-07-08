//! Shared harness for the Phase 3 posting tests: the full router over
//! `MemStore`, a bootstrapped company, per-role members with device tokens,
//! command senders and ledger inspection helpers.

// Each test binary compiles this module independently and uses a different
// subset of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use atlas_team_backend::posting::model::{Bin, PartyTransaction, TaxTransaction};
use atlas_team_backend::store::MemStore;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

/// Numeric assertions tolerate sub-cent float noise (0.005, the same
/// tolerance as the engine's JE balance guard).
pub const TOLERANCE: f64 = 0.005;

pub struct TestApp {
    pub router: Router,
    pub store: Arc<MemStore>,
    pub company_id: String,
    pub owner_token: String,
    /// role -> device token (owner's created lazily too)
    device_tokens: HashMap<String, String>,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_stripe_secret(None).await
    }

    /// A TestApp whose Stripe webhook secret is configured explicitly
    /// (`None` = unset, the webhook endpoint fails closed).
    pub async fn with_stripe_secret(secret: Option<&str>) -> Self {
        let store = Arc::new(MemStore::new());
        let router = atlas_team_backend::router_with(store.clone(), secret.map(str::to_string));
        let mut app = Self {
            router,
            store,
            company_id: String::new(),
            owner_token: String::new(),
            device_tokens: HashMap::new(),
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
        app
    }

    pub fn company_uuid(&self) -> Uuid {
        self.company_id.parse().unwrap()
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

    // -- ledger inspection over MemStore ------------------------------------

    /// Σ(debit − credit) for an account across every posted GL entry.
    pub fn account_balance(&self, account: &str) -> f64 {
        self.store
            .all_gl_entries(self.company_uuid())
            .iter()
            .filter(|entry| entry.account == account)
            .map(|entry| entry.debit - entry.credit)
            .sum()
    }

    /// Σ(debit − credit) for an account scoped to one voucher.
    pub fn voucher_account(&self, account: &str, voucher_no: &str) -> f64 {
        self.store
            .all_gl_entries(self.company_uuid())
            .iter()
            .filter(|entry| entry.account == account && entry.voucher_no == voucher_no)
            .map(|entry| entry.debit - entry.credit)
            .sum()
    }

    pub fn gl_count(&self, voucher_no: &str) -> usize {
        self.store
            .all_gl_entries(self.company_uuid())
            .iter()
            .filter(|entry| entry.voucher_no == voucher_no)
            .count()
    }

    pub fn sle_count(&self, voucher_no: &str) -> usize {
        self.store
            .all_stock_ledger_entries(self.company_uuid())
            .iter()
            .filter(|sle| sle.voucher_no == voucher_no)
            .count()
    }

    /// A customer/supplier subledger row by its deterministic id.
    pub fn party_transaction(&self, id: &str) -> Option<PartyTransaction> {
        self.store
            .all_party_transactions(self.company_uuid())
            .into_iter()
            .find(|t| t.id == id)
    }

    /// A tax subledger row by its deterministic id.
    pub fn tax_transaction(&self, id: &str) -> Option<TaxTransaction> {
        self.store
            .all_tax_transactions(self.company_uuid())
            .into_iter()
            .find(|t| t.id == id)
    }

    pub fn party_transaction_count(&self, voucher_no: &str) -> usize {
        self.store
            .all_party_transactions(self.company_uuid())
            .iter()
            .filter(|t| t.voucher_no == voucher_no)
            .count()
    }

    pub fn tax_transaction_count(&self, voucher_no: &str) -> usize {
        self.store
            .all_tax_transactions(self.company_uuid())
            .iter()
            .filter(|t| t.voucher_no == voucher_no)
            .count()
    }

    pub fn settlement_count(&self, payment_no: &str) -> usize {
        self.store
            .all_settlements(self.company_uuid())
            .iter()
            .filter(|s| s.payment_voucher_no == payment_no)
            .count()
    }

    pub fn bin(&self, item: &str, warehouse: &str) -> Option<Bin> {
        self.store
            .all_bins(self.company_uuid())
            .into_iter()
            .find(|bin| bin.item == item && bin.warehouse == warehouse)
    }

    pub fn outstanding(&self, doctype: &str, id: &str) -> Option<f64> {
        self.store
            .document(self.company_uuid(), doctype, id)
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
