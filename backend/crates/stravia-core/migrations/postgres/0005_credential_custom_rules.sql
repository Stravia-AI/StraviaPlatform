-- 用户自定义凭据保护规则；spec 保存 simple/pattern 两种模式的 JSON 定义。
CREATE TABLE credential_custom_rules (
    id          text PRIMARY KEY NOT NULL,
    name        text NOT NULL,
    description text NOT NULL,
    enabled     boolean NOT NULL,
    spec        text NOT NULL,
    created_at  bigint NOT NULL,
    updated_at  bigint NOT NULL
);
