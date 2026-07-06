//! Atlas Team Rust Backend — Phase 2 coordination MVP.
//!
//! Local-first Flutter clients keep drafting locally; this service is the
//! always-on coordination authority: identity (companies, users, memberships,
//! devices), the per-company mutation-log sync the client's `CloudAdapter`
//! contract expects, content-addressed blobs, webhook intake, and the audit
//! log. The posting engine (official documents, numbering, GL) is Phase 3 and
//! deliberately absent.

pub mod api;
pub mod auth;
pub mod error;
pub mod model;
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
