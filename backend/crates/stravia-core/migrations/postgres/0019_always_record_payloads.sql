DELETE FROM settings WHERE name = 'enable_payload';

ALTER TABLE models DROP COLUMN enable_payload;
