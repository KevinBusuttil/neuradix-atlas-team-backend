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

/// Runtime configuration, read once at startup (env) or supplied explicitly
/// (tests, embedders). Everything abuse-protection-related lives here so the
/// test harness can turn the knobs per app instance instead of mutating
/// process-global environment variables under a parallel test runner.
#[derive(Debug, Clone, Default)]
pub struct AppConfig {
    /// Stripe webhook signing secret (env `STRIPE_WEBHOOK_SECRET`). `None`
    /// makes the Stripe webhook endpoint fail **closed**: events are
    /// intake-logged but never processed (503).
    pub stripe_webhook_secret: Option<String>,
    /// Shared secret gating company bootstrap (env `ATLAS_BOOTSTRAP_TOKEN`).
    /// When set, `POST /companies` requires the matching
    /// `X-Atlas-Bootstrap-Token` header (403 otherwise). When unset the
    /// endpoint stays open — the self-hoster default — and startup logs a
    /// WARN saying so.
    pub bootstrap_token: Option<String>,
}

impl AppConfig {
    /// Production configuration: read every knob from the environment,
    /// warning about the ones whose absence leaves a surface open.
    pub fn from_env() -> Self {
        let bootstrap_token = env_nonempty("ATLAS_BOOTSTRAP_TOKEN");
        if bootstrap_token.is_none() {
            tracing::warn!(
                "ATLAS_BOOTSTRAP_TOKEN is not set: POST /companies accepts unauthenticated \
                 company creation (anyone who can reach this instance can bootstrap a company). \
                 Set ATLAS_BOOTSTRAP_TOKEN and send it as the X-Atlas-Bootstrap-Token header \
                 to gate bootstrap."
            );
        }
        Self {
            stripe_webhook_secret: env_nonempty("STRIPE_WEBHOOK_SECRET"),
            bootstrap_token,
        }
    }
}

/// A non-empty, trimmed environment variable.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<dyn Store>,
    pub config: Arc<AppConfig>,
}

/// Build the full application router over any [`Store`], reading the
/// configuration from the environment ([`AppConfig::from_env`]).
pub fn router(store: Arc<dyn Store>) -> axum::Router {
    router_with(store, AppConfig::from_env())
}

/// Build the router with explicit configuration (tests; embedders that
/// manage configuration themselves).
pub fn router_with(store: Arc<dyn Store>, config: AppConfig) -> axum::Router {
    api::router(AppState {
        store,
        config: Arc::new(config),
    })
}
