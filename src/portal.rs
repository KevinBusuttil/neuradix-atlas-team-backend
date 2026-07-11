//! Customer / accountant portal (served under `portal.atlas.neuradix.app`;
//! the paths are host-agnostic and live in the same binary).
//!
//! Two planes:
//!
//! * **Management** (`/companies/{id}/portal-links…`, existing bearer auth,
//!   owner/admin only): mint, list and revoke portal links. A link's token is
//!   generated and stored hashed exactly like the other tokens — but in its
//!   own table, so a portal token can never authenticate a member/device
//!   endpoint and member/device tokens never resolve on the portal plane.
//! * **Portal** (`/portal/{token}…`, the token in the path is the whole
//!   credential): a *customer* link is scoped strictly to its customer — a
//!   document whose `customer` payload field differs is a 404, never a 403
//!   leak; an *accountant* link is company-wide read-only (summary counts,
//!   the GL as CSV, the audit feed).
//!
//! Customer pages content-negotiate: `Accept: text/html` returns minimal
//! server-rendered pages (plain string templates, inline styles, no external
//! assets); JSON stays the default. Every interpolated value goes through
//! [`html_escape`] — document payloads are user data.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::{generate_token, hash_token, require_membership, require_role, AuthContext};
use crate::error::ApiError;
use crate::model::{
    AuditEntry, MutationRecord, MutationStatus, MutationType, PortalLink, PortalLinkKind, Role,
};
use crate::posting::replication::row_envelope;
use crate::projection::CompanyDocument;
use crate::AppState;

/// Device id stamped on portal-authored mutations (quotation accept/reject).
pub const PORTAL_DEVICE_ID: &str = "atlas-portal";

const DEFAULT_EXPIRES_DAYS: i64 = 90;
const DEFAULT_AUDIT_LIMIT: i64 = 100;
const MAX_AUDIT_LIMIT: i64 = 500;
const QUOTATION: &str = "Quotation";
const SALES_INVOICE: &str = "Sales Invoice";
/// Doctypes a customer link may read.
const CUSTOMER_DOCTYPES: [&str; 2] = [QUOTATION, SALES_INVOICE];

/// Management-plane routes (existing bearer auth, owner/admin).
pub fn management_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/companies/{company_id}/portal-links",
            post(create_link).get(list_links),
        )
        .route(
            "/companies/{company_id}/portal-links/{link_id}",
            delete(revoke_link),
        )
}

/// The public token-in-path portal plane. Registered separately so the
/// router can wrap exactly these routes in the public rate limiter.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/portal/{token}", get(portal_summary))
        .route(
            "/portal/{token}/documents/{doctype}/{document_id}",
            get(portal_document),
        )
        .route(
            "/portal/{token}/quotations/{quotation_id}/accept",
            post(quote_accept),
        )
        .route(
            "/portal/{token}/quotations/{quotation_id}/reject",
            post(quote_reject),
        )
        .route("/portal/{token}/gl.csv", get(portal_gl_csv))
        .route("/portal/{token}/audit", get(portal_audit))
}

// ---------------------------------------------------------------------------
// Management plane (owner/admin, existing bearer auth)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateLinkRequest {
    kind: PortalLinkKind,
    #[serde(default)]
    party: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default, alias = "expiresDays")]
    expires_days: Option<i64>,
}

