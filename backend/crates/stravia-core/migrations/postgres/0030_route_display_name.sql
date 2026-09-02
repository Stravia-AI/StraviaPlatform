ALTER TABLE models RENAME COLUMN name TO model_id;
ALTER TABLE models ADD COLUMN display_name TEXT;

ALTER INDEX idx_models_route_id RENAME TO idx_models_route_id_legacy;
CREATE UNIQUE INDEX idx_models_route_id ON models(model_id);
DROP INDEX idx_models_route_id_legacy;
