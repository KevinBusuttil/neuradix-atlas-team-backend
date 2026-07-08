//! Value-coercion helpers over JSON payloads, ported from the Dart
//! `ledger_values.dart` so both engines read loosely-typed payload fields the
//! same way (numbers may arrive as numbers or strings, flags as bool/int/"1").

use serde_json::Value;

/// Coerces a payload value to a number; 0 for null/garbage so ledger math
/// never fails on a missing field (Dart `asNum`).
pub fn as_num(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// A trimmed non-empty string, or None (Dart `asNonEmpty`).
pub fn as_non_empty(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        _ => None,
    }
}

/// Truthiness for a payload flag stored as a bool, int, or string "1"
/// (Dart `isTrue`).
pub fn is_true(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() == Some(1.0),
        Some(Value::String(s)) => s == "1",
        _ => false,
    }
}

/// Half-away-from-zero rounding matching the Dart/Swift `round2`/`round3`.
pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// `-reversal` id suffix pairing reversal rows with their originals.
pub const REVERSAL_SUFFIX: &str = "-reversal";

/// Whether an item participates in inventory: `item_type` "Service" (case
/// insensitive) never moves stock; anything else — including unknown items —
/// defaults to a stock item, matching the Dart `isStockItem`.
pub fn is_stock_item_type(item_type: Option<&str>) -> bool {
    match item_type {
        Some(t) => !t.trim().eq_ignore_ascii_case("service"),
        None => true,
    }
}

/// The transaction→stock UOM conversion factor for an item — 1.0 when the
/// line UOM is the item's stock UOM (or unknown/missing/unregistered). Reads
/// `Item.uoms` for a matching row with a positive `conversion_factor`,
/// mirroring the Dart `uomFactor` so both engines agree on how much inventory
/// a line really moves.
pub fn uom_factor(item: Option<&crate::posting::model::Item>, line_uom: Option<&str>) -> f64 {
    let (Some(item), Some(line_uom)) = (item, line_uom) else {
        return 1.0;
    };
    let Some(stock_uom) = item.stock_uom.as_deref() else {
        return 1.0;
    };
    if stock_uom == line_uom {
        return 1.0;
    }
    for row in &item.uoms {
        if row.uom == line_uom && row.conversion_factor > 0.0 {
            return row.conversion_factor;
        }
    }
    1.0
}

/// An invoice's outstanding balance: grand total less everything settled
/// against it. Allocations are already signed (reversals negative), so a
/// plain sum nets cancellations correctly (Dart `outstandingAmount`).
pub fn outstanding_amount(grand_total: f64, settled: impl IntoIterator<Item = f64>) -> f64 {
    grand_total - settled.into_iter().sum::<f64>()
}
