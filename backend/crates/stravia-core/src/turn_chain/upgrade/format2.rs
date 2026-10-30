//! Migration-only format-0/1 -> format-2 encoder and old-reference writer.
//!
//! The historical 0007 hook runs before migration 0032 exists. It must keep
//! writing the legacy 256-byte-threshold envelope with SHA256 content keys into
//! `turn_chain_*`; the product module owns the domain-prefixed, no-threshold
//! format and must never see this path. Nothing here is reachable from normal
//! history reads or writes.

use std::collections::HashMap;

use anyhow::{Context, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Values serialized below this size stay inline in the legacy envelope.
const MIN_EXTERNAL_BYTES: usize = 256;

/// Raw history item arrays were externalized per element by the legacy writer.
const ITEM_ARRAY_KEYS: &[&str] = &[
    "items",
    "input",
    "output",
    "messages",
    "transcript",
    "client_output",
];
/// Shared configuration values were externalized whole.
const SCALAR_KEYS: &[&str] = &["system", "effective_system", "instructions", "tools"];
/// Vendor containers walked for inner slots first, then externalized whole.
pub(super) const PROFILE_KEYS: &[&str] = &[
    "__open_responses_response_profile",
    "__open_responses_effective_request",
];

pub(super) struct Encoded {
    /// Storage-codec encoded legacy envelope.
    pub payload: Vec<u8>,
    /// Unique `(SHA256 content key, raw JSON bytes)` in envelope order.
    pub contents: Vec<(String, Vec<u8>)>,
}

struct Extraction {
    slots: Vec<(String, usize)>,
    contents: Vec<(String, Vec<u8>)>,
    index: HashMap<String, usize>,
}

/// Legacy SHA256 identity. Migration verification only.
pub(super) fn content_key(bytes: &[u8]) -> String {
    stravia_runtime_contract::identifier::encode_digest(&Sha256::digest(bytes).into())
}

fn escape_token(token: &str) -> String {
    let mut escaped = String::with_capacity(token.len());
    for character in token.chars() {
        match character {
            '~' => escaped.push_str("~0"),
            '/' => escaped.push_str("~1"),
            character => escaped.push(character),
        }
    }
    escaped
}

pub(super) fn pointer_token(token: &str) -> String {
    format!("/{}", escape_token(token))
}

/// Rejects malformed JSON pointers before any pointer is resolved.
pub(super) fn validate_pointer(pointer: &str) -> anyhow::Result<()> {
    ensure!(
        pointer.starts_with('/'),
        "invalid legacy history slot pointer"
    );
    let bytes = pointer.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'~' {
            ensure!(
                index + 1 < bytes.len() && matches!(bytes[index + 1], b'0' | b'1'),
                "invalid legacy history slot pointer escape"
            );
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(())
}

/// Returns whether this pointer names a legacy profile container slot. Nested
/// profile slots always end in their own key token; the fixed container names
/// contain no pointer escapes, so the final token is compared directly.
pub(super) fn is_profile_pointer(pointer: &str) -> bool {
    pointer
        .rsplit('/')
        .next()
        .is_some_and(|token| PROFILE_KEYS.contains(&token))
}

/// Externalizes one non-null legacy unit at or above the old 256-byte
/// threshold. The identity is raw compact JSON, never a canonical projection.
fn externalize(value: &mut Value, path: &str, state: &mut Extraction) -> anyhow::Result<bool> {
    if value.is_null() {
        return Ok(false);
    }
    let mut raw = Vec::new();
    serde_json::to_writer(&mut raw, &*value)?;
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
    state.slots.push((path.to_owned(), index));
    *value = Value::Null;
    Ok(true)
}

fn externalize_items(
    array: &mut [Value],
    path: &mut String,
    state: &mut Extraction,
) -> anyhow::Result<()> {
    for (index, item) in array.iter_mut().enumerate() {
        let base = path.len();
        path.push('/');
        path.push_str(&index.to_string());
        externalize(item, path, state)?;
        path.truncate(base);
    }
    Ok(())
}

fn walk(value: &mut Value, path: &mut String, state: &mut Extraction) -> anyhow::Result<()> {
    match value {
        Value::Object(object) => {
            for (name, child) in object.iter_mut() {
                let base = path.len();
                path.push('/');
                path.push_str(&escape_token(name));
                let name = name.as_str();
                if ITEM_ARRAY_KEYS.contains(&name) && child.is_array() {
                    externalize_items(
                        child.as_array_mut().context("legacy history item array")?,
                        path,
                        state,
                    )?;
                } else if SCALAR_KEYS.contains(&name) {
                    externalize(child, path, state)?;
                } else if PROFILE_KEYS.contains(&name) {
                    // The container must restore before its nested slots.
                    let first = state.slots.len();
                    walk(child, path, state)?;
                    if externalize(child, path, state)? && state.slots.len() > first + 1 {
                        state.slots[first..].rotate_right(1);
                    }
                } else {
                    walk(child, path, state)?;
                }
                path.truncate(base);
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter_mut().enumerate() {
                let base = path.len();
                path.push('/');
                path.push_str(&index.to_string());
                walk(child, path, state)?;
                path.truncate(base);
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct Envelope<'a> {
    data: &'a Value,
    slots: &'a [(String, usize)],
    contents: Vec<&'a str>,
}

/// Legacy envelope encoder used by the historical 0007 hook only.
pub(super) fn encode(mut payload: Value) -> anyhow::Result<Encoded> {
    let mut state = Extraction {
        slots: Vec::new(),
        contents: Vec::new(),
        index: HashMap::new(),
    };
    walk(&mut payload, &mut String::new(), &mut state)?;
    let envelope = Envelope {
        data: &payload,
        slots: &state.slots,
        contents: state.contents.iter().map(|(key, _)| key.as_str()).collect(),
    };
    let bytes = serde_json::to_vec(&envelope)?;
    Ok(Encoded {
        payload: crate::storage_codec::encode(bytes.into())?,
        contents: state.contents,
    })
}

macro_rules! backend {
    ($put:ident, $lookup:ident, $db:ty, $conn:ty, $lock:literal) => {
        async fn $lookup(
            connection: &mut $conn,
            principal: &str,
            keys: &[&str],
            lock: bool,
        ) -> anyhow::Result<Vec<Option<i64>>> {
            let positions: HashMap<&str, usize> = keys
                .iter()
                .enumerate()
                .map(|(index, key)| (*key, index))
                .collect();
            let mut ids = vec![None; keys.len()];
            for chunk in keys.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT id, content_key FROM turn_chain_contents WHERE principal = ",
                );
                query.push_bind(principal).push(" AND content_key IN (");
                let mut list = query.separated(",");
                for key in chunk {
                    list.push_bind(*key);
                }
                list.push_unseparated(")");
                if lock {
                    query.push($lock);
                }
                let rows: Vec<(i64, String)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, key) in rows {
                    if let Some(index) = positions.get(key.as_str()) {
                        ids[*index] = Some(id);
                    }
                }
            }
            Ok(ids)
        }

        /// Writes prepared legacy contents and links the distinct ids. Only the
        /// historical 0007 converter calls this.
        pub(super) async fn $put(
            connection: &mut $conn,
            node: &str,
            principal: &str,
            encoded: &Encoded,
            contents: &HashMap<String, Vec<u8>>,
        ) -> anyhow::Result<()> {
            if encoded.contents.is_empty() {
                return Ok(());
            }
            let keys: Vec<&str> = encoded
                .contents
                .iter()
                .map(|(key, _)| key.as_str())
                .collect();
            let mut ids = $lookup(connection, principal, &keys, true).await?;
            let positions: HashMap<&str, usize> = keys
                .iter()
                .enumerate()
                .map(|(index, key)| (*key, index))
                .collect();
            let mut missing: Vec<(&str, &[u8])> = Vec::new();
            for ((key, _), id) in encoded.contents.iter().zip(&ids) {
                if id.is_none() {
                    missing.push((
                        key.as_str(),
                        contents
                            .get(key)
                            .context("missing prepared legacy history content")?
                            .as_slice(),
                    ));
                }
            }
            // Sorted speculative-insert order for concurrent legacy writers.
            missing.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for chunk in missing.chunks(200) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_chain_contents (principal, content_key, content) ",
                );
                query.push_values(chunk.iter(), |mut row, (key, content)| {
                    row.push_bind(principal).push_bind(*key).push_bind(*content);
                });
                query.push(
                    " ON CONFLICT (principal, content_key) DO NOTHING RETURNING id, content_key",
                );
                let inserted: Vec<(i64, String)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, key) in inserted {
                    if let Some(index) = positions.get(key.as_str()) {
                        ids[*index] = Some(id);
                    }
                }
            }
            let pending: Vec<&str> = keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| ids[index].is_none().then_some(*key))
                .collect();
            if !pending.is_empty() {
                let resolved = $lookup(connection, principal, &pending, true).await?;
                for (key, id) in pending.iter().zip(resolved) {
                    if let Some(id) = id {
                        ids[positions[*key]] = Some(id);
                    }
                }
            }
            let mut content_ids = ids
                .into_iter()
                .map(|id| id.context("missing legacy history content"))
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
    };
}
backend!(
    put_sqlite_prepared,
    put_sqlite_prepared_lookup,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>,
    ""
);
backend!(
    put_postgres_prepared,
    put_postgres_prepared_lookup,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    " FOR KEY SHARE"
);

#[cfg(test)]
pub(super) fn decode_envelope(bytes: &[u8]) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(&crate::storage_codec::decode(
        bytes,
    )?)?)
}
