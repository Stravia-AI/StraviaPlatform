DELETE FROM models WHERE operation = 'image_generation';
DELETE FROM settings WHERE name = 'default_image_route_id';

DROP TABLE image_generation_attempts;
DROP TABLE image_generation_runs;
DROP TABLE image_continuations;
DROP TABLE image_capability_drifts;
DROP TABLE artifact_delivery_tokens;

ALTER TABLE models DROP COLUMN operation;
ALTER TABLE api_keys DROP COLUMN image_rpm;
ALTER TABLE api_keys DROP COLUMN image_rpd;
ALTER TABLE api_keys DROP COLUMN allow_image_generation;
ALTER TABLE artifacts DROP COLUMN insecure_transport;
