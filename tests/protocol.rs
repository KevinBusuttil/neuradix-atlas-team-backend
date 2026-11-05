//! C6: the client/server protocol contract over real HTTP routing.
//!
//! This binary runs with `ATLAS_MIN_CLIENT_PROTOCOL=3` (set before the
//! first request reads it), as an operator retiring pre-contract clients
//! would run it. The default of 0 is covered by every other test binary,
//! whose requests all pass.

mod support;

use axum::http::{Method, StatusCode};
use serde_json::json;
use support::TestApp;

use atlas_team_backend::protocol::PROTOCOL;

fn raise_minimum() {
    // Every test sets the same value, so the once-read minimum is 3 whichever
    // test reads it first.
    std::env::set_var("ATLAS_MIN_CLIENT_PROTOCOL", "3");
}

#[tokio::test]
async fn protocol_endpoint_and_header_describe_the_server() {
    raise_minimum();
    let app = TestApp::new().await;
    let (status, header, body) = app
        .request_with_protocol(Method::GET, "/protocol", None, None, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["protocol"], PROTOCOL);
    assert_eq!(body["minClientProtocol"], 3);
    assert_eq!(header.as_deref(), Some(PROTOCOL.to_string().as_str()));

    let (status, _, ready) = app
        .request_with_protocol(Method::GET, "/ready", None, None, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ready["protocol"], PROTOCOL);
}

#[tokio::test]
async fn an_outdated_client_is_refused_before_it_changes_anything() {
    raise_minimum();
    let app = TestApp::new().await;
    let token = app.owner_token.clone();
    let path = format!("/companies/{}/invitations", app.company_id);
    let invite = json!({ "email": "late@example.com", "role": "sales" });

    for protocol in [None, Some("2")] {
        let (status, header, body) = app
            .request_with_protocol(Method::POST, &path, Some(&token), protocol, invite.clone())
            .await;
        assert_eq!(status, StatusCode::UPGRADE_REQUIRED, "{protocol:?}: {body}");
        assert!(body["error"].as_str().unwrap().contains("Update Atlas"));
        assert_eq!(body["minClientProtocol"], 3);
        assert_eq!(header.as_deref(), Some(PROTOCOL.to_string().as_str()));
    }
    // The refusal comes before any handler: even a bad token gets the
    // upgrade message, not 401, so nothing behind it ran.
    let (status, _, _) = app
        .request_with_protocol(
            Method::POST,
            &path,
            Some("not-a-token"),
            Some("2"),
            invite.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED);

    let (status, _, body) = app
        .request_with_protocol(
            Method::POST,
            &path,
            Some(&token),
            Some("banana"),
            invite.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A current client, and a newer one, go through.
    for protocol in ["3", "4"] {
        let (status, _, body) = app
            .request_with_protocol(
                Method::POST,
                &path,
                Some(&token),
                Some(protocol),
                invite.clone(),
            )
            .await;
        assert!(status.is_success(), "{protocol}: {status} {body}");
    }
}

#[tokio::test]
async fn surfaces_the_app_does_not_call_are_never_refused() {
    raise_minimum();
    let app = TestApp::new().await;
    for path in ["/health", "/ready", "/protocol"] {
        let (status, header, _) = app
            .request_with_protocol(Method::GET, path, None, None, json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(header.is_some(), "{path} carries the server protocol");
    }
}
