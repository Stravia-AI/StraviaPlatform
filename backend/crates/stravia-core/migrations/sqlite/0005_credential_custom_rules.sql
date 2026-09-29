-- 用户自定义凭据保护规则；spec 保存 simple/pattern 两种模式的 JSON 定义。
CREATE TABLE credential_custom_rules (
    id          TEXT PRIMARY KEY NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    enabled     INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    spec        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
