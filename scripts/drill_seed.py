#!/usr/bin/env python3
"""Seed a realistic Team company through the public API, for restore drills
and smoke tests. Stdlib only. Prints JSON {company_id, device_token, ...}.

    python3 scripts/drill_seed.py http://127.0.0.1:8080 [bootstrap-token]

Creates: a company with owner + purchasing/sales/stock/pos/accountant
members and devices; a stock item; a purchase receipt, purchase invoice,
sales invoice, partial payment, delivery note, an offline-confirmed till
sale, a cancelled document; synced masters/drafts; and an attachment blob.
Every official action goes through the command API exactly as the clients
do, so the resulting database exercises every table the drill verifies.
"""
import hashlib
import json
import sys
import urllib.error
import urllib.request
import uuid

BASE = sys.argv[1].rstrip("/")
BOOTSTRAP = sys.argv[2] if len(sys.argv) > 2 else None
RUN = uuid.uuid4().hex[:8]


def call(method, path, token=None, body=None, raw=None, headers=None, expect=(200, 201)):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("Content-Type", "application/octet-stream" if raw is not None else "application/json")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req) as resp:
            status, text = resp.status, resp.read().decode()
    except urllib.error.HTTPError as err:
        status, text = err.code, err.read().decode()
    if status not in expect:
        raise SystemExit(f"{method} {path} -> {status}: {text}")
    return json.loads(text) if text else None


boot = call("POST", "/companies", body={
    "name": f"Drill Trading {RUN}", "owner_email": f"owner-{RUN}@example.com",
    "owner_name": "Drill Owner"}, headers={"X-Atlas-Bootstrap-Token": BOOTSTRAP} if BOOTSTRAP else None)
company, owner = boot["company"]["id"], boot["token"]
C = f"/companies/{company}"

devices = {"owner": call("POST", f"{C}/devices", owner, {"name": "owner laptop"})["deviceToken"]}
for role in ["purchasing", "sales", "stock", "pos", "accountant"]:
    inv = call("POST", f"{C}/invitations", owner, {"email": f"{role}-{RUN}@example.com", "role": role})
    user = call("POST", f"/invitations/{inv['token']}/accept", body={"display_name": role})
    devices[role] = call("POST", f"{C}/devices", user["token"], {"name": f"{role} device"})["deviceToken"]

call("POST", f"{C}/items", owner, {"id": "WIDGET", "item_type": "Stock", "valuation_method": "FIFO"})


def submit(role, doctype, doc_id, payload, items=(), **extra):
    body = {"doctype": doctype, "document_id": doc_id, "payload": payload,
            "items": list(items), "idempotency_key": f"submit-{doctype}-{doc_id}", **extra}
    return call("POST", f"{C}/commands/submit-document", devices[role], body)


day = "2026-07-01"
submit("purchasing", "Purchase Receipt", "PREC-1",
       {"supplier": "SUP-1", "posting_date": day, "set_warehouse": "Main"},
       [{"item": "WIDGET", "qty": 20, "rate": 5}])
submit("purchasing", "Purchase Invoice", "PINV-1",
       {"supplier": "SUP-1", "posting_date": day, "payable_account": "Creditors",
        "expense_account": "Purchases"},
       [{"item": "SERVICE", "qty": 1, "rate": 40, "amount": 40}])
submit("sales", "Sales Invoice", "SINV-1",
       {"customer": "CUST-1", "posting_date": day, "receivable_account": "Debtors",
        "income_account": "Sales",
        "taxes": [{"tax_type": "VAT", "tax_code": "VAT-18", "tax_account": "VAT Output",
                   "taxable_amount": 120, "tax_amount": 21.6, "rate": 18}]},
       [{"item": "WIDGET", "qty": 10, "rate": 12, "amount": 120}])
submit("accountant", "Payment Entry", "PAY-1",
       {"payment_type": "Receive", "party_type": "Customer", "party": "CUST-1",
        "posting_date": day, "paid_amount": 100, "paid_from": "Debtors", "paid_to": "Bank",
        "references": [{"reference_doctype": "Sales Invoice", "reference_name": "SINV-1",
                        "allocated_amount": 100}]})
submit("stock", "Delivery Note", "DN-1",
       {"customer": "CUST-1", "posting_date": day, "set_warehouse": "Main"},
       [{"item": "WIDGET", "qty": 10, "rate": 12}])
submit("pos", "POS Invoice", "POS-TILL1-0001",
       {"customer": "WALK-IN", "posting_date": day, "set_warehouse": "Main", "till_series": "TILL1",
        "tenders": [{"mode_of_payment": "Cash", "amount": 24}]},
       [{"item": "WIDGET", "qty": 2, "rate": 12}], offline_fiscal=True)
submit("stock", "Stock Entry", "STE-1",
       {"stock_entry_type": "Material Receipt", "posting_date": day},
       [{"item": "WIDGET", "qty": 1, "valuation_rate": 5, "target_warehouse": "Main"}])
call("POST", f"{C}/commands/cancel-document", devices["stock"],
     {"doctype": "Stock Entry", "document_id": "STE-1", "idempotency_key": "cancel-Stock Entry-STE-1"})

mutations = []
for i in range(1, 6):
    cid = f"CUST-SEED-{i}"
    mutations.append({
        "id": f"seed-{RUN}-{i}", "type": "createDocument", "docType": "Customer", "documentId": cid,
        "payload": {"id": cid, "doctype": "Customer", "docstatus": 0,
                    "payload": json.dumps({"customer_name": f"Seed Customer {i}"})},
        "deviceId": "drill-device", "userId": "drill", "localTimestamp": 1, "status": "pending"})
call("POST", f"{C}/sync/push", devices["sales"], {"mutations": mutations})

blob = f"drill attachment {RUN}".encode()
call("PUT", f"{C}/blobs/{hashlib.sha256(blob).hexdigest()}", devices["owner"], raw=blob)

print(json.dumps({"company_id": company, "device_token": devices["owner"], "owner_token": owner}))
