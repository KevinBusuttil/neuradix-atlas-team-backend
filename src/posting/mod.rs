//! Phase 3 — backend-authoritative posting engine (Track B of
//! `docs/STOCK_COGS_IMPLEMENTATION_PLAN.md`).
//!
//! The accounting semantics are a faithful port of the Dart Solo engine
//! (`mercantis.hub.flutter/lib/ledger/ledger_derivation.dart`,
//! `ledger_derivation_service.dart`, `stock_costing.dart`, `stock_balance.dart`)
//! so both engines pass the same language-neutral fixture suite
//! (`tests/fixtures/*.json`). Deterministic row ids mirror the Dart
//! conventions: `GL-{id}-debit`, `SLE-{id}-{i}`, `{sleId}-gl-d`/`-gl-c`,
//! and a `-reversal` suffix on cancel.

pub mod engine;
pub mod model;
pub mod stock;
pub mod values;
