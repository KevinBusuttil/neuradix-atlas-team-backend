//! Invoice payment links (served under `pay.atlas.neuradix.app`; the paths
//! are host-agnostic and live in the same binary — domain doc §12).
//!
//! Two planes, mirroring the portal:
//!
//! * **Management** (`/companies/{id}/pay-links…`, existing bearer auth,
//!   owner/admin/sales/accountant): mint, list and revoke pay links. A link's
//!   token is generated and stored hashed exactly like the other tokens — but
//!   in its own table, so a pay token never authenticates any other plane and
//!   member/device/portal tokens never resolve here.
//! * **Pay page** (`/pay/{token}`, the token in the path is the whole
//!   credential): read-only view of exactly one submitted Sales Invoice —
//!   company name, invoice id + official number, posting date, line items,
//!   grand total, the **live** outstanding amount (from the posted document's
//!   payload, maintained by the posting engine) and the payment state.
//!
//! Card payments hand off **without any outbound HTTP**: when the company
//! settings carry `stripe_payment_link_url` (a Stripe Payment Link the owner
//! created in the Stripe dashboard), the HTML page renders a "Pay by card"
//! button linking there with `?client_reference_id={token}` appended — Stripe
//! echoes `client_reference_id` back in its `checkout.session.completed`
//! webhook, which is how the payment finds its way to the right invoice.
//! Without it, the page renders the `payment_instructions` settings text
//! (bank transfer details etc.) instead.
//!
//! Content negotiation matches the portal: `Accept: text/html` returns a
//! minimal server-rendered page (inline styles, no external assets, every
//! interpolated value HTML-escaped); JSON is the default.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::Sha256;
use uuid::Uuid;

use crate::auth::{generate_token, hash_token, require_membership, require_role, AuthContext};
use crate::error::ApiError;
use crate::model::{AuditEntry, PayLink, Role, WebhookEvent, WebhookKind};
use crate::portal::html_escape;
use crate::posting::engine::{submit_document as engine_submit, Actor, SubmitCommand};
use crate::posting::values::as_num;
use crate::AppState;

/// Pay links default to a 60-day life (invoices age faster than portals).
const DEFAULT_EXPIRES_DAYS: i64 = 60;
/// An invoice whose outstanding is below half a cent counts as paid (the
/// engine's own sub-cent tolerance).
pub const PAID_EPSILON: f64 = 0.005;
const SALES_INVOICE: &str = "Sales Invoice";
/// Roles that may manage pay links.
const LINK_ROLES: [Role; 4] = [Role::Owner, Role::Admin, Role::Sales, Role::Accountant];
/// Device id stamped on the mutations a webhook-posted Payment Entry
/// replicates onto the company log — the payments plane's system authorship,
/// alongside `atlas-backend` (posting replication) and `atlas-portal`
/// (portal quote decisions).
pub const PAYMENTS_DEVICE_ID: &str = "atlas-payments";
/// Maximum Stripe-Signature timestamp skew (Stripe's own recommended
/// tolerance): staler events are rejected as possible replays.
const MAX_SIGNATURE_SKEW_SECONDS: i64 = 300;

/// Management-plane routes (existing bearer auth, owner/admin/sales/accountant).
pub fn management_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/companies/{company_id}/pay-links",
            post(create_link).get(list_links),
        )
        .route(
            "/companies/{company_id}/pay-links/{link_id}",
            delete(revoke_link),
        )
}

/// The public token-in-path pay page. Registered separately so the router
/// can wrap exactly this route in the public rate limiter.
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/pay/{token}", get(pay_page))
}

/// Signature-verified Stripe processing; the path is frozen under the
/// /webhooks/... surface (domain doc §9) and takes precedence over the
/// generic log-only /webhooks/payments/{provider} intake route. Registered
/// separately so the router can wrap it in the per-provider webhook rate
/// limiter alongside the generic intake routes.
pub fn webhook_routes() -> Router<AppState> {
    Router::new().route("/webhooks/payments/stripe", post(stripe_webhook))
}

