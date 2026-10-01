//! Per-item, principal-scoped storage. Domain payload versions are unchanged.
//!
//! Format 2 stores the node payload once as a binary envelope:
//! `{"data": <payload with externalized slots set to null>, "slots":
//! [[json_pointer, contents_index], ...], "contents": [content_key, ...]}`.
//! `contents` holds each distinct content key once; `slots` keeps order,
//! repeats and nesting. `turn_chain_node_contents` records only the distinct
//! content ids a node references — paths live solely inside the envelope and
//! are never read back during restore.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_runtime_contract::turn_chain::TurnNode;

/// Values serialized below this size stay inline in the envelope: a content
/// row plus a slot entry costs more than the value itself.
const MIN_EXTERNAL_BYTES: usize = 256;

/// Keys whose array elements are raw history items. Elements are opaque
/// leaves: they are externalized verbatim and their inner fields are never
/// interpreted.
const ITEM_ARRAY_KEYS: &[&str] = &[
    "items",
    "input",
    "output",
    "messages",
    "transcript",
    "client_output",
];
/// Keys whose whole value is a shared configuration string or object.
const SCALAR_KEYS: &[&str] = &["system", "effective_system", "instructions", "tools"];
/// Vendor containers that are walked for inner slots first, then externalized
/// whole so their smaller keys deduplicate independently.
const PROFILE_KEYS: &[&str] = &[
    "__open_responses_response_profile",
    "__open_responses_effective_request",
];

struct Extraction {
    /// `(json pointer, contents index)` pairs in restore apply order: a parent
    /// container always precedes slots nested inside it.
    slots: Vec<(String, usize)>,
    /// Unique `(content_key, raw JSON bytes)` in `contents` array order.
    contents: Vec<(String, Vec<u8>)>,
    index: HashMap<String, usize>,
}

pub(super) struct Encoded {
    pub payload: Vec<u8>,
    /// Unique `(content_key, raw JSON)` in envelope `contents` order. Bytes are
    /// kept raw: only keys missing from `turn_chain_contents` pay for codec
    /// compression and the insert round-trip.
    pub contents: Vec<(String, Vec<u8>)>,
}

pub(super) fn content_key(bytes: &[u8]) -> String {
    stravia_runtime_contract::identifier::encode_digest(&Sha256::digest(bytes).into())
}

/// Externalizes `value` when its raw JSON serialization reaches
/// `MIN_EXTERNAL_BYTES`; the identity is the raw bytes, never a canonical
/// projection, so item ids, references, phases and meta are preserved.
/// Returns whether a slot was recorded and the value replaced by null.
fn externalize(value: &mut Value, path: String, state: &mut Extraction) -> anyhow::Result<bool> {
    if value.is_null() {
        return Ok(false);
    }
    let raw = serde_json::to_vec(value)?;
    if raw.len() < MIN_EXTERNAL_BYTES {
        return Ok(false);
    }
    let key = content_key(&raw);
    let index = match state.index.get(&key) {
        Some(index) => *index,
        None => {
            let index = state.contents.len();
            state.index.insert(key.clone(), index);
            state.contents.push((key, raw));
            index
        }
    };
    state.slots.push((path, index));
    *value = Value::Null;
    Ok(true)
}

fn externalize_items(
    array: &mut [Value],
    path: &str,
    state: &mut Extraction,
) -> anyhow::Result<()> {
    for (index, item) in array.iter_mut().enumerate() {
        externalize(item, format!("{path}/{index}"), state)?;
    }
    Ok(())
}

