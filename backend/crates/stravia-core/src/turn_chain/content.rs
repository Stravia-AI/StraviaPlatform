//! SQL 内部结构去重；业务 payload 的版本和语义不随存储表示改变。
use std::collections::{HashMap, HashSet};

use anyhow::{Context, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_runtime_contract::turn_chain::TurnNode;

const MIN_SHARED_BYTES: usize = 256;
const PROFILES: [&str; 2] = [
    "/effective_output/vendor/ingress/__open_responses_response_profile",
    "/effective_output/vendor/ingress/__open_responses_effective_request",
];

pub(super) struct ContentRef {
    pub path: String,
    pub key: String,
    pub content: String,
}

pub(super) struct Encoded {
    pub payload: String,
    pub format: i64,
    pub refs: Vec<ContentRef>,
}

pub(super) fn encode(mut payload: Value) -> anyhow::Result<Encoded> {
    let mut refs = Vec::new();
    // 先提取叶子，再提取去掉叶子后的 profile。读侧按路径长度恢复，
    // 因而同一 instructions/tools 不会又被完整嵌入共享 profile。
    for path in [
        "/effective_system",
        "/effective_request/instructions",
        "/effective_request/tools",
    ] {
        extract(&mut payload, path, &mut refs)?;
    }
    for profile in PROFILES {
        for field in ["instructions", "tools"] {
            extract(&mut payload, &format!("{profile}/{field}"), &mut refs)?;
        }
        extract(&mut payload, profile, &mut refs)?;
    }
    let format = i64::from(!refs.is_empty());
    if format == 1 {
        payload = serde_json::json!({"data": payload, "references": refs.len()});
    }
    Ok(Encoded {
        payload: serde_json::to_string(&payload)?,
        format,
        refs,
    })
}

fn extract(payload: &mut Value, path: &str, refs: &mut Vec<ContentRef>) -> anyhow::Result<()> {
    let Some(value) = payload.pointer_mut(path) else {
        return Ok(());
    };
    if value.is_null() {
        return Ok(());
    }
    let content = serde_json::to_string(value)?;
    if content.len() < MIN_SHARED_BYTES {
        return Ok(());
    }
    let key = content_key(&content);
    *value = Value::Null;
    refs.push(ContentRef {
        path: path.into(),
        key,
        content,
    });
    Ok(())
}

fn content_key(content: &str) -> String {
    stravia_runtime_contract::identifier::encode_digest(&Sha256::digest(content.as_bytes()).into())
}

// 每批一次 JOIN；避免随历史节点数逐条查询。SQL 层强制 Principal 一致。
macro_rules! backend {
    ($put:ident, $restore:ident, $db:ty, $conn:ty) => {
        pub(super) async fn $put(
            connection: &mut $conn, node: &str, principal: &str, encoded: &Encoded,
        ) -> anyhow::Result<()> {
            for reference in &encoded.refs {
                let stored: Option<String> = sqlx::query_scalar(
                    "INSERT INTO turn_chain_contents (principal, content_key, content) VALUES ($1, $2, $3) \
                     ON CONFLICT (principal, content_key) DO UPDATE SET content = turn_chain_contents.content \
                     WHERE turn_chain_contents.content = excluded.content RETURNING content_key"
                ).bind(principal).bind(&reference.key).bind(&reference.content)
                    .fetch_optional(&mut *connection).await?;
                ensure!(stored.is_some(), "history content digest collision");
                sqlx::query("INSERT INTO turn_chain_content_refs (node_id, principal, path, content_key) VALUES ($1, $2, $3, $4)")
                    .bind(node).bind(principal).bind(&reference.path).bind(&reference.key)
                    .execute(&mut *connection).await?;
            }
            Ok(())
        }

        pub(super) async fn $restore(connection: &mut $conn, nodes: &mut [TurnNode]) -> anyhow::Result<()> {
            for batch in nodes.chunks_mut(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT n.id, CAST(n.storage_format AS BIGINT), r.path, r.content_key, c.content FROM turn_chain_nodes n \
                     LEFT JOIN turn_chain_content_refs r ON r.node_id = n.id AND r.principal = n.principal \
                     LEFT JOIN turn_chain_contents c ON c.principal = r.principal AND c.content_key = r.content_key WHERE n.id IN ("
                );
                let mut list = query.separated(",");
                for node in batch.iter() { list.push_bind(node.id.as_str()); }
                list.push_unseparated(")");
                let rows: Vec<(String, i64, Option<String>, Option<String>, Option<String>)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                let mut by_node: HashMap<String, (i64, Vec<(String, String)>)> = HashMap::new();
                let mut contents = HashMap::new();
                for (id, format, path, key, content) in rows {
                    let entry = by_node.entry(id).or_insert_with(|| (format, Vec::new()));
                    if let Some(path) = path {
                        let key = key.context("missing history content identity")?;
                        let content = content.context("missing history content")?;
                        if !contents.contains_key(&key) {
                            ensure!(content_key(&content) == key, "damaged history content");
                            contents.insert(key.clone(), serde_json::from_str::<Value>(&content)?);
                        }
                        entry.1.push((path, key));
                    }
                }
                for node in batch {
                    let (format, mut references) = by_node.remove(node.id.as_str()).context("history node disappeared")?;
                    match format {
                        0 => ensure!(references.is_empty(), "unexpected history references"),
                        1 => {
                            let expected = node.payload["references"].as_u64().context("invalid history reference count")?;
                            ensure!(expected > 0 && expected == references.len() as u64, "incomplete history references");
                            let mut value = node.payload.get_mut("data").context("missing history payload")?.take();
                            references.sort_by_key(|(path, _)| path.len());
                            let mut seen = HashSet::new();
                            for (path, key) in references {
                                ensure!(seen.insert(path.clone()), "duplicate history reference");
                                let slot = value.pointer_mut(&path).context("invalid history content path")?;
                                ensure!(slot.is_null(), "history reference overwrites content");
                                *slot = contents.get(&key).context("missing history content")?.clone();
                            }
                            node.payload = value;
                        }
                        _ => anyhow::bail!("unsupported history storage format"),
                    }
                }
            }
            Ok(())
        }
    }
}

backend!(
    put_sqlite,
    restore_sqlite,
    sqlx::Sqlite,
    sqlx::SqliteConnection
);
backend!(
    put_postgres,
    restore_postgres,
    sqlx::Postgres,
    sqlx::PgConnection
);