// ---------------------------------------------------------------------------
// Management plane (owner/admin/sales/accountant, existing bearer auth)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateLinkRequest {
    #[serde(alias = "invoiceId")]
    invoice_id: String,
    #[serde(default, alias = "expiresDays")]
    expires_days: Option<i64>,
}

/// The invoice a pay link (or a webhook payment) targets: the officially
/// posted document when it exists, else the read-model row a client synced.
/// Only submitted Sales Invoices qualify.
async fn resolve_invoice(
    state: &AppState,
    company_id: Uuid,
    invoice_id: &str,
) -> Result<Option<InvoiceView>, ApiError> {
    if let Some(doc) = state
        .store
        .posted_document(company_id, SALES_INVOICE, invoice_id)
        .await?
    {
        if doc.docstatus != 1 {
            return Ok(None);
        }
        let payload = doc.payload.clone();
        return Ok(Some(InvoiceView {
            invoice_id: doc.id,
            official_number: doc.official_number,
            posting_date: payload
                .get("posting_date")
                .and_then(Value::as_str)
                .map(str::to_string),
            customer: payload
                .get("customer")
                .and_then(Value::as_str)
                .map(str::to_string),
            grand_total: as_num(payload.get("grand_total")),
            outstanding: as_num(payload.get("outstanding_amount")),
            items: payload
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }));
    }
    // Read-model fallback: an invoice submitted on the sync plane before the
    // company adopted backend postings.
    let Some(doc) = state
        .store
        .company_document(company_id, SALES_INVOICE, invoice_id)
        .await?
    else {
        return Ok(None);
    };
    if doc.docstatus != 1 {
        return Ok(None);
    }
    Ok(Some(InvoiceView {
        invoice_id: doc.document_id,
        official_number: doc
            .payload
            .get("official_number")
            .and_then(Value::as_str)
            .map(str::to_string),
        posting_date: doc
            .payload
            .get("posting_date")
            .and_then(Value::as_str)
            .map(str::to_string),
        customer: doc
            .payload
            .get("customer")
            .and_then(Value::as_str)
            .map(str::to_string),
        grand_total: as_num(doc.payload.get("grand_total")),
        outstanding: as_num(doc.payload.get("outstanding_amount")),
        items: doc
            .children
            .as_ref()
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| doc.payload.get("items").and_then(Value::as_array).cloned())
            .unwrap_or_default(),
    }))
}

async fn create_link(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<CreateLinkRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &LINK_ROLES)?;
    let invoice_id = req.invoice_id.trim();
    if invoice_id.is_empty() {
        return Err(ApiError::BadRequest("invoice_id is required".into()));
    }
    // The invoice must exist as a submitted Sales Invoice.
    if resolve_invoice(&state, company_id, invoice_id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound);
    }
    let expires_days = req.expires_days.unwrap_or(DEFAULT_EXPIRES_DAYS);
    if expires_days < 1 {
        return Err(ApiError::BadRequest(
            "expires_days must be at least 1".into(),
        ));
    }
    let token = generate_token();
    let now = Utc::now();
    let link = PayLink {
        id: Uuid::new_v4(),
        company_id,
        invoice_id: invoice_id.to_string(),
        token_hash: hash_token(&token),
        created_by: auth.user_id(),
        created_at: now,
        expires_at: now + Duration::days(expires_days),
        revoked_at: None,
    };
    state.store.create_pay_link(link.clone()).await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "pay.link.create",
        json!({ "linkId": link.id, "invoiceId": link.invoice_id }),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": link.id,
            "token": token,
            "url_path": format!("/pay/{token}"),
            "expiresAt": link.expires_at,
        })),
    ))
}

async fn list_links(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &LINK_ROLES)?;
    let links: Vec<Value> = state
        .store
        .pay_links(company_id)
        .await?
        .into_iter()
        .map(|link| {
            json!({
                "id": link.id,
                "invoiceId": link.invoice_id,
                "createdAt": link.created_at,
                "expiresAt": link.expires_at,
                "revoked": link.revoked_at.is_some(),
                "revokedAt": link.revoked_at,
            })
        })
        .collect();
    Ok(Json(json!({ "links": links })))
}

