-- 公开身份不变；内部关联使用不会复用的整数，正文与频繁更新的保留期分离。
-- 旧表在有界数据转换完成前保留，0033 才清理它们。
CREATE TABLE turn_scopes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    principal TEXT NOT NULL UNIQUE
);

CREATE TABLE turn_nodes (
    node_pk INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    scope_id BIGINT NOT NULL REFERENCES turn_scopes(id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK(kind IN ('response', 'agent', 'web_search')),
    parent_pk BIGINT,
    payload_version BIGINT NOT NULL CHECK(payload_version > 0),
    created_at BIGINT NOT NULL,
    prefix_namespace TEXT,
    prefix_fingerprint TEXT,
    prefix_item_count BIGINT,
    prefix_completed_at BIGINT,
    prefix_projection_version BIGINT,
    prefix_controls_fingerprint TEXT,
    prefix_context_fingerprint TEXT,
    UNIQUE(node_pk, scope_id),
    UNIQUE(node_pk, scope_id, kind),
    FOREIGN KEY(parent_pk, scope_id, kind)
        REFERENCES turn_nodes(node_pk, scope_id, kind)
        ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX turn_nodes_parent_idx ON turn_nodes(parent_pk, scope_id, kind);
CREATE INDEX turn_nodes_scope_idx ON turn_nodes(scope_id);
CREATE INDEX turn_nodes_prefix_idx
    ON turn_nodes(scope_id, kind, prefix_namespace, prefix_fingerprint,
                  prefix_item_count DESC, prefix_completed_at DESC, id DESC)
    WHERE prefix_namespace IS NOT NULL;

CREATE TABLE turn_node_payloads (
    node_pk INTEGER PRIMARY KEY REFERENCES turn_nodes(node_pk) ON DELETE CASCADE,
    payload BLOB NOT NULL
);

CREATE TABLE turn_node_retention (
    node_pk INTEGER PRIMARY KEY REFERENCES turn_nodes(node_pk) ON DELETE CASCADE,
    expires_at BIGINT NOT NULL,
    deleting INTEGER NOT NULL DEFAULT 0 CHECK(deleting IN (0, 1))
);
CREATE INDEX turn_node_retention_expiry_idx ON turn_node_retention(expires_at, node_pk);

CREATE TABLE turn_contents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_id BIGINT NOT NULL REFERENCES turn_scopes(id) ON DELETE RESTRICT,
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    content BLOB NOT NULL,
    UNIQUE(scope_id, digest),
    UNIQUE(scope_id, id)
);

-- ordinal 是 descriptor 中的局部引用；相同内容的多次 occurrence 复用同一 ordinal。
-- scope_id 只用于数据库级复合外键归属校验，不复制 Principal 长字符串。
CREATE TABLE turn_node_contents (
    node_pk BIGINT NOT NULL,
    scope_id BIGINT NOT NULL,
    ordinal BIGINT NOT NULL CHECK(ordinal >= 0),
    content_id BIGINT NOT NULL,
    PRIMARY KEY(node_pk, ordinal),
    UNIQUE(node_pk, content_id),
    FOREIGN KEY(node_pk, scope_id)
        REFERENCES turn_nodes(node_pk, scope_id) ON DELETE CASCADE,
    FOREIGN KEY(scope_id, content_id)
        REFERENCES turn_contents(scope_id, id) ON DELETE RESTRICT
) WITHOUT ROWID;
CREATE INDEX turn_node_contents_content_idx ON turn_node_contents(content_id);

CREATE TABLE turn_node_gc (
    node_pk INTEGER PRIMARY KEY REFERENCES turn_nodes(node_pk) ON DELETE CASCADE
);
CREATE TABLE turn_content_gc (
    content_id INTEGER PRIMARY KEY REFERENCES turn_contents(id) ON DELETE CASCADE
);

CREATE TABLE turn_history_upgrade (
    phase INTEGER PRIMARY KEY CHECK(phase = 1),
    stage INTEGER NOT NULL CHECK(stage BETWEEN 0 AND 4),
    last_node_id TEXT,
    last_content_id BIGINT
);
INSERT INTO turn_history_upgrade(phase, stage) VALUES (1, 0);
