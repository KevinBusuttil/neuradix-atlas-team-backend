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
pub mod limit;
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
#[derive(Debug, Clone)]
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
    /// Whether the first `X-Forwarded-For` address is trusted as the client
    /// key for rate limiting (env `ATLAS_TRUST_PROXY`, default **true** — the
    /// service is designed to sit behind nginx on 127.0.0.1). Self-hosters
    /// exposing the port directly MUST set `ATLAS_TRUST_PROXY=0`, otherwise
    /// clients can dodge (or poison) buckets by forging the header; with it
    /// off, the socket peer address is used instead.
    pub trust_proxy: bool,
    /// Rate limit for unauthenticated auth-ish endpoints — company creation
    /// and invitation accept — in requests per minute per client (env
    /// `ATLAS_RL_AUTH_PER_MIN`, default 20; 0 disables).
    pub rl_auth_per_min: u32,
    /// Rate limit for webhook intake, per provider path (env
    /// `ATLAS_RL_WEBHOOK_PER_MIN`, default 120; 0 disables).
    pub rl_webhook_per_min: u32,
    /// Rate limit for the public token-in-path portal/pay pages, per client
    /// (env `ATLAS_RL_PUBLIC_PER_MIN`, default 60; 0 disables).
    pub rl_public_per_min: u32,
    /// Cap on stored webhook events per (kind, provider path) — the durable
    /// intake backlog an abuser could otherwise grow without bound (env
    /// `ATLAS_WEBHOOK_BACKLOG_MAX`, default 1000; 0 disables). Intake past
    /// the cap answers 429, which is safe: serious providers (Stripe
    /// documented) retry with backoff for days.
    pub webhook_backlog_max: i64,
}

impl Default for AppConfig {
    /// Production defaults — the same values `from_env` falls back to.
    fn default() -> Self {
        Self {
            stripe_webhook_secret: None,
            bootstrap_token: None,
            trust_proxy: true,
            rl_auth_per_min: 20,
            rl_webhook_per_min: 120,
            rl_public_per_min: 60,
            webhook_backlog_max: 1000,
        }
    }
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
        let defaults = Self::default();
        Self {
            stripe_webhook_secret: env_nonempty("STRIPE_WEBHOOK_SECRET"),
            bootstrap_token,
            trust_proxy: env_flag("ATLAS_TRUST_PROXY", defaults.trust_proxy),
            rl_auth_per_min: env_parse("ATLAS_RL_AUTH_PER_MIN", defaults.rl_auth_per_min),
            rl_webhook_per_min: env_parse("ATLAS_RL_WEBHOOK_PER_MIN", defaults.rl_webhook_per_min),
            rl_public_per_min: env_parse("ATLAS_RL_PUBLIC_PER_MIN", defaults.rl_public_per_min),
            webhook_backlog_max: env_parse(
                "ATLAS_WEBHOOK_BACKLOG_MAX",
                defaults.webhook_backlog_max,
            ),
        }
    }
}

/// A parseable environment variable, or `default` (absent, empty or garbage).
fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> T {
    env_nonempty(name)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// A boolean flag: `0`, `false`, `no` and `off` (case-insensitive) read as
/// false, anything else set reads as true; absent means `default`.
fn env_flag(name: &str, default: bool) -> bool {
    match env_nonempty(name) {
        Some(value) => !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => default,
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
    /// In-process token buckets for the unauthenticated surfaces (see
    /// [`limit`]). Per router instance — a multi-replica deployment would
    /// need a shared limiter, but this service runs as one process.
    pub limiter: Arc<limit::RateLimiter>,
}

/// Build the full application router over any [`Store`], reading the
/// configuration from the environment ([`AppConfig::from_env`]).
pub fn router(store: Arc<dyn Store>) -> axum::Router {
    router_with(store, AppConfig::from_env())
}

/// Build the router with explicit configuration (tests; embedders that
/// manage configuration themselves).
pub fn router_with(store: Arc<dyn Store>, config: AppConfig) -> axum::Router {
    let limiter = Arc::new(limit::RateLimiter::new(&config));
    api::router(AppState {
        store,
        config: Arc::new(config),
        limiter,
    })
}
