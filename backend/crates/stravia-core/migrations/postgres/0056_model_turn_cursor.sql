ALTER TABLE vendor_atomic_lineages ADD COLUMN model_turn_cursor TEXT;
ALTER TABLE vendor_atomic_checkpoints ADD COLUMN model_turn_cursor TEXT;
