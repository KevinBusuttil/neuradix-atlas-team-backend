//! In-process token-bucket rate limiting for the unauthenticated surfaces.
//!
//! Three classes, each with its own per-minute budget (0 disables a class —
//! the store-parameterized test suite runs with everything at 0 and dedicated
//! tests construct apps with tiny limits):
//!
//! * **Auth** (`ATLAS_RL_AUTH_PER_MIN`, default 20) — company creation and
//!   invitation accept, keyed per client.
//! * **Webhook** (`ATLAS_RL_WEBHOOK_PER_MIN`, default 120) — webhook intake,
//!   keyed per **provider path** (providers deliver from many rotating
//!   addresses, so an IP key would neither identify the provider nor stop a
//!   flood that rotates sources).
//! * **Public** (`ATLAS_RL_PUBLIC_PER_MIN`, default 60) — the token-in-path
//!   portal/pay planes, keyed per client.
//!
//! The client key is the first `X-Forwarded-For` address when
//! `ATLAS_TRUST_PROXY` is on (default — the deployment sits behind nginx on
//! 127.0.0.1, which appends the real peer), else the socket peer address.
//!
//! Deliberately **not** limited: `/health` (uptime probes) and every
//! authenticated endpoint — devices poll sync frequently by design, and
//! authenticated traffic is already attributable and revocable per token.
//!
//! Over-limit responses are 429 with a JSON `{"error": ...}` body and a
//! `Retry-After` header saying when one token will be back.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Extensions, HeaderMap};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::{AppConfig, AppState};

/// Rate-limit class; part of the bucket key, so classes never share buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RlClass {
    /// Unauthenticated auth-ish endpoints (company creation, invitation
    /// accept).
    Auth,
    /// Webhook intake, per provider path.
    Webhook,
    /// Public token-in-path portal/pay pages.
    Public,
}

/// How often the bucket map is swept for idle entries.
const PRUNE_INTERVAL: Duration = Duration::from_secs(120);
/// A bucket idle this long has fully refilled (budgets are per minute), so
/// dropping it and starting fresh later is behaviour-neutral.
const IDLE_EVICT: Duration = Duration::from_secs(120);

struct Bucket {
    tokens: f64,
    updated: Instant,
}

struct Buckets {
    map: HashMap<(RlClass, String), Bucket>,
    last_prune: Instant,
}

/// Token-bucket limiter: capacity = per-minute budget, refill = budget/60 per
/// second, one token per request. Small `Mutex<HashMap>` — the whole check is
/// a hash lookup and a few float ops, and the unauthenticated surfaces this
/// guards are low-traffic by design.
pub struct RateLimiter {
    auth_per_min: u32,
    webhook_per_min: u32,
    public_per_min: u32,
    buckets: Mutex<Buckets>,
}

impl RateLimiter {
    pub fn new(config: &AppConfig) -> Self {
        Self {
            auth_per_min: config.rl_auth_per_min,
            webhook_per_min: config.rl_webhook_per_min,
            public_per_min: config.rl_public_per_min,
            buckets: Mutex::new(Buckets {
                map: HashMap::new(),
                last_prune: Instant::now(),
            }),
        }
    }

    fn per_min(&self, class: RlClass) -> u32 {
        match class {
            RlClass::Auth => self.auth_per_min,
            RlClass::Webhook => self.webhook_per_min,
            RlClass::Public => self.public_per_min,
        }
    }

    /// Take one token from `(class, key)`'s bucket. `Err(retry_after_secs)`
    /// when the bucket is empty; a class whose budget is 0 is disabled and
    /// always admits.
    pub fn check(&self, class: RlClass, key: &str) -> Result<(), u64> {
        let per_min = self.per_min(class);
        if per_min == 0 {
            return Ok(());
        }
        let capacity = f64::from(per_min);
        let rate = capacity / 60.0; // tokens per second
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap();
        if now.duration_since(buckets.last_prune) >= PRUNE_INTERVAL {
            buckets
                .map
                .retain(|_, bucket| now.duration_since(bucket.updated) < IDLE_EVICT);
            buckets.last_prune = now;
        }
        let bucket = buckets
            .map
            .entry((class, key.to_string()))
            .or_insert(Bucket {
                tokens: capacity,
                updated: now,
            });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // Seconds until one whole token is back, never reported as 0.
            Err(((1.0 - bucket.tokens) / rate).ceil().max(1.0) as u64)
        }
    }
}

/// The limiter key for one client: the first `X-Forwarded-For` entry when the
/// proxy is trusted, else the socket peer address (`ConnectInfo`, present in
/// production via `into_make_service_with_connect_info`). "unknown" when
/// neither exists (in-process test requests) — those share one bucket.
pub fn client_key(config: &AppConfig, headers: &HeaderMap, extensions: &Extensions) -> String {
    if config.trust_proxy {
        if let Some(first) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|addr| !addr.is_empty())
        {
            return first.to_string();
        }
    }
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

async fn enforce(
    state: AppState,
    class: RlClass,
    key: String,
    req: Request,
    next: Next,
) -> Response {
    match state.limiter.check(class, &key) {
        Ok(()) => next.run(req).await,
        Err(retry_after_secs) => {
            tracing::warn!(class = ?class, key = %key, "rate limit exceeded");
            ApiError::TooManyRequests {
                message: "rate limit exceeded; retry later".into(),
                retry_after_secs,
            }
            .into_response()
        }
    }
}

/// Middleware for the auth class (client-keyed).
pub async fn limit_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let key = client_key(&state.config, req.headers(), req.extensions());
    enforce(state, RlClass::Auth, key, req, next).await
}

/// Middleware for the public portal/pay class (client-keyed).
pub async fn limit_public(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let key = client_key(&state.config, req.headers(), req.extensions());
    enforce(state, RlClass::Public, key, req, next).await
}

/// Middleware for webhook intake, keyed per provider path so each provider
/// gets its own budget and no forged header can move a request between
/// buckets.
pub async fn limit_webhook(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let key = req.uri().path().to_string();
    enforce(state, RlClass::Webhook, key, req, next).await
}
