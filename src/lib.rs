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
pub mod posting;
pub mod store;

use std::sync::Arc;

use store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
}

/// Build the full application router over any [`Store`].
pub fn router(store: Arc<dyn Store>) -> axum::Router {
    api::router(AppState { store })
}
