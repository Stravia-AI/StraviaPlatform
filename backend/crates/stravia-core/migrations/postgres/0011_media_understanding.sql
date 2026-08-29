ALTER TABLE api_keys
ADD COLUMN allow_media_understanding BOOLEAN NOT NULL DEFAULT FALSE;
