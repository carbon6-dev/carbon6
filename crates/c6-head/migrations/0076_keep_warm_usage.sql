-- Maintenance calls still count toward total spending and budgets.
ALTER TABLE usage ADD COLUMN keep_warm INTEGER NOT NULL DEFAULT 0;
