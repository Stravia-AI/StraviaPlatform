//! Per-item, principal-scoped storage. Domain payload versions are unchanged.
//!
//! Format 2 stores the node payload once as a binary envelope:
//! `{"data": <payload with externalized slots set to null>, "slots":
//! [[json_pointer, contents_index], ...], "contents": [content_key, ...]}`.
//! `contents` holds each distinct content key once; `slots` keeps order,
//! repeats and nesting. `turn_chain_node_contents` records only the distinct
//! content ids a node references — paths live solely inside the envelope and
//! are never read back during restore.

use std::collections::HashMap;
use std::io::{self, Write};

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
    /// Unique content in envelope order. Hint-missing entries become codec
    /// bytes; hint-existing entries retain raw JSON for GC recovery.
    pub contents: Vec<(String, Vec<u8>)>,
    stored: Vec<bool>,
}

pub(super) fn content_key(bytes: &[u8]) -> String {
    stravia_runtime_contract::identifier::encode_digest(&Sha256::digest(bytes).into())
}

#[derive(Default)]
struct HashWriter {
    bytes: usize,
    hash: Sha256,
}

impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("JSON size exceeds address space"))?;
        self.hash.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Externalizes `value` when its raw JSON serialization reaches
/// `MIN_EXTERNAL_BYTES`; the identity is the raw bytes, never a canonical
/// projection, so item ids, references, phases and meta are preserved.
/// Returns whether a slot was recorded and the value replaced by null.
fn externalize(value: &mut Value, path: String, state: &mut Extraction) -> anyhow::Result<bool> {
    if value.is_null() {
        return Ok(false);
    }
    // Count and fingerprint the same compact JSON stream before allocating
    // its body. Repeated items need only this pass; unique items retain the
    // previous two-pass serialization and exact-sized body allocation.
    let mut writer = HashWriter::default();
    serde_json::to_writer(&mut writer, &*value)?;
    if writer.bytes < MIN_EXTERNAL_BYTES {
        return Ok(false);
    }
    let key = stravia_runtime_contract::identifier::encode_digest(&writer.hash.finalize().into());
    let index = match state.index.get(&key) {
        Some(index) => *index,
        None => {
            let mut raw = Vec::with_capacity(writer.bytes);
            serde_json::to_writer(&mut raw, &*value)?;
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
                    if externalize(child, path, state)? && state.slots.len() > first + 1 {
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
    // Keep the JSON map's established ordering (also with preserve_order),
    // without constructing another Value tree or copying keys and pointers.
    struct Envelope<'a> {
        data: &'a Value,
        slots: &'a [(String, usize)],
        contents: ContentKeys<'a>,
    }
    struct ContentKeys<'a>(&'a [(String, Vec<u8>)]);
    impl serde::Serialize for ContentKeys<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeSeq;
            let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
            for (key, _) in self.0 {
                sequence.serialize_element(key)?;
            }
            sequence.end()
        }
    }
    impl serde::Serialize for Envelope<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeMap;
            static FIELDS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
            let fields = FIELDS.get_or_init(|| {
                ["data", "slots", "contents"]
                    .into_iter()
                    .map(|key| (key.to_owned(), Value::Null))
                    .collect::<serde_json::Map<_, _>>()
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect()
            });
            let mut map = serializer.serialize_map(Some(3))?;
            for key in fields {
                match key.as_str() {
                    "data" => map.serialize_entry(key, self.data)?,
                    "slots" => map.serialize_entry(key, self.slots)?,
                    "contents" => map.serialize_entry(key, &self.contents)?,
                    _ => unreachable!("envelope field"),
                }
            }
            map.end()
        }
    }
    let envelope = Envelope {
        data: &payload,
        slots: &state.slots,
        contents: ContentKeys(&state.contents),
    };
    let payload = crate::storage_codec::encode(serde_json::to_vec(&envelope)?.into())?;
    Ok(Encoded {
        payload,
        stored: vec![false; state.contents.len()],
        contents: state.contents,
    })
}

