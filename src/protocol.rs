//! C6: the client/server protocol contract (`contract/team_protocol.json`).
//!
//! * The server speaks protocol [`PROTOCOL`] and says so on `GET /protocol`,
//!   on `/ready`, and in the `X-Atlas-Protocol` header of every response.
//! * A client sends its own level in the same request header. A client
//!   below the server's minimum gets **426 Upgrade Required** with a message
//!   a person can act on, before the request touches any data, so an
//!   outdated app can never write to the books.
//! * A request without the header comes from a client built before the
//!   contract existed and counts as protocol 0. The minimum defaults to 0 so
//!   those keep working; an operator raises it with
//!   `ATLAS_MIN_CLIENT_PROTOCOL` (clamped to [`PROTOCOL`]) to retire them.
//! * Only the client planes are checked (`/companies…`, `/invitations…`).
//!   Health, readiness, webhooks, the portal and the pay pages are not
//!   called by the app and are never refused here.
//! * The client applies the mirror rule: it will not sync with a server
//!   below its own minimum (`clientMinServerProtocol`).

use std::sync::OnceLock;

use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// The protocol this server speaks.
pub const PROTOCOL: u32 = 3;

/// The request and response header that carries a protocol level.
pub const HEADER: &str = "x-atlas-protocol";

/// The level of a request that carries no header (a pre-contract client).
pub const UNVERSIONED: u32 = 0;

/// The oldest client the server accepts unless configured otherwise.
pub const DEFAULT_MIN_CLIENT_PROTOCOL: u32 = 0;

/// The oldest server a current client agrees to sync with. The server does
/// not enforce this; it is published so the contract file is complete.
pub const CLIENT_MIN_SERVER_PROTOCOL: u32 = 3;

/// The configured minimum client protocol (read once).
pub fn min_client_protocol() -> u32 {
    static MIN: OnceLock<u32> = OnceLock::new();
    *MIN.get_or_init(|| parse_min(std::env::var("ATLAS_MIN_CLIENT_PROTOCOL").ok().as_deref()))
}

fn parse_min(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_MIN_CLIENT_PROTOCOL)
        .min(PROTOCOL)
}

/// What `GET /protocol` answers.
pub fn describe() -> Value {
    json!({
        "protocol": PROTOCOL,
        "minClientProtocol": min_client_protocol(),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

fn is_client_plane(path: &str) -> bool {
    path.starts_with("/companies") || path.starts_with("/invitations")
}

/// Why a client at `client` may not use a server whose minimum is `min`,
/// or None. Pure, for tests.
pub fn refusal(client: Option<&str>, min: u32) -> Option<(StatusCode, String)> {
    let level = match client {
        None => UNVERSIONED,
        Some(raw) => match raw.trim().parse::<u32>() {
            Ok(level) => level,
            Err(_) => {
                return Some((
                    StatusCode::BAD_REQUEST,
                    format!("{HEADER} must be a whole number"),
                ))
            }
        },
    };
    (level < min).then(|| {
        (
            StatusCode::UPGRADE_REQUIRED,
            format!(
                "This version of Atlas is too old for this Team server (it speaks \
                 protocol {level}, the server needs {min} or later). Update Atlas \
                 on this device, then sync again. Nothing was changed."
            ),
        )
    })
}

/// Middleware: refuse an outdated client on the client planes, and stamp
/// the server's protocol on every response.
pub async fn enforce(request: Request, next: Next) -> Response {
    let header_value = |value: u32| HeaderValue::from(value);
    if is_client_plane(request.uri().path()) {
        let client = request
            .headers()
            .get(HEADER)
            .map(|v| v.to_str().unwrap_or("?").to_string());
        if let Some((status, message)) = refusal(client.as_deref(), min_client_protocol()) {
            let mut response = (
                status,
                Json(json!({
                    "error": message,
                    "protocol": PROTOCOL,
                    "minClientProtocol": min_client_protocol(),
                })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(HEADER, header_value(PROTOCOL));
            return response;
        }
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(HEADER, header_value(PROTOCOL));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_file_matches_the_constants() {
        let raw = include_str!("../contract/team_protocol.json");
        let contract: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(contract["protocol"], PROTOCOL);
        assert_eq!(contract["header"].as_str().unwrap().to_lowercase(), HEADER);
        assert_eq!(contract["unversionedClientProtocol"], UNVERSIONED);
        assert_eq!(
            contract["defaultMinClientProtocol"],
            DEFAULT_MIN_CLIENT_PROTOCOL
        );
        assert_eq!(
            contract["clientMinServerProtocol"],
            CLIENT_MIN_SERVER_PROTOCOL
        );
        let levels = contract["levels"].as_array().unwrap();
        assert_eq!(levels.len() as u32, PROTOCOL, "one entry per level");
        assert_eq!(levels.last().unwrap()["protocol"], PROTOCOL);
    }

    #[test]
    fn refusal_rules() {
        assert!(refusal(None, 0).is_none());
        assert!(refusal(Some("3"), 3).is_none());
        assert!(refusal(Some("4"), 3).is_none());
        assert_eq!(
            refusal(None, 3).unwrap().0,
            StatusCode::UPGRADE_REQUIRED,
            "an unversioned client is refused once the minimum is raised"
        );
        assert_eq!(
            refusal(Some("2"), 3).unwrap().0,
            StatusCode::UPGRADE_REQUIRED
        );
        assert_eq!(refusal(Some("x"), 0).unwrap().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn minimum_is_clamped_to_this_server() {
        assert_eq!(parse_min(None), DEFAULT_MIN_CLIENT_PROTOCOL);
        assert_eq!(parse_min(Some("2")), 2);
        assert_eq!(parse_min(Some("99")), PROTOCOL);
        assert_eq!(parse_min(Some("junk")), DEFAULT_MIN_CLIENT_PROTOCOL);
    }
}