fn walk(value: &mut Value, path: &str, state: &mut Extraction) -> anyhow::Result<()> {
    match value {
        Value::Object(object) => {
            for (name, child) in object.iter_mut() {
                let path = format!("{path}/{}", name.replace('~', "~0").replace('/', "~1"));
                let name = name.as_str();
                if ITEM_ARRAY_KEYS.contains(&name) && child.is_array() {
                    externalize_items(
                        child.as_array_mut().context("history item array")?,
                        &path,
                        state,
                    )?;
                } else if SCALAR_KEYS.contains(&name) {
                    externalize(child, path, state)?;
                } else if PROFILE_KEYS.contains(&name) {
                    // Inner slots are discovered first but the container must
                    // restore before them: keep its entry ahead of its own
                    // subtree so pointer resolution always descends through a
                    // materialized parent.
                    let first = state.slots.len();
                    walk(child, &path, state)?;
                    if externalize(child, path.clone(), state)? && state.slots.len() > first + 1 {
                        state.slots[first..].rotate_right(1);
                    }
                } else {
                    walk(child, &path, state)?;
                }
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter_mut().enumerate() {
                walk(child, &format!("{path}/{index}"), state)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Binary-encodes the node envelope once. Repeated values share one
/// `contents` entry; every occurrence still records its own slot so restore
/// reproduces order, repeats and nulls exactly.
pub(super) fn encode(mut payload: Value) -> anyhow::Result<Encoded> {
    let mut state = Extraction {
        slots: Vec::new(),
        contents: Vec::new(),
        index: HashMap::new(),
    };
    walk(&mut payload, "", &mut state)?;
    let envelope = serde_json::json!({
        "data": payload,
        "slots": state
            .slots
            .iter()
            .map(|(pointer, index)| serde_json::json!([pointer, index]))
            .collect::<Vec<_>>(),
        "contents": state
            .contents
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
    });
    let payload = crate::storage_codec::encode(&serde_json::to_vec(&envelope)?)?;
    Ok(Encoded {
        payload,
        contents: state.contents,
    })
}

macro_rules! backend {
    ($put:ident, $prepared:ident, $inner:ident, $restore:ident, $db:ty, $conn:ty, $lock:literal) => {
        /// Links an already-inserted node to its distinct content ids. Callers
        /// hold the contents table lock on PostgreSQL; SQLite serializes on the
        /// commit transaction. Empty contents skip every contents statement.
        pub(super) async fn $put(
            connection: &mut $conn,
            node: &str,
            principal: &str,
            encoded: &Encoded,
        ) -> anyhow::Result<()> {
            $inner(connection, node, principal, encoded, None).await
        }

        /// Startup migration only: contents have already been codec-encoded
        /// on a blocking worker. Runtime writes retain lazy compression.
        pub(super) async fn $prepared(
            connection: &mut $conn,
            node: &str,
            principal: &str,
            encoded: &Encoded,
            contents: &HashMap<String, Vec<u8>>,
        ) -> anyhow::Result<()> {
            $inner(connection, node, principal, encoded, Some(contents)).await
        }

        async fn $inner(
            connection: &mut $conn,
            node: &str,
            principal: &str,
            encoded: &Encoded,
            prepared: Option<&HashMap<String, Vec<u8>>>,
        ) -> anyhow::Result<()> {
            if encoded.contents.is_empty() {
                return Ok(());
            }
            let positions: HashMap<&str, usize> = encoded
                .contents
                .iter()
                .enumerate()
                .map(|(index, (key, _))| (key.as_str(), index))
                .collect();
            let mut ids: Vec<Option<i64>> = vec![None; encoded.contents.len()];
            for keys in encoded.contents.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT id, content_key FROM turn_chain_contents WHERE principal = ",
                );
                query.push_bind(principal).push(" AND content_key IN (");
                let mut list = query.separated(",");
                for (key, _) in keys {
                    list.push_bind(key.as_str());
                }
                list.push_unseparated(")");
                query.push($lock);
                let rows: Vec<(i64, String)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, key) in rows {
                    if let Some(&index) = positions.get(key.as_str()) {
                        ids[index] = Some(id);
                    }
                }
            }
            // Sorted so two concurrent commits inserting overlapping key sets
            // take speculative insertion locks in the same order.
            let mut missing: Vec<&(String, Vec<u8>)> = encoded
                .contents
                .iter()
                .enumerate()
                .filter(|(index, _)| ids[*index].is_none())
                .map(|(_, entry)| entry)
                .collect();
            missing.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            for chunk in missing.chunks(200) {
                let mut rows = Vec::with_capacity(chunk.len());
                for entry in chunk {
                    rows.push((
                        entry.0.as_str(),
                        if let Some(contents) = prepared {
                            std::borrow::Cow::Borrowed(
                                contents
                                    .get(&entry.0)
                                    .context("missing prepared history content")?
                                    .as_slice(),
                            )
                        } else {
                            std::borrow::Cow::Owned(crate::storage_codec::encode(
                                entry.1.as_slice(),
                            )?)
                        },
                    ));
                }
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_chain_contents (principal, content_key, content) ",
                );
                query.push_values(rows.iter(), |mut row, (key, content)| {
                    row.push_bind(principal)
                        .push_bind(*key)
                        .push_bind(content.as_ref());
                });
                query.push(
                    " ON CONFLICT (principal, content_key) DO NOTHING RETURNING id, content_key",
                );
                let inserted: Vec<(i64, String)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, key) in inserted {
                    if let Some(&index) = positions.get(key.as_str()) {
                        ids[index] = Some(id);
                    }
                }
            }
            // Concurrent commits that won a race above committed their row; a
            // targeted lookup resolves those ids without DO UPDATE or retries.
            for chunk in encoded.contents.chunks(400) {
                let pending: Vec<&str> = chunk
                    .iter()
                    .filter(|(key, _)| ids[positions[key.as_str()]].is_none())
                    .map(|(key, _)| key.as_str())
                    .collect();
                if pending.is_empty() {
                    continue;
                }
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT id, content_key FROM turn_chain_contents WHERE principal = ",
                );
                query.push_bind(principal).push(" AND content_key IN (");
                let mut list = query.separated(",");
                for key in pending {
                    list.push_bind(key);
                }
                list.push_unseparated(")");
                query.push($lock);
                let rows: Vec<(i64, String)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, key) in rows {
                    if let Some(&index) = positions.get(key.as_str()) {
                        ids[index] = Some(id);
                    }
                }
            }
            let mut content_ids = Vec::with_capacity(encoded.contents.len());
            for index in 0..encoded.contents.len() {
                content_ids.push(ids[index].context("missing history content")?);
            }
            content_ids.sort_unstable();
            content_ids.dedup();
            for chunk in content_ids.chunks(200) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_chain_node_contents (node_id, principal, content_id) ",
                );
                query.push_values(chunk.iter(), |mut row, id| {
                    row.push_bind(node).push_bind(principal).push_bind(*id);
                });
                query.build().execute(&mut *connection).await?;
            }
            Ok(())
        }

        /// Restores node payloads in place. Envelopes carry every slot path and
        /// content key, so the reference table is never read here: unique keys
        /// are collected across the whole chain, fetched once and verified
        /// against their content_key before any slot is spliced.
        pub(super) async fn $restore(
            connection: &mut $conn,
            principal: &str,
            nodes: &mut [TurnNode],
        ) -> anyhow::Result<()> {
            let mut plans: Vec<Vec<(String, String)>> = Vec::with_capacity(nodes.len());
            let mut needed: HashSet<String> = HashSet::new();
            for node in nodes.iter() {
                let envelope = &node.payload;
                let contents = envelope
                    .get("contents")
                    .and_then(Value::as_array)
                    .context("missing history contents")?;
                let mut keys = Vec::with_capacity(contents.len());
                for entry in contents {
                    keys.push(
                        entry
                            .as_str()
                            .context("invalid history content key")?
                            .to_owned(),
                    );
                }
                let slots = envelope
                    .get("slots")
                    .and_then(Value::as_array)
                    .context("missing history slots")?;
                let mut plan = Vec::with_capacity(slots.len());
                for slot in slots {
                    let pair = slot.as_array().context("invalid history slot")?;
                    ensure!(pair.len() == 2, "invalid history slot");
                    let pointer = pair[0]
                        .as_str()
                        .context("invalid history slot pointer")?
                        .to_owned();
                    let index =
                        usize::try_from(pair[1].as_u64().context("invalid history slot index")?)
                            .context("invalid history slot index")?;
                    let key = keys
                        .get(index)
                        .context("history slot index out of range")?
                        .clone();
                    needed.insert(key.clone());
                    plan.push((pointer, key));
                }
                plans.push(plan);
            }
            let mut needed: Vec<String> = needed.into_iter().collect();
            needed.sort_unstable();
            let mut values: HashMap<String, Value> = HashMap::with_capacity(needed.len());
            for chunk in needed.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT content_key, content FROM turn_chain_contents WHERE principal = ",
                );
                query.push_bind(principal).push(" AND content_key IN (");
                let mut list = query.separated(",");
                for key in chunk {
                    list.push_bind(key.as_str());
                }
                list.push_unseparated(")");
                let rows: Vec<(String, Vec<u8>)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (key, encoded) in rows {
                    let raw = crate::storage_codec::decode(&encoded)?;
                    ensure!(content_key(&raw) == key, "damaged history content");
                    values.insert(key, serde_json::from_slice::<Value>(&raw)?);
                }
            }
            for key in &needed {
                ensure!(values.contains_key(key), "missing history content");
            }
            for (node, plan) in nodes.iter_mut().zip(plans) {
                let mut data = node
                    .payload
                    .get_mut("data")
                    .context("missing history payload")?
                    .take();
                for (pointer, key) in plan {
                    let slot = data.pointer_mut(&pointer).context("invalid history slot")?;
                    ensure!(slot.is_null(), "history slot overwrites data");
                    *slot = values.get(&key).context("missing history content")?.clone();
                }
                node.payload = data;
            }
            Ok(())
        }
    };
}
backend!(
    put_sqlite,
    put_sqlite_prepared,
    put_sqlite_inner,
    restore_sqlite,
    sqlx::Sqlite,
    sqlx::SqliteConnection,
    ""
);
backend!(
    put_postgres,
    put_postgres_prepared,
    put_postgres_inner,
    restore_postgres,
    sqlx::Postgres,
    sqlx::PgConnection,
    " FOR KEY SHARE"
);