async fn revoke_link(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((company_id, link_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &LINK_ROLES)?;
    if !state.store.revoke_pay_link(company_id, link_id).await? {
        return Err(ApiError::NotFound);
    }
    audit(
        &state,
        company_id,
        Some(&auth),
        "pay.link.revoke",
        json!({ "linkId": link_id }),
    )
    .await?;
    Ok(Json(json!({ "revoked": true })))
}

async fn audit(
    state: &AppState,
    company_id: Uuid,
    auth: Option<&AuthContext>,
    action: &str,
    detail: Value,
) -> Result<(), ApiError> {
    state
        .store
        .append_audit(AuditEntry {
            id: Uuid::new_v4(),
            company_id,
            user_id: auth.map(|a| a.user_id()),
            device_id: auth.and_then(|a| a.device_id()),
            action: action.to_string(),
            detail,
            at: Utc::now(),
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pay page (the token in the path is the credential)
// ---------------------------------------------------------------------------

/// What the pay page (and the webhook processor) needs to know about the
/// invoice a link targets.
pub(crate) struct InvoiceView {
    pub invoice_id: String,
    pub official_number: Option<String>,
    pub posting_date: Option<String>,
    /// The invoice's customer — the party a webhook-posted Payment Entry is
    /// received from.
    pub customer: Option<String>,
    pub grand_total: f64,
    /// Live outstanding, from the posted document's payload (the posting
    /// engine maintains it on every settlement).
    pub outstanding: f64,
    pub items: Vec<Value>,
}

impl InvoiceView {
    pub fn paid(&self) -> bool {
        self.outstanding.abs() < PAID_EPSILON
    }
}

/// Resolves a pay token: unknown, revoked and expired all read as 404 — the
/// pay plane never confirms that a token once existed.
pub(crate) async fn resolve_link(state: &AppState, token: &str) -> Result<PayLink, ApiError> {
    let link = state
        .store
        .pay_link_by_hash(&hash_token(token))
        .await?
        .ok_or(ApiError::NotFound)?;
    if link.revoked_at.is_some() || link.expires_at < Utc::now() {
        return Err(ApiError::NotFound);
    }
    Ok(link)
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.contains("text/html"))
        .unwrap_or(false)
}

/// The Stripe Payment Link handoff URL for one pay token:
/// `client_reference_id` ties the eventual checkout webhook back to the link,
/// `prefilled_email` is left blank for the customer to fill.
fn card_payment_url(stripe_payment_link_url: &str, token: &str) -> String {
    format!("{stripe_payment_link_url}?client_reference_id={token}&prefilled_email=")
}

async fn pay_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let link = resolve_link(&state, &token).await?;
    let company = state
        .store
        .company(link.company_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let invoice = resolve_invoice(&state, link.company_id, &link.invoice_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let settings = state.store.company_settings(link.company_id).await?;
    let card_url = settings
        .stripe_payment_link_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| card_payment_url(url, &token));
    let instructions = settings
        .payment_instructions
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);

    if wants_html(&headers) {
        return Ok(Html(invoice_page(
            &company.name,
            &invoice,
            card_url.as_deref(),
            instructions.as_deref(),
        ))
        .into_response());
    }
    Ok(Json(json!({
        "company": company.name,
        "invoice_id": invoice.invoice_id,
        "official_number": invoice.official_number,
        "posting_date": invoice.posting_date,
        "customer": invoice.customer,
        "items": invoice.items,
        "grand_total": invoice.grand_total,
        "outstanding_amount": invoice.outstanding,
        "paid": invoice.paid(),
        "card_payment_url": card_url,
        "payment_instructions": instructions,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Stripe webhook processing (POST /webhooks/payments/stripe)
// ---------------------------------------------------------------------------

/// Event types that carry a completed checkout with `client_reference_id` +
/// `amount_total` in `data.object`.
const CHECKOUT_EVENT_TYPES: [&str; 2] = ["checkout.session.completed", "payment_link.completed"];

/// Verifies a `Stripe-Signature: t=...,v1=...` header: `v1` is the
/// hex-encoded HMAC-SHA256 of `{t}.{raw body}` under the webhook secret.
/// Comparison is constant-time ([`Mac::verify_slice`]); timestamps more than
/// [`MAX_SIGNATURE_SKEW_SECONDS`] from `now` are rejected as stale.
fn verify_stripe_signature(
    secret: &str,
    header: &str,
    body: &[u8],
    now: i64,
) -> Result<(), ApiError> {
    let mut timestamp: Option<i64> = None;
    let mut candidates: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = value.trim().parse().ok(),
            Some(("v1", value)) => {
                if let Ok(signature) = hex::decode(value.trim()) {
                    candidates.push(signature);
                }
            }
            _ => {}
        }
    }
    let t = timestamp.ok_or_else(|| {
        ApiError::BadRequest("malformed Stripe-Signature header: missing timestamp".into())
    })?;
    if (now - t).abs() > MAX_SIGNATURE_SKEW_SECONDS {
        return Err(ApiError::BadRequest(
            "stale Stripe-Signature timestamp".into(),
        ));
    }
    for candidate in &candidates {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(t.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        if mac.verify_slice(candidate).is_ok() {
            return Ok(());
        }
    }
    Err(ApiError::BadRequest("invalid webhook signature".into()))
}

/// A 200 the provider will not retry, explaining why nothing was posted.
fn unhandled(reason: &str) -> Json<Value> {
    Json(json!({ "handled": false, "reason": reason }))
}

/// Signature-verified Stripe webhook processing. Every delivery is
/// intake-logged first (`webhook_events` stays the durable inbox); with no
/// configured secret the endpoint fails **closed** (503, nothing processed).
/// A verified `checkout.session.completed` resolves its `client_reference_id`
/// pay token to a company + invoice and posts an official Payment Entry
/// through the posting engine — settlement, outstanding maintenance, gap-free
/// numbering and device replication all come from the same
/// `Store::posting_commit` path the command API uses. Idempotency key
/// `stripe-{event id}` makes event redeliveries replays, never double
/// postings. Business-level rejections (unknown/expired token, missing
/// invoice, already-paid invoice) are 200 `handled:false` so Stripe does not
/// retry forever; each is audit-logged when a company is attributable.
async fn stripe_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    // Durable intake first, exactly like the generic log-only route — and
    // behind the same backlog cap (Stripe retries refused deliveries with
    // backoff, so a 429 defers the event instead of losing it).
    crate::api::check_webhook_backlog(&state, WebhookKind::Payment, "stripe").await?;
    state
        .store
        .insert_webhook_event(WebhookEvent {
            id: Uuid::new_v4(),
            kind: WebhookKind::Payment,
            provider: "stripe".into(),
            headers: crate::api::headers_to_json(&headers),
            body: body.to_vec(),
            received_at: Utc::now(),
        })
        .await?;

    // Fail closed: without a secret nothing can be verified, and unverified
    // events are never processed.
    let Some(secret) = state.config.stripe_webhook_secret.as_deref() else {
        tracing::error!(
            "STRIPE_WEBHOOK_SECRET is not configured; refusing to process the Stripe event"
        );
        return Err(ApiError::Unavailable(
            "webhook secret not configured".into(),
        ));
    };
    let signature = headers
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest("missing Stripe-Signature header".into()))?;
    verify_stripe_signature(secret, signature, &body, Utc::now().timestamp())?;

    let event: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("webhook body is not valid JSON".into()))?;
    let event_type = event["type"].as_str().unwrap_or_default();
    if !CHECKOUT_EVENT_TYPES.contains(&event_type) {
        return Ok(unhandled("ignored event type"));
    }
    let Some(event_id) = event["id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(unhandled("missing event id"));
    };
    let object = &event["data"]["object"];
    let Some(token) = object["client_reference_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(unhandled("missing client_reference_id"));
    };
    let Some(amount_cents) = object["amount_total"].as_i64().filter(|cents| *cents > 0) else {
        return Ok(unhandled("missing or non-positive amount_total"));
    };
    let currency = object["currency"].as_str().unwrap_or_default().to_string();
    let paid_amount = amount_cents as f64 / 100.0;

    // Resolve the pay token. An unknown token has no company to audit
    // against; expired/revoked ones do.
    let Some(link) = state.store.pay_link_by_hash(&hash_token(token)).await? else {
        return Ok(unhandled("unknown payment link"));
    };
    let company_id = link.company_id;
    let webhook_audit = |reason: &str| {
        json!({
            "eventId": event_id,
            "invoiceId": link.invoice_id,
            "amount": paid_amount,
            "currency": currency,
            "reason": reason,
        })
    };
    if link.revoked_at.is_some() || link.expires_at < Utc::now() {
        let reason = "expired or revoked payment link";
        audit(
            &state,
            company_id,
            None,
            "pay.webhook.rejected",
            webhook_audit(reason),
        )
        .await?;
        return Ok(unhandled(reason));
    }

    // Only an officially posted, submitted Sales Invoice can take an official
    // Payment Entry (the engine validates references against posted
    // documents).
    let invoice = state
        .store
        .posted_document(company_id, SALES_INVOICE, &link.invoice_id)
        .await?
        .filter(|doc| doc.docstatus == 1);
    let Some(invoice) = invoice else {
        let reason = "invoice is not an officially posted Sales Invoice";
        audit(
            &state,
            company_id,
            None,
            "pay.webhook.rejected",
            webhook_audit(reason),
        )
        .await?;
        return Ok(unhandled(reason));
    };
    let outstanding = as_num(invoice.payload.get("outstanding_amount"));
    let idempotency_key = format!("stripe-{event_id}");
    // A redelivered event replays through the idempotency key even though the
    // invoice it settled now reads as paid; only genuinely new events are
    // rejected on a zero outstanding.
    let is_replay = state
        .store
        .idempotent_response(company_id, &idempotency_key)
        .await?
        .is_some();
    if !is_replay && outstanding < PAID_EPSILON {
        let reason = "invoice has no outstanding amount";
        audit(
            &state,
            company_id,
            None,
            "pay.webhook.rejected",
            webhook_audit(reason),
        )
        .await?;
        return Ok(unhandled(reason));
    }

    // Post the official Payment Entry exactly like the command API does; the
    // posting engine + replication handle settlement, outstanding update and
    // device replication from here.
    let mut payload = Map::new();
    payload.insert("payment_type".into(), json!("Receive"));
    if let Some(party) = invoice.payload.get("customer").and_then(Value::as_str) {
        payload.insert("party".into(), json!(party));
    }
    payload.insert("paid_amount".into(), json!(paid_amount));
    if !currency.is_empty() {
        payload.insert("currency".into(), json!(currency));
    }
    if let Some(date) = event["created"]
        .as_i64()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
    {
        payload.insert(
            "posting_date".into(),
            json!(date.format("%Y-%m-%d").to_string()),
        );
    }
    payload.insert(
        "references".into(),
        json!([{
            "reference_doctype": SALES_INVOICE,
            "reference_name": link.invoice_id,
            "allocated_amount": paid_amount.min(outstanding),
        }]),
    );
    let outcome = engine_submit(
        state.store.as_ref(),
        company_id,
        SubmitCommand {
            doctype: "Payment Entry".into(),
            document_id: Some(format!("PAY-STRIPE-{event_id}")),
            payload,
            items: Vec::new(),
            idempotency_key: Some(idempotency_key),
        },
        Actor::system(PAYMENTS_DEVICE_ID),
    )
    .await?;

    audit(
        &state,
        company_id,
        None,
        "pay.webhook.payment",
        json!({
            "eventId": event_id,
            "invoiceId": link.invoice_id,
            "documentId": outcome.response["document_id"],
            "number": outcome.response["number"],
            "amount": paid_amount,
            "currency": currency,
            "replayed": outcome.replayed,
        }),
    )
    .await?;
    Ok(Json(json!({
        "handled": true,
        "document_id": outcome.response["document_id"],
        "number": outcome.response["number"],
        "replayed": outcome.replayed,
    })))
}

// ---------------------------------------------------------------------------
// Server-rendered page (plain string template, everything escaped — the
// portal's renderer pattern)
// ---------------------------------------------------------------------------

/// A scalar payload value as display text; objects/arrays are not rendered.
fn display_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:720px;\
         margin:2rem auto;padding:0 1rem;color:#1a1a1a;line-height:1.5\">{}</body></html>",
        html_escape(title),
        body
    )
}

fn items_table(items: &[Value]) -> String {
    if items.is_empty() {
        return String::new();
    }
    // Column order: first appearance across the rows (scalar values only).
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Some(map) = item.as_object() {
            for (key, value) in map {
                if display_value(value).is_some() && !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let cell = "padding:.35rem .75rem;border:1px solid #ddd;text-align:left";
    let mut html = String::from(
        "<h2>Line items</h2><table style=\"border-collapse:collapse;width:100%\"><tr>",
    );
    for column in &columns {
        html.push_str(&format!(
            "<th style=\"{cell};background:#f5f5f5\">{}</th>",
            html_escape(column)
        ));
    }
    html.push_str("</tr>");
    for item in items {
        html.push_str("<tr>");
        for column in &columns {
            let text = item.get(column).and_then(display_value).unwrap_or_default();
            html.push_str(&format!("<td style=\"{cell}\">{}</td>", html_escape(&text)));
        }
        html.push_str("</tr>");
    }
    html.push_str("</table>");
    html
}

/// The payment block: paid state, card button (Stripe Payment Link handoff)
/// or the company's manual payment instructions.
fn payment_block(
    invoice: &InvoiceView,
    card_url: Option<&str>,
    instructions: Option<&str>,
) -> String {
    if invoice.paid() {
        return "<p><strong style=\"color:#166534\">Paid &mdash; thank you</strong></p>"
            .to_string();
    }
    let mut out = format!(
        "<p>Status: <strong style=\"color:#92400e\">Awaiting payment</strong> \
         &mdash; outstanding {}</p>",
        html_escape(&invoice.outstanding.to_string())
    );
    if let Some(url) = card_url {
        out.push_str(&format!(
            "<p><a href=\"{}\" style=\"display:inline-block;padding:.6rem 1.5rem;\
             border-radius:4px;background:#1d4ed8;color:#fff;text-decoration:none\">\
             Pay by card</a></p>",
            html_escape(url)
        ));
    } else if let Some(text) = instructions {
        out.push_str(&format!(
            "<h2>Payment instructions</h2>\
             <p style=\"white-space:pre-wrap;background:#f5f5f5;padding:1rem;\
             border-radius:4px\">{}</p>",
            html_escape(text)
        ));
    }
    out
}

fn invoice_page(
    company_name: &str,
    invoice: &InvoiceView,
    card_url: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let official = invoice
        .official_number
        .as_deref()
        .map(|number| {
            format!(
                " <span style=\"color:#555\">({})</span>",
                html_escape(number)
            )
        })
        .unwrap_or_default();
    let posting_date = invoice
        .posting_date
        .as_deref()
        .map(|date| format!("<p>Posting date: {}</p>", html_escape(date)))
        .unwrap_or_default();
    let body = format!(
        "<h1>{}</h1><h2>Invoice {}{}</h2>{}{}\
         <p><strong>Grand total:</strong> {}</p>\
         <p><strong>Outstanding:</strong> {}</p>{}",
        html_escape(company_name),
        html_escape(&invoice.invoice_id),
        official,
        posting_date,
        items_table(&invoice.items),
        html_escape(&invoice.grand_total.to_string()),
        html_escape(&invoice.outstanding.to_string()),
        payment_block(invoice, card_url, instructions),
    );
    page(
        &format!("{company_name} — invoice {}", invoice.invoice_id),
        &body,
    )
}