/// Existence probes are hints only: GC can remove rows before the transaction.
pub(super) async fn prepare_missing(
    encoded: &mut Encoded,
    ids: &[Option<i64>],
) -> anyhow::Result<()> {
    let stored = &encoded.stored;
    let pending: Vec<_> = encoded
        .contents
        .iter_mut()
        .enumerate()
        .filter(|(index, _)| ids[*index].is_none() && !stored[*index])
        .map(|(index, (_, bytes))| (index, std::mem::take(bytes)))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let prepared = tokio::task::spawn_blocking(move || {
        pending
            .into_iter()
            .map(|(index, raw)| Ok((index, crate::storage_codec::encode(raw.into())?)))
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await
    .context("history content preparation worker failed")??;
    for (index, bytes) in prepared {
        encoded.contents[index].1 = bytes;
        encoded.stored[index] = true;
    }
    Ok(())
}

enum RestoredContent {
    Owned(Value),
    Shared(std::sync::Arc<Value>),
}

impl RestoredContent {
    fn value(&self) -> &Value {
        match self {
            Self::Owned(value) => value,
            Self::Shared(value) => value,
        }
    }

    fn into_value(self) -> Value {
        match self {
            Self::Owned(value) => value,
            Self::Shared(value) => std::sync::Arc::unwrap_or_clone(value),
        }
    }
}

fn content_memory(value: &Value) -> usize {
    let own = std::mem::size_of::<Value>();
    own.saturating_add(match value {
        Value::String(value) => value.capacity(),
        Value::Array(values) => values
            .iter()
            .fold(0usize, |sum, value| {
                sum.saturating_add(content_memory(value))
            })
            .saturating_add(
                values
                    .capacity()
                    .saturating_sub(values.len())
                    .saturating_mul(own),
            ),
        Value::Object(values) => values.iter().fold(0usize, |sum, (key, value)| {
            sum.saturating_add(key.capacity())
                .saturating_add(content_memory(value))
                .saturating_add(std::mem::size_of::<String>() + 4 * std::mem::size_of::<usize>())
        }),
        _ => 0,
    })
}

macro_rules! backend {
    ($put:ident, $prepared:ident, $inner:ident, $lookup:ident, $restore:ident, $restore_context:ident, $db:ty, $conn:ty, $lock:literal) => {
        /// Links an already-inserted node to its distinct content ids. Callers
        /// hold the contents table lock on PostgreSQL; SQLite serializes on the
        /// commit transaction. Empty contents skip every contents statement.
        pub(super) async fn $put(
            connection: &mut $conn,
            node: &str,
            principal: &str,
            encoded: &mut Encoded,
        ) -> anyhow::Result<()> {
            let ids = $lookup(connection, principal, encoded, true).await?;
            // The authoritative lookup protects existing rows until commit;
            // their recovery buffers are no longer needed.
            for ((_, bytes), id) in encoded.contents.iter_mut().zip(&ids) {
                if id.is_some() {
                    *bytes = Vec::new();
                }
            }
            // Only hint-existing rows collected before this lookup need
            // recovery compression, still off the asynchronous worker.
            prepare_missing(encoded, &ids).await?;
            let mut positions = HashMap::with_capacity(encoded.contents.len());
            let mut missing = Vec::new();
            for (index, (key, bytes)) in encoded.contents.iter_mut().enumerate() {
                positions.insert(key.as_str(), index);
                if ids[index].is_none() {
                    ensure!(encoded.stored[index], "missing prepared history content");
                    missing.push((key.as_str(), std::mem::take(bytes)));
                }
            }
            $inner(connection, node, principal, positions, missing, ids).await
        }

        /// Startup migration only: contents have already been codec-encoded
        /// on a blocking worker and raw buffers may already be empty.
        pub(super) async fn $prepared(
            connection: &mut $conn,
            node: &str,
            principal: &str,
            encoded: &Encoded,
            contents: &HashMap<String, Vec<u8>>,
        ) -> anyhow::Result<()> {
            let ids = $lookup(connection, principal, encoded, true).await?;
            let mut positions = HashMap::with_capacity(encoded.contents.len());
            let mut missing = Vec::new();
            for (index, (key, _)) in encoded.contents.iter().enumerate() {
                positions.insert(key.as_str(), index);
                if ids[index].is_none() {
                    missing.push((
                        key.as_str(),
                        contents
                            .get(key)
                            .context("missing prepared history content")?
                            .as_slice(),
                    ));
                }
            }
            $inner(connection, node, principal, positions, missing, ids).await
        }

        /// Without transaction locks the result is only an existence hint.
        pub(super) async fn $lookup(
            connection: &mut $conn,
            principal: &str,
            encoded: &Encoded,
            lock: bool,
        ) -> anyhow::Result<Vec<Option<i64>>> {
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
                if lock {
                    query.push($lock);
                }
                let rows: Vec<(i64, String)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, key) in rows {
                    if let Some(&index) = positions.get(key.as_str()) {
                        ids[index] = Some(id);
                    }
                }
            }
            Ok(ids)
        }

        async fn $inner<'q, B>(
            connection: &mut $conn,
            node: &'q str,
            principal: &'q str,
            positions: HashMap<&'q str, usize>,
            mut missing: Vec<(&'q str, B)>,
            mut ids: Vec<Option<i64>>,
        ) -> anyhow::Result<()>
        where
            B: sqlx::Encode<'q, $db> + sqlx::Type<$db> + Send + 'q,
        {
            if positions.is_empty() {
                return Ok(());
            }
            // Sorted so two concurrent commits inserting overlapping key sets
            // take speculative insertion locks in the same order.
            missing.sort_unstable_by(|left, right| left.0.cmp(right.0));
            let mut missing = missing.into_iter().peekable();
            while missing.peek().is_some() {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_chain_contents (principal, content_key, content) ",
                );
                query.push_values(missing.by_ref().take(200), |mut row, (key, content)| {
                    row.push_bind(principal).push_bind(key).push_bind(content);
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
            let pending: Vec<&str> = positions
                .iter()
                .filter_map(|(key, index)| ids[*index].is_none().then_some(*key))
                .collect();
            for chunk in pending.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT id, content_key FROM turn_chain_contents WHERE principal = ",
                );
                query.push_bind(principal).push(" AND content_key IN (");
                let mut list = query.separated(",");
                for key in chunk {
                    list.push_bind(*key);
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
            let mut content_ids = ids
                .into_iter()
                .map(|id| id.context("missing history content"))
                .collect::<anyhow::Result<Vec<_>>>()?;
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

        /// Restores node payloads. Envelopes carry every slot path and
        /// content key, so the reference table is never read here: unique keys
        /// are collected across the whole chain, fetched once and verified
        /// against their content_key before any slot is spliced.
        pub(super) async fn $restore(
            connection: &mut $conn,
            principal: &str,
            nodes: Vec<TurnNode>,
        ) -> anyhow::Result<Vec<TurnNode>> {
            $restore_context(connection, principal, nodes, None, |node, _| Ok(node)).await
        }

        pub(super) async fn $restore_context<T>(
            connection: &mut $conn,
            principal: &str,
            nodes: Vec<TurnNode>,
            mut context: Option<&mut stravia_runtime_contract::turn_chain::TurnReadContext>,
            mut finish: impl FnMut(TurnNode, Option<&stravia_runtime_contract::turn_chain::TurnReadContext>) -> anyhow::Result<T> + Send,
        ) -> anyhow::Result<Vec<T>> {
            let mut plans: Vec<Vec<(String, String)>> = Vec::with_capacity(nodes.len());
            let mut uses: HashMap<String, usize> = HashMap::new();
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
                            .context("invalid history content key")?,
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
                        .to_string();
                    *uses.entry(key.clone()).or_default() += 1;
                    plan.push((pointer, key));
                }
                plans.push(plan);
            }
            let mut needed: Vec<&String> = uses.keys().collect();
            needed.sort_unstable();
            let mut values: HashMap<String, RestoredContent> = HashMap::with_capacity(needed.len());
            if let Some(context) = context.as_deref() {
                for key in &needed {
                    if let Some(value) = context.cached_content(key) { values.insert((*key).clone(), RestoredContent::Shared(value)); }
                }
            }
            let missing: Vec<_> = needed.iter().copied().filter(|key| !values.contains_key(*key)).collect();
            let mut content_reads = 0u64;
            for chunk in missing.chunks(400) {
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
                    let value = serde_json::from_slice::<Value>(&raw)?;
                    let value = if context.as_deref().is_some_and(|context| context.admission_open()) {
                        let value = std::sync::Arc::new(value);
                        let bytes = content_memory(&value).saturating_add(key.capacity());
                        context.as_deref_mut().expect("open content admission").stage_content(key.clone(), value.clone(), bytes);
                        RestoredContent::Shared(value)
                    } else {
                        RestoredContent::Owned(value)
                    };
                    values.insert(key, value);
                    content_reads += 1;
                }
            }
            for key in &needed {
                ensure!(values.contains_key(*key), "missing history content");
            }
            drop(needed);
            let mut restored = Vec::with_capacity(nodes.len());
            for (mut node, plan) in nodes.into_iter().zip(plans) {
                let mut data = node
                    .payload
                    .get_mut("data")
                    .context("missing history payload")?
                    .take();
                for (pointer, key) in plan {
                    let slot = data.pointer_mut(&pointer).context("invalid history slot")?;
                    ensure!(slot.is_null(), "history slot overwrites data");
                    let remaining = uses.get_mut(&key).context("missing history content")?;
                    *remaining -= 1;
                    // Each JSON slot needs an independent Value, but the final
                    // occurrence can consume an uncached decoded tree instead
                    // of cloning it and immediately dropping the original.
                    *slot = if *remaining == 0 {
                        values.remove(&key).context("missing history content")?.into_value()
                    } else {
                        values.get(&key).context("missing history content")?.value().clone()
                    };
                }
                node.payload = data;
                restored.push(finish(node, context.as_deref())?);
            }
            tracing::info!(target: "stravia::perf", content_read_count = content_reads,
                reference_count = content_reads, "turn_chain.content.restore");
            Ok(restored)
        }
    };
}
backend!(
    put_sqlite,
    put_sqlite_prepared,
    put_sqlite_inner,
    lookup_sqlite,
    restore_sqlite,
    restore_sqlite_context,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>,
    ""
);
backend!(
    put_postgres,
    put_postgres_prepared,
    put_postgres_inner,
    lookup_postgres,
    restore_postgres,
    restore_postgres_context,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    " FOR KEY SHARE"
);
