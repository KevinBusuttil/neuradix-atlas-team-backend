-- Phase 3 refinement — UOM conversion at posting. A stock ledger entry keeps
-- the transaction UOM the line was entered in (`uom`, null = the item's stock
-- UOM); its qty_change/valuation_rate are already converted to stock units by
-- the engine (qty × factor, receipt rate ÷ factor — value-preserving),
-- mirroring the Dart `uomFactor` + `_costStockMovements` semantics.

alter table stock_ledger_entries add column uom text;