async fn create_link(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(company_id): Path<Uuid>,
    Json(req): Json<CreateLinkRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let role = require_membership(&state, &auth, company_id).await?;
    require_role(role, &[Role::Owner, Role::Admin])?;
    let party = req
        .party
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if req.kind == PortalLinkKind::Customer && party.is_none() {
        return Err(ApiError::BadRequest(
            "party (the customer id) is required for customer links".into(),
        ));
    }
    let expires_days = req.expires_days.unwrap_or(DEFAULT_EXPIRES_DAYS);
    if expires_days < 1 {
        return Err(ApiError::BadRequest(
            "expires_days must be at least 1".into(),
        ));
    }
    let token = generate_token();
    let now = Utc::now();
    let link = PortalLink {
        id: Uuid::new_v4(),
        company_id,
        kind: req.kind,
        party: party.map(str::to_string),
        label: req
            .label
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string),
        token_hash: hash_token(&token),
        created_by: auth.user_id(),
        created_at: now,
        expires_at: now + Duration::days(expires_days),
        revoked_at: None,
    };
    state.store.create_portal_link(link.clone()).await?;
    audit(
        &state,
        company_id,
        Some(&auth),
        "portal.link.create",
        json!({ "linkId": link.id, "kind": link.kind, "party": link.party }),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": link.id,
            "token": token,
            "url_path": format!("/portal/{token}"),
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
    require_role(role, &[Role::Owner, Role::Admin])?;
    let links: Vec<Value> = state
        .store
        .portal_links(company_id)
        .await?
        .into_iter()
        .map(|link| {
            json!({
                "id": link.id,
                "kind": link.kind,
                "party": link.party,
                "label": link.label,
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
    require_role(role, &[Role::Owner, Role::Admin])?;
    if !state.store.revoke_portal_link(company_id, link_id).await? {
        return Err(ApiError::NotFound);
    }
    audit(
        &state,
        company_id,
        Some(&auth),
        "portal.link.revoke",
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
// Portal plane (the token in the path is the credential)
// ---------------------------------------------------------------------------

/// Resolves a portal token: unknown, revoked and expired all read as 404 —
/// the portal plane never confirms that a token once existed.
async fn resolve_link(state: &AppState, token: &str) -> Result<PortalLink, ApiError> {
    let link = state
        .store
        .portal_link_by_hash(&hash_token(token))
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

fn doc_customer(doc: &CompanyDocument) -> Option<&str> {
    doc.payload.get("customer").and_then(Value::as_str)
}

fn payload_num(doc: &CompanyDocument, field: &str) -> f64 {
    crate::posting::values::as_num(doc.payload.get(field))
}

fn payload_str<'a>(doc: &'a CompanyDocument, field: &str) -> Option<&'a str> {
    doc.payload
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The customer's open quotations: docstatus 0, scoped to the party.
async fn open_quotations(
    state: &AppState,
    company_id: Uuid,
    party: &str,
) -> Result<Vec<CompanyDocument>, ApiError> {
    Ok(state
        .store
        .company_documents(company_id, QUOTATION)
        .await?
        .into_iter()
        .filter(|doc| doc.docstatus == 0 && doc_customer(doc) == Some(party))
        .collect())
}

/// The customer's unpaid invoices: submitted Sales Invoices with
/// `outstanding_amount > 0`, scoped to the party.
async fn unpaid_invoices(
    state: &AppState,
    company_id: Uuid,
    party: &str,
) -> Result<Vec<CompanyDocument>, ApiError> {
    Ok(state
        .store
        .company_documents(company_id, SALES_INVOICE)
        .await?
        .into_iter()
        .filter(|doc| {
            doc.docstatus == 1
                && doc_customer(doc) == Some(party)
                && payload_num(doc, "outstanding_amount") > 0.0
        })
        .collect())
}

fn doc_json(doc: &CompanyDocument) -> Value {
    json!({
        "id": doc.document_id,
        "doctype": doc.doctype,
        "docstatus": doc.docstatus,
        "payload": doc.payload,
    })
}

async fn portal_summary(
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
    match link.kind {
        PortalLinkKind::Customer => {
            let party = link.party.clone().unwrap_or_default();
            let quotations = open_quotations(&state, link.company_id, &party).await?;
            let invoices = unpaid_invoices(&state, link.company_id, &party).await?;
            if wants_html(&headers) {
                return Ok(Html(customer_summary_page(
                    &token,
                    &company.name,
                    &party,
                    &quotations,
                    &invoices,
                ))
                .into_response());
            }
            Ok(Json(json!({
                "company": company.name,
                "customer": party,
                "quotations": quotations.iter().map(doc_json).collect::<Vec<_>>(),
                "invoices": invoices.iter().map(doc_json).collect::<Vec<_>>(),
            }))
            .into_response())
        }
        PortalLinkKind::Accountant => {
            let counts = state.store.posted_document_counts(link.company_id).await?;
            let gl_count = state.store.gl_entries_ordered(link.company_id).await?.len();
            let mut count_map = serde_json::Map::new();
            for (doctype, n) in counts {
                count_map.insert(doctype, json!(n));
            }
            Ok(Json(json!({
                "company": company.name,
                "documentCounts": count_map,
                "glEntryCount": gl_count,
            }))
            .into_response())
        }
    }
}

async fn portal_document(
    State(state): State<AppState>,
    Path((token, doctype, document_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let link = resolve_link(&state, &token).await?;
    if link.kind != PortalLinkKind::Customer || !CUSTOMER_DOCTYPES.contains(&doctype.as_str()) {
        return Err(ApiError::NotFound);
    }
    let party = link.party.clone().unwrap_or_default();
    let doc = state
        .store
        .company_document(link.company_id, &doctype, &document_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Strict customer scoping: someone else's document id is indistinguishable
    // from a nonexistent one.
    if doc_customer(&doc) != Some(party.as_str()) {
        return Err(ApiError::NotFound);
    }
    if wants_html(&headers) {
        return Ok(Html(document_page(&token, &doc)).into_response());
    }
    Ok(Json(json!({
        "id": doc.document_id,
        "doctype": doc.doctype,
        "docstatus": doc.docstatus,
        "payload": doc.payload,
        "children": doc.children,
    }))
    .into_response())
}

enum QuoteVerb {
    Accept,
    Reject,
}

impl QuoteVerb {
    /// (payload field this verb sets, the opposing field that conflicts,
    /// audit action, mutation-id suffix)
    fn spec(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            QuoteVerb::Accept => (
                "accepted_on",
                "rejected_on",
                "portal.quote.accept",
                "accept",
            ),
            QuoteVerb::Reject => (
                "rejected_on",
                "accepted_on",
                "portal.quote.reject",
                "reject",
            ),
        }
    }
}

async fn quote_accept(
    State(state): State<AppState>,
    Path((token, quotation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    quote_action(state, token, quotation_id, headers, QuoteVerb::Accept).await
}

async fn quote_reject(
    State(state): State<AppState>,
    Path((token, quotation_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    quote_action(state, token, quotation_id, headers, QuoteVerb::Reject).await
}

/// Accept or reject a quotation. Appends a system mutation (device id
/// [`PORTAL_DEVICE_ID`], `updateDocument` row envelope) stamping the decision
/// date onto the quotation payload, plus an audit row. Idempotent: a repeat of
/// the same verb returns 200 without a new mutation; the opposing verb after a
/// decision is a 409.
async fn quote_action(
    state: AppState,
    token: String,
    quotation_id: String,
    headers: HeaderMap,
    verb: QuoteVerb,
) -> Result<Response, ApiError> {
    let link = resolve_link(&state, &token).await?;
    if link.kind != PortalLinkKind::Customer {
        return Err(ApiError::NotFound);
    }
    let party = link.party.clone().unwrap_or_default();
    let doc = state
        .store
        .company_document(link.company_id, QUOTATION, &quotation_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if doc_customer(&doc) != Some(party.as_str()) {
        return Err(ApiError::NotFound);
    }
    let (set_field, conflict_field, audit_action, suffix) = verb.spec();
    if payload_str(&doc, conflict_field).is_some() {
        return Err(ApiError::Conflict(format!(
            "quotation {quotation_id} already has {conflict_field} set"
        )));
    }
    let date = match payload_str(&doc, set_field) {
        // Replay: already decided the same way — no new mutation, no audit.
        Some(existing) => existing.to_string(),
        None => {
            let now = Utc::now();
            let now_ms = now.timestamp_millis();
            let today = now.date_naive().to_string();
            let mut fields = match &doc.payload {
                Value::Object(map) => map.clone(),
                _ => serde_json::Map::new(),
            };
            fields.remove("items");
            fields.insert(set_field.into(), json!(today));
            let envelope = row_envelope(&quotation_id, QUOTATION, doc.docstatus, &fields, now_ms)
                .map_err(|e| ApiError::Internal(e.into()))?;
            let record = MutationRecord {
                id: format!("portal-{quotation_id}-{suffix}"),
                mutation_type: MutationType::UpdateDocument,
                doc_type: QUOTATION.to_string(),
                document_id: quotation_id.clone(),
                payload: envelope,
                device_id: PORTAL_DEVICE_ID.to_string(),
                user_id: String::new(),
                local_timestamp: now_ms,
                sync_version: None,
                status: MutationStatus::Pushed,
            };
            state
                .store
                .push_mutations(link.company_id, vec![record])
                .await?;
            state
                .store
                .append_audit(AuditEntry {
                    id: Uuid::new_v4(),
                    company_id: link.company_id,
                    user_id: None,
                    device_id: None,
                    action: audit_action.to_string(),
                    detail: json!({ "quotation": quotation_id, "customer": party }),
                    at: now,
                })
                .await?;
            today
        }
    };
    if wants_html(&headers) {
        // Back to the document page, which now renders the decided state.
        return Ok(Redirect::to(&format!(
            "/portal/{token}/documents/{QUOTATION}/{}",
            encode_segment(&quotation_id)
        ))
        .into_response());
    }
    Ok(Json(json!({ "quotation": quotation_id, set_field: date })).into_response())
}

// ---------------------------------------------------------------------------
// Accountant plane (read-only, JSON/CSV)
// ---------------------------------------------------------------------------

async fn portal_gl_csv(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let link = resolve_link(&state, &token).await?;
    if link.kind != PortalLinkKind::Accountant {
        return Err(ApiError::NotFound);
    }
    let entries = state.store.gl_entries_ordered(link.company_id).await?;
    let mut csv = String::from(
        "posting_date,voucher_type,voucher_no,account,debit,credit,party_type,party,is_reversal\n",
    );
    for entry in entries {
        let row = [
            csv_field(&entry.posting_date),
            csv_field(&entry.voucher_type),
            csv_field(&entry.voucher_no),
            csv_field(&entry.account),
            entry.debit.to_string(),
            entry.credit.to_string(),
            csv_field(entry.party_type.as_deref().unwrap_or("")),
            csv_field(entry.party.as_deref().unwrap_or("")),
            entry.is_reversal.to_string(),
        ];
        csv.push_str(&row.join(","));
        csv.push('\n');
    }
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        csv,
    )
        .into_response())
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

async fn portal_audit(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<Value>, ApiError> {
    let link = resolve_link(&state, &token).await?;
    if link.kind != PortalLinkKind::Accountant {
        return Err(ApiError::NotFound);
    }
    let limit = query
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);
    let entries = state.store.recent_audit(link.company_id, limit).await?;
    Ok(Json(json!({ "entries": entries })))
}

/// RFC 4180 field quoting for values that contain separators or quotes.
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Server-rendered pages (plain string templates, everything escaped)
// ---------------------------------------------------------------------------

/// Minimal HTML escaping for every interpolated value — document payloads are
/// user data.
pub fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Percent-encodes one path segment (document ids are user data too).
fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

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

fn doc_href(token: &str, doctype: &str, document_id: &str) -> String {
    format!(
        "/portal/{}/documents/{}/{}",
        encode_segment(token),
        encode_segment(doctype),
        encode_segment(document_id)
    )
}

fn doc_list_items(token: &str, docs: &[CompanyDocument], amount_field: &str) -> String {
    if docs.is_empty() {
        return "<p style=\"color:#666\">None.</p>".to_string();
    }
    let mut out = String::from("<ul>");
    for doc in docs {
        let amount = doc
            .payload
            .get(amount_field)
            .and_then(display_value)
            .map(|v| {
                format!(
                    " &mdash; {}: {}",
                    html_escape(amount_field),
                    html_escape(&v)
                )
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "<li><a href=\"{}\">{}</a>{}</li>",
            doc_href(token, &doc.doctype, &doc.document_id),
            html_escape(&doc.document_id),
            amount
        ));
    }
    out.push_str("</ul>");
    out
}

fn customer_summary_page(
    token: &str,
    company_name: &str,
    party: &str,
    quotations: &[CompanyDocument],
    invoices: &[CompanyDocument],
) -> String {
    let body = format!(
        "<h1>{}</h1><p>Customer: <strong>{}</strong></p>\
         <h2>Open quotations</h2>{}\
         <h2>Unpaid invoices</h2>{}",
        html_escape(company_name),
        html_escape(party),
        doc_list_items(token, quotations, "grand_total"),
        doc_list_items(token, invoices, "outstanding_amount"),
    );
    page(&format!("{company_name} — customer portal"), &body)
}

fn status_line(doc: &CompanyDocument) -> String {
    if let Some(date) = doc.payload.get("accepted_on").and_then(Value::as_str) {
        return format!(
            "<p><strong style=\"color:#166534\">Accepted on {}</strong></p>",
            html_escape(date)
        );
    }
    if let Some(date) = doc.payload.get("rejected_on").and_then(Value::as_str) {
        return format!(
            "<p><strong style=\"color:#991b1b\">Rejected on {}</strong></p>",
            html_escape(date)
        );
    }
    let status = match doc.docstatus {
        1 => "Submitted",
        2 => "Cancelled",
        _ => "Open",
    };
    format!("<p>Status: {status}</p>")
}

fn header_table(doc: &CompanyDocument) -> String {
    let Some(fields) = doc.payload.as_object() else {
        return String::new();
    };
    let mut rows = String::new();
    for (key, value) in fields {
        if key == "items" {
            continue;
        }
        if let Some(text) = display_value(value) {
            rows.push_str(&format!(
                "<tr><td style=\"padding:.25rem .75rem .25rem 0;color:#555\">{}</td>\
                 <td style=\"padding:.25rem 0\">{}</td></tr>",
                html_escape(key),
                html_escape(&text)
            ));
        }
    }
    format!("<table style=\"border-collapse:collapse\">{rows}</table>")
}

fn items_table(doc: &CompanyDocument) -> String {
    let items: Vec<Value> = doc
        .children
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| doc.payload.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    if items.is_empty() {
        return String::new();
    }
    // Column order: first appearance across the rows (scalar values only).
    let mut columns: Vec<String> = Vec::new();
    for item in &items {
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
    for item in &items {
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

fn totals_block(doc: &CompanyDocument) -> String {
    let mut out = String::new();
    for field in ["grand_total", "outstanding_amount"] {
        if let Some(text) = doc.payload.get(field).and_then(display_value) {
            out.push_str(&format!(
                "<p><strong>{}:</strong> {}</p>",
                html_escape(field),
                html_escape(&text)
            ));
        }
    }
    out
}

fn decision_forms(token: &str, doc: &CompanyDocument) -> String {
    let undecided = doc
        .payload
        .get("accepted_on")
        .and_then(Value::as_str)
        .is_none()
        && doc
            .payload
            .get("rejected_on")
            .and_then(Value::as_str)
            .is_none();
    if doc.doctype != QUOTATION || doc.docstatus != 0 || !undecided {
        return String::new();
    }
    let base = format!(
        "/portal/{}/quotations/{}",
        encode_segment(token),
        encode_segment(&doc.document_id)
    );
    let button = "padding:.5rem 1.25rem;border:none;border-radius:4px;color:#fff;cursor:pointer";
    format!(
        "<div style=\"margin-top:1.5rem\">\
         <form method=\"post\" action=\"{base}/accept\" style=\"display:inline\">\
         <button style=\"{button};background:#166534\">Accept quotation</button></form> \
         <form method=\"post\" action=\"{base}/reject\" style=\"display:inline\">\
         <button style=\"{button};background:#991b1b\">Reject quotation</button></form></div>"
    )
}

fn document_page(token: &str, doc: &CompanyDocument) -> String {
    let title = format!("{} {}", doc.doctype, doc.document_id);
    let body = format!(
        "<p><a href=\"/portal/{}\">&larr; Back to summary</a></p>\
         <h1>{} {}</h1>{}{}{}{}{}",
        encode_segment(token),
        html_escape(&doc.doctype),
        html_escape(&doc.document_id),
        status_line(doc),
        header_table(doc),
        items_table(doc),
        totals_block(doc),
        decision_forms(token, doc),
    );
    page(&title, &body)
}
