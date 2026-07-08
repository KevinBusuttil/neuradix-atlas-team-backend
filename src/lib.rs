//! Atlas Team Rust Backend — Phase 2 coordination plane + Phase 3 posting
//! authority.
//!
//! Local-first Flutter clients keep drafting locally; this service is the
//! always-on authority: identity (companies, users, memberships, devices),
//! the per-company mutation-log sync the client's `CloudAdapter` contract
//! expects, content-addressed blobs, webhook intake, the audit log, and —
//! since Phase 3 — official document postings (gap-free numbering, GL +
//! stock ledger + COGS, reversals, settlements) via the command API in
//! [`posting`].

pub mod api;
pub mod auth;
pub mod error;
pub mod model;
pub mod pay;
pub mod portal;
pub mod posting;
pub mod projection;
pub mod store;

use std::sync::Arc;

use store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    /// Stripe webhook signing secret (env `STRIPE_WEBHOOK_SECRET`, read once
    /// at startup). `None` makes the Stripe webhook endpoint fail **closed**:
    /// events are intake-logged but never processed (503).
    pub stripe_webhook_secret: Option<String>,
}

/// Build the full application router over any [`Store`], reading the Stripe
/// webhook secret from the `STRIPE_WEBHOOK_SECRET` environment variable.
pub fn router(store: Arc<dyn Store>) -> axum::Router {
    let stripe_webhook_secret = std::env::var("STRIPE_WEBHOOK_SECRET")
        .ok()
        .filter(|secret| !secret.trim().is_empty());
    router_with(store, stripe_webhook_secret)
}

/// Build the router with an explicit Stripe webhook secret (tests; embedders
/// that manage configuration themselves).
pub fn router_with(store: Arc<dyn Store>, stripe_webhook_secret: Option<String>) -> axum::Router {
    api::router(AppState {
        store,
        stripe_webhook_secret,
    })
}
