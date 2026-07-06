//! Pure stock-balance fold and cost-of-issue calculation, ported from the
//! Dart `stock_balance.dart` / `stock_costing.dart` so both engines value
//! inventory identically.

use crate::posting::values::{round2, round3};

/// One prior stock-ledger movement, already in chronological order.
#[derive(Debug, Clone, Copy)]
pub struct LedgerRow {
    pub qty_change: f64,
    pub valuation_rate: f64,
}

/// The recomputed running balance for one (item, warehouse) pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BinSnapshot {
    pub actual_qty: f64,
    pub valuation_rate: f64,
    pub stock_value: f64,
}

/// Folds the FULL set of stock-ledger rows for a pair — including reversal
/// rows, whose negated `qty_change` nets naturally — into the Bin snapshot
/// (Dart `StockBalance.compute`).
pub fn compute_balance(rows: &[LedgerRow]) -> BinSnapshot {
    let mut qty = 0.0;
    let mut value = 0.0;
    for row in rows {
        qty += row.qty_change;
        value += row.qty_change * row.valuation_rate;
    }
    let actual_qty = round3(qty);
    let stock_value = round2(value);
    let valuation_rate = if actual_qty != 0.0 {
        round2(stock_value / actual_qty)
    } else {
        0.0
    };
    BinSnapshot {
        actual_qty,
        valuation_rate,
        stock_value,
    }
}

pub const MOVING_AVERAGE: &str = "Moving Average";
pub const FIFO: &str = "FIFO";

/// The cost per stock unit to issue `qty` units, given the chronological
/// `prior` ledger rows for the (item, warehouse) and the item's valuation
/// method (defaults to moving average). For FIFO, any shortfall beyond the
/// available receipt layers is costed at the moving-average rate so the
/// issue is never valued at zero (Dart `StockCosting.issueRate`).
pub fn issue_rate(prior: &[LedgerRow], qty: f64, method: Option<&str>) -> f64 {
    if qty <= 0.0 {
        return 0.0;
    }
    if method == Some(FIFO) {
        return fifo_rate(prior, qty);
    }
    compute_balance(prior).valuation_rate
}

/// Weighted-average unit cost of the `qty` units FIFO would consume next,
/// after replaying `prior` into its remaining receipt layers.
fn fifo_rate(prior: &[LedgerRow], qty: f64) -> f64 {
    let mut layers = remaining_layers(prior);
    let mut need = qty;
    let mut cost = 0.0;
    let mut taken = 0.0;
    let mut i = 0;
    while need > 0.0 && i < layers.len() {
        let layer = &mut layers[i];
        let take = if layer.qty <= need { layer.qty } else { need };
        cost += take * layer.rate;
        taken += take;
        need -= take;
        if layer.qty <= take {
            i += 1;
        } else {
            layer.qty -= take;
        }
    }
    if need > 0.0 {
        // Issuing more than is on hand: cost the shortfall at the moving
        // average (or 0 when there is no history) rather than leaving it free.
        let fallback = compute_balance(prior).valuation_rate;
        cost += need * fallback;
        taken += need;
    }
    if taken > 0.0 {
        cost / taken
    } else {
        0.0
    }
}

struct Layer {
    qty: f64,
    rate: f64,
}

/// Folds `prior` into the FIFO layers still on hand, oldest first. Receipts
/// push a layer at their valuation rate; issues consume from the oldest.
fn remaining_layers(prior: &[LedgerRow]) -> Vec<Layer> {
    let mut layers: Vec<Layer> = Vec::new();
    for row in prior {
        let q = row.qty_change;
        if q > 0.0 {
            layers.push(Layer {
                qty: q,
                rate: row.valuation_rate,
            });
        } else if q < 0.0 {
            let mut consume = -q;
            while consume > 0.0 && !layers.is_empty() {
                if layers[0].qty <= consume {
                    consume -= layers[0].qty;
                    layers.remove(0);
                } else {
                    layers[0].qty -= consume;
                    consume = 0.0;
                }
            }
        }
    }
    layers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(qty_change: f64, valuation_rate: f64) -> LedgerRow {
        LedgerRow {
            qty_change,
            valuation_rate,
        }
    }

    #[test]
    fn moving_average_is_value_over_qty() {
        let prior = [row(10.0, 5.0), row(10.0, 7.0)];
        assert_eq!(issue_rate(&prior, 4.0, None), 6.0);
    }

    #[test]
    fn fifo_consumes_oldest_layers_first() {
        let prior = [row(10.0, 5.0), row(10.0, 7.0)];
        // 12 units: 10 @ 5 + 2 @ 7 = 64 → 64/12.
        let rate = issue_rate(&prior, 12.0, Some(FIFO));
        assert!((rate - 64.0 / 12.0).abs() < 1e-9);
    }

    #[test]
    fn fifo_shortfall_falls_back_to_moving_average() {
        let prior = [row(2.0, 4.0)];
        // 3 units: 2 @ 4 from the layer, 1 @ 4 (moving average) shortfall.
        assert_eq!(issue_rate(&prior, 3.0, Some(FIFO)), 4.0);
    }

    #[test]
    fn balance_rounds_like_the_dart_engine() {
        let snap = compute_balance(&[row(3.0, 3.333), row(-1.0, 3.333)]);
        assert_eq!(snap.actual_qty, 2.0);
        assert_eq!(snap.stock_value, 6.67);
        assert_eq!(snap.valuation_rate, 3.34);
    }
}
