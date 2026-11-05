-- C5: Journal Entry lines carry a cost centre (the Dart `_gl` builder's
-- `cost_center`), so the GL keeps the dimension the Solo engine keeps.
alter table gl_entries add column cost_center text;
