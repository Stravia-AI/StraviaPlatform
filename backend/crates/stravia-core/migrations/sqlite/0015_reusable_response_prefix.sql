ALTER TABLE turn_chain_nodes ADD COLUMN prefix_namespace TEXT;
ALTER TABLE turn_chain_nodes ADD COLUMN prefix_fingerprint TEXT;
ALTER TABLE turn_chain_nodes ADD COLUMN prefix_item_count INTEGER;
ALTER TABLE turn_chain_nodes ADD COLUMN prefix_completed_at INTEGER;

CREATE INDEX idx_turn_chain_reusable_prefix
ON turn_chain_nodes (
    principal,
    kind,
    prefix_namespace,
    prefix_fingerprint,
    prefix_item_count DESC,
    prefix_completed_at DESC,
    expires_at,
    id DESC
)
WHERE prefix_namespace IS NOT NULL;

DELETE FROM artifact_delivery_tokens WHERE principal = 'anonymous';
DELETE FROM media_derivatives WHERE principal = 'anonymous';
DELETE FROM artifact_uploads WHERE principal = 'anonymous';
DELETE FROM image_generation_runs WHERE principal = 'anonymous';
UPDATE image_continuations SET parent_id = NULL WHERE principal = 'anonymous';
DELETE FROM image_continuations WHERE principal = 'anonymous';
UPDATE turn_chain_nodes SET parent_id = NULL WHERE principal = 'anonymous';
DELETE FROM turn_chain_nodes WHERE principal = 'anonymous';
DELETE FROM artifacts WHERE principal = 'anonymous';
