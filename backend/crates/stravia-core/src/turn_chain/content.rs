//! Per-node semantic content externalization, principal-scoped content rows.
//!
//! A stored node payload is one descriptor:
//! `{"data": <payload with externalized slots null>, "slots":
//! [[json_pointer, ordinal], ...]}`. `turn_node_contents` holds the
//! authoritative `ordinal -> content_id` mapping; the descriptor carries no
//! content keys or ids. Every non-null semantic unit is externalized regardless
//! of size: empty, short and huge values share one rule and one deduplication
//! path. Raw compact serde JSON is the content identity; SHA-256 over the
//! fixed `stravia.turn-content.v1\0` prefix and those bytes produces a 32-byte
//! digest, and every digest row is owned by exactly one principal scope.
//!
//! Unit boundaries are unchanged: array elements under history-item keys are
//! opaque leaves, shared system/instruction/tool values are units, and vendor
//! profile containers are walked first and then externalized whole so their
//! parent slot restores before its nested slots.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::{Context, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use stravia_runtime_contract::turn_chain::{TurnNode, TurnNodeId, TurnNodeKind, TurnReadContext};

pub(super) type ContentDigest = [u8; 32];

/// Legacy domain-free hashes are not normalized content identities.
const CONTENT_DIGEST_DOMAIN: &[u8] = b"stravia.turn-content.v1\0";

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

/// One distinct content body. `bytes` starts as raw compact JSON and becomes
/// storage-codec bytes after `prepare_missing`; `encoded` records which form is
/// present so a stale existence hint can rebuild the body without a second
/// serialization.
pub(super) struct Content {
    digest: ContentDigest,
    bytes: Vec<u8>,
    encoded: bool,
}

impl Content {
    fn new(digest: ContentDigest, bytes: Vec<u8>) -> Self {
        Self {
            digest,
            bytes,
            encoded: false,
        }
    }

    #[cfg(test)]
    pub(super) fn digest(&self) -> ContentDigest {
        self.digest
    }

    /// Releases the body once the row is known to exist inside the caller's
    /// locked transaction; recovery before that point still needs it.
    fn release(&mut self) {
        self.bytes = Vec::new();
        self.encoded = true;
    }
}

pub(super) struct Extracted {
    pub data: Value,
    pub slots: Vec<(String, usize)>,
    pub contents: Vec<Content>,
}

pub(super) struct Encoded {
    pub payload: Vec<u8>,
    pub contents: Vec<Content>,
}

#[derive(Clone, Copy)]
pub(super) enum LookupScope<'a> {
    Id(i64),
    Principal(&'a str),
}

pub(super) struct StoredNode {
    pub node_pk: i64,
    pub scope_id: i64,
    /// 来自已核验 Principal/kind 的节点查询；只聚合数值引用，不重复传输正文。
    pub references: Vec<[i64; 3]>,
    pub id: TurnNodeId,
    pub kind: TurnNodeKind,
    pub parent_id: Option<TurnNodeId>,
    pub payload_version: u32,
    /// Storage-codec encoded descriptor as selected from `turn_node_payloads`.
    pub payload: Vec<u8>,
}

// 投影要求节点别名为 n。顺序由恢复器核验，SQL 不为每个节点建立聚合排序表。
macro_rules! reference_projection {
    (sqlite) => {
        "(SELECT json_group_array(json_array(c.scope_id,c.ordinal,c.content_id)) \
         FROM turn_node_contents c WHERE c.node_pk=n.node_pk) AS content_references"
    };
    (postgres) => {
        "COALESCE((SELECT json_agg(json_build_array(c.scope_id,c.ordinal,c.content_id)) \
         FROM turn_node_contents c WHERE c.node_pk=n.node_pk),'[]'::json) AS content_references"
    };
}
pub(super) use reference_projection;

/// Full 32-byte SHA-256 content identity with a fixed domain prefix.
pub(super) fn content_key(bytes: &[u8]) -> ContentDigest {
    Sha256::new()
        .chain_update(CONTENT_DIGEST_DOMAIN)
        .chain_update(bytes)
        .finalize()
        .into()
}

#[derive(Default)]
struct Extraction {
    slots: Vec<(String, usize)>,
    contents: Vec<Content>,
    index: HashMap<ContentDigest, usize>,
    /// Serialization buffer reused across occurrences; a duplicate value never
    /// keeps a second copy of its body.
    scratch: Vec<u8>,
}

fn push_pointer_token(path: &mut String, token: &str) {
    path.push('/');
    for character in token.chars() {
        match character {
            '~' => path.push_str("~0"),
            '/' => path.push_str("~1"),
            character => path.push(character),
        }
    }
}

/// Externalizes one semantic unit. Null is presence, not content, and stays
/// inline. The slot path is moved into the slot list exactly once.
fn externalize(value: &mut Value, path: &str, state: &mut Extraction) -> anyhow::Result<bool> {
    if value.is_null() {
        return Ok(false);
    }
    state.scratch.clear();
    serde_json::to_writer(&mut state.scratch, &*value)?;
    let digest = content_key(&state.scratch);
    let ordinal = if let Some(ordinal) = state.index.get(&digest).copied() {
        ordinal
    } else {
        let ordinal = state.contents.len();
        state.index.insert(digest, ordinal);
        state
            .contents
            .push(Content::new(digest, std::mem::take(&mut state.scratch)));
        ordinal
    };
    state.slots.push((path.to_owned(), ordinal));
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
        let _ = write!(path, "/{index}");
        externalize(item, path.as_str(), state)?;
        path.truncate(base);
    }
    Ok(())
}

fn walk(value: &mut Value, path: &mut String, state: &mut Extraction) -> anyhow::Result<()> {
    match value {
        Value::Object(object) => {
            for (name, child) in object.iter_mut() {
                let base = path.len();
                push_pointer_token(path, name);
                let name = name.as_str();
                if ITEM_ARRAY_KEYS.contains(&name) && child.is_array() {
                    externalize_items(
                        child.as_array_mut().context("history item array")?,
                        path,
                        state,
                    )?;
                } else if SCALAR_KEYS.contains(&name) {
                    externalize(child, path.as_str(), state)?;
                } else if PROFILE_KEYS.contains(&name) {
                    // Inner slots are discovered first but the container must
                    // restore before them: keep its entry ahead of its own
                    // subtree so pointer resolution always descends through a
                    // materialized parent.
                    let first = state.slots.len();
                    walk(child, path, state)?;
                    if externalize(child, path.as_str(), state)? && state.slots.len() > first + 1 {
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
                let _ = write!(path, "/{index}");
                walk(child, path, state)?;
                path.truncate(base);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Extracts every non-null semantic unit. Repeated raw bytes share one
/// `Content`; every occurrence keeps its own slot so order, repeats, escapes
/// and nulls survive restore.
pub(super) fn extract(mut payload: Value) -> anyhow::Result<Extracted> {
    let mut state = Extraction::default();
    let mut path = String::new();
    walk(&mut payload, &mut path, &mut state)?;
    Ok(Extracted {
        data: payload,
        slots: state.slots,
        contents: state.contents,
    })
}

#[derive(serde::Serialize)]
struct DescriptorRef<'a> {
    data: &'a Value,
    slots: &'a [(String, usize)],
}

/// Encodes the v3 descriptor. The ordinal array is the only place paths and
/// ordinal numbers live; content ids stay in the relation table.
pub(super) fn encode_descriptor(
    data: &Value,
    slots: &[(String, usize)],
) -> anyhow::Result<Vec<u8>> {
    let descriptor = serde_json::to_vec(&DescriptorRef { data, slots })?;
    crate::storage_codec::encode(std::borrow::Cow::Owned(descriptor))
}

pub(super) fn encode(payload: Value) -> anyhow::Result<Encoded> {
    let Extracted {
        data,
        slots,
        contents,
    } = extract(payload)?;
    let payload = encode_descriptor(&data, &slots)?;
    Ok(Encoded { payload, contents })
}

/// Compresses bodies for content ids that were missing at the last existence
/// probe. Probes before the write transaction are hints only, so existing ids
/// keep their body for recovery until the authoritative locked lookup.
pub(super) async fn prepare_missing(
    contents: &mut [Content],
    ids: &[Option<i64>],
) -> anyhow::Result<()> {
    ensure!(
        contents.len() == ids.len(),
        "history content id count mismatch"
    );
    for (content, id) in contents.iter().zip(ids) {
        if id.is_none() {
            ensure!(!content.bytes.is_empty(), "missing history content body");
        }
    }
    let pending: Vec<_> = contents
        .iter_mut()
        .zip(ids)
        .enumerate()
        .filter(|(_, (content, id))| id.is_none() && !content.encoded)
        .map(|(index, (content, _))| (index, std::mem::take(&mut content.bytes)))
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
        contents[index].bytes = bytes;
        contents[index].encoded = true;
    }
    Ok(())
}

/// Content positions grouped by digest without allocating per digest: the
/// first occurrence covers the common unique case and only genuine repeats get
/// a small overflow vector. Duplicate digests are legal for lookup/intern and
/// all positions resolve to the same row; the link relation itself requires
/// distinct ordinals.
struct DigestPositions {
    first: HashMap<ContentDigest, usize>,
    duplicates: HashMap<ContentDigest, Vec<usize>>,
}

impl DigestPositions {
    fn new(contents: &[Content]) -> Self {
        let mut first = HashMap::with_capacity(contents.len());
        let mut duplicates: HashMap<ContentDigest, Vec<usize>> = HashMap::new();
        for (index, content) in contents.iter().enumerate() {
            match first.entry(content.digest) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(index);
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    duplicates.entry(*entry.key()).or_default().push(index);
                }
            }
        }
        Self { first, duplicates }
    }

    fn digests(&self) -> impl Iterator<Item = ContentDigest> + '_ {
        self.first.keys().copied()
    }

    fn observe(&self, ids: &mut [Option<i64>], digest: &[u8], id: i64) -> anyhow::Result<()> {
        let digest: ContentDigest = digest
            .try_into()
            .context("invalid stored history content digest")?;
        if let Some(index) = self.first.get(&digest) {
            ids[*index] = Some(id);
            if let Some(indices) = self.duplicates.get(&digest) {
                for index in indices {
                    ids[*index] = Some(id);
                }
            }
        }
        Ok(())
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    data: Value,
    slots: Vec<(String, usize)>,
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

macro_rules! backend {
    ($put:ident, $lookup:ident, $intern:ident, $link:ident, $restore:ident, $restore_context:ident, $db:ty, $conn:ty, $lock:literal) => {
        /// Links an already-interned node to distinct content ids. The ids
        /// array order is the descriptor ordinal. Callers hold the write
        /// transaction; PostgreSQL callers hold the contents table lock.
        pub(super) async fn $link(
            connection: &mut $conn,
            node_pk: i64,
            scope_id: i64,
            ids: &[i64],
        ) -> anyhow::Result<()> {
            if ids.is_empty() {
                return Ok(());
            }
            let mut seen = HashSet::with_capacity(ids.len());
            for id in ids {
                ensure!(
                    seen.insert(*id),
                    "duplicate history content identity at multiple ordinals"
                );
            }
            i64::try_from(ids.len()).context("history content ordinal overflow")?;
            for (base, chunk) in ids.chunks(200).enumerate() {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_node_contents (node_pk, scope_id, ordinal, content_id) ",
                );
                query.push_values(chunk.iter().enumerate(), |mut row, (offset, id)| {
                    let ordinal = base * 200 + offset;
                    row.push_bind(node_pk)
                        .push_bind(scope_id)
                        .push_bind(ordinal as i64)
                        .push_bind(*id);
                });
                query.build().execute(&mut *connection).await?;
            }
            Ok(())
        }

        /// Existence probe. Without the caller's transaction locks the result
        /// is only a hint; `lock` adds a row lock on PostgreSQL.
        pub(super) async fn $lookup(
            connection: &mut $conn,
            scope: LookupScope<'_>,
            contents: &[Content],
            lock: bool,
        ) -> anyhow::Result<Vec<Option<i64>>> {
            let mut ids = vec![None; contents.len()];
            if contents.is_empty() {
                return Ok(ids);
            }
            let positions = DigestPositions::new(contents);
            let mut digests: Vec<ContentDigest> = positions.digests().collect();
            digests.sort_unstable();
            for chunk in digests.chunks(400) {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT id, digest FROM turn_contents WHERE scope_id = ",
                );
                match scope {
                    LookupScope::Id(id) => { query.push_bind(id); }
                    LookupScope::Principal(principal) => {
                        query.push("(SELECT id FROM turn_scopes WHERE principal = ")
                            .push_bind(principal).push(")");
                    }
                }
                query.push(" AND digest IN (");
                let mut list = query.separated(",");
                for digest in chunk {
                    list.push_bind(digest.as_slice());
                }
                list.push_unseparated(")");
                // Deterministic row-lock order for concurrent commits.
                query.push(" ORDER BY digest");
                if lock {
                    query.push($lock);
                }
                let rows: Vec<(i64, Vec<u8>)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, digest) in rows {
                    positions.observe(&mut ids, &digest, id)?;
                }
            }
            Ok(ids)
        }

        /// Authoritative locked lookup plus missing-row insertion. Returns one
        /// id per input digest position in input order. Existing bodies are
        /// released only after the locked SELECT protects their rows; stale
        /// hint-missing bodies are compressed before ordered inserts.
        pub(super) async fn $intern(
            connection: &mut $conn,
            scope_id: i64,
            contents: &mut [Content],
        ) -> anyhow::Result<Vec<i64>> {
            let mut ids = $lookup(connection, LookupScope::Id(scope_id), contents, true).await?;
            for (content, id) in contents.iter_mut().zip(&ids) {
                if id.is_some() {
                    content.release();
                }
            }
            prepare_missing(contents, &ids).await?;

            let positions = DigestPositions::new(contents);
            let mut missing: Vec<usize> = ids
                .iter()
                .enumerate()
                .filter_map(|(index, id)| id.is_none().then_some(index))
                .collect();
            // Deterministic insert order: concurrent writers take speculative
            // insertion locks in the same digest order.
            missing.sort_unstable_by(|left, right| {
                contents[*left].digest.cmp(&contents[*right].digest)
            });
            let mut unique: Vec<usize> = Vec::with_capacity(missing.len());
            for index in missing {
                if unique
                    .last()
                    .is_none_or(|previous| contents[*previous].digest != contents[index].digest)
                {
                    unique.push(index);
                }
            }
            for chunk in unique.chunks(200) {
                for index in chunk {
                    ensure!(
                        contents[*index].encoded && !contents[*index].bytes.is_empty(),
                        "missing prepared history content"
                    );
                }
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_contents (scope_id, digest, content) ",
                );
                query.push_values(chunk.iter(), |mut row, index| {
                    row.push_bind(scope_id)
                        .push_bind(contents[*index].digest.as_slice())
                        .push_bind(contents[*index].bytes.as_slice());
                });
                query.push(
                    " ON CONFLICT (scope_id, digest) DO NOTHING RETURNING id, digest",
                );
                let inserted: Vec<(i64, Vec<u8>)> =
                    query.build_query_as().fetch_all(&mut *connection).await?;
                for (id, digest) in inserted {
                    positions.observe(&mut ids, &digest, id)?;
                }
            }

            // A row that won the conflict but was not returned is resolved by
            // one targeted locked lookup; no retry loop and no meaningless
            // UPDATE.
            let unresolved: Vec<usize> = ids
                .iter()
                .enumerate()
                .filter_map(|(index, id)| id.is_none().then_some(index))
                .collect();
            if !unresolved.is_empty() {
                let mut keys: Vec<ContentDigest> = unresolved
                    .iter()
                    .map(|index| contents[*index].digest)
                    .collect();
                keys.sort_unstable();
                keys.dedup();
                for chunk in keys.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, digest FROM turn_contents WHERE scope_id = ",
                    );
                    query.push_bind(scope_id).push(" AND digest IN (");
                    let mut list = query.separated(",");
                    for digest in chunk {
                        list.push_bind(digest.as_slice());
                    }
                    list.push_unseparated(")");
                    query.push(" ORDER BY digest");
                    query.push($lock);
                    let rows: Vec<(i64, Vec<u8>)> =
                        query.build_query_as().fetch_all(&mut *connection).await?;
                    for (id, digest) in rows {
                        positions.observe(&mut ids, &digest, id)?;
                    }
                }
            }
            let ids = ids
                .into_iter()
                .map(|id| id.context("missing history content"))
                .collect::<anyhow::Result<Vec<_>>>()?;
            for content in contents.iter_mut() {
                content.release();
            }
            Ok(ids)
        }

        /// Intern then link convenience for a node that already has its
        /// `turn_nodes` row.
        pub(super) async fn $put(
            connection: &mut $conn,
            node_pk: i64,
            scope_id: i64,
            contents: &mut [Content],
        ) -> anyhow::Result<()> {
            let ids = $intern(connection, scope_id, contents).await?;
            $link(connection, node_pk, scope_id, &ids).await
        }

        pub(super) async fn $restore(
            connection: &mut $conn,
            nodes: Vec<StoredNode>,
        ) -> anyhow::Result<Vec<TurnNode>> {
            $restore_context(connection, nodes, None, |node, _| Ok(node)).await
        }

        /// 节点和 ordinal 映射来自同一条已核验归属的查询；只按缺失数值 ID
        /// 批读正文，每份正文验证后再恢复，避免共享引用重复传输大正文。
        pub(super) async fn $restore_context<T>(
            connection: &mut $conn,
            mut nodes: Vec<StoredNode>,
            mut context: Option<&mut TurnReadContext>,
            mut finish: impl FnMut(TurnNode, Option<&TurnReadContext>) -> anyhow::Result<T> + Send,
        ) -> anyhow::Result<Vec<T>> {
            if nodes.is_empty() {
                return Ok(Vec::new());
            }
            let scope_id = nodes[0].scope_id;
            let mut identities = HashSet::with_capacity(nodes.len());
            let mut mappings = Vec::with_capacity(nodes.len());
            for node in &mut nodes {
                ensure!(
                    identities.insert(node.node_pk),
                    "duplicate history node identity"
                );
                ensure!(node.scope_id == scope_id, "history node scope mismatch");
                let mut references = std::mem::take(&mut node.references);
                references.sort_unstable_by_key(|reference| reference[1]);
                let mut mapping = Vec::with_capacity(references.len());
                for [row_scope, ordinal, content_id] in references {
                    ensure!(scope_id == row_scope, "history content scope mismatch");
                    let ordinal = usize::try_from(ordinal)
                        .context("invalid history content ordinal")?;
                    ensure!(
                        ordinal == mapping.len(),
                        "non-contiguous history content ordinal"
                    );
                    mapping.push(content_id);
                }
                mappings.push(mapping);
            }

            let mut plans = Vec::with_capacity(nodes.len());
            let mut uses: HashMap<i64, usize> = HashMap::new();
            for (index, node) in nodes.iter_mut().enumerate() {
                let encoded = std::mem::take(&mut node.payload);
                let raw = crate::storage_codec::decode(&encoded)?;
                let descriptor: Descriptor = serde_json::from_slice(&raw)?;
                drop(raw);
                drop(encoded);
                let mapping = &mappings[index];
                let mut seen = vec![false; mapping.len()];
                for (_, ordinal) in &descriptor.slots {
                    ensure!(
                        *ordinal < mapping.len(),
                        "history slot ordinal out of range"
                    );
                    seen[*ordinal] = true;
                    *uses.entry(mapping[*ordinal]).or_default() += 1;
                }
                ensure!(
                    seen.iter().all(|referenced| *referenced),
                    "unreferenced history content ordinal"
                );
                plans.push((descriptor.data, descriptor.slots));
            }

            let mut needed: Vec<i64> = uses.keys().copied().collect();
            needed.sort_unstable();
            let mut values: HashMap<i64, RestoredContent> =
                HashMap::with_capacity(needed.len());
            if let Some(context) = context.as_deref() {
                for id in &needed {
                    if let Some(value) = context.cached_content(*id) {
                        values.insert(*id, RestoredContent::Shared(value));
                    }
                }
            }
            let missing: Vec<i64> = needed
                .iter()
                .copied()
                .filter(|id| !values.contains_key(id))
                .collect();
            let mut content_reads = 0u64;
            if !missing.is_empty() {
                for chunk in missing.chunks(256) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, digest, content FROM turn_contents WHERE scope_id = ",
                    );
                    query.push_bind(scope_id).push(" AND id IN (");
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(") ORDER BY id");
                    let rows: Vec<(i64, Vec<u8>, Vec<u8>)> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    for (id, digest, encoded) in rows {
                        let digest: ContentDigest = digest
                            .as_slice()
                            .try_into()
                            .context("invalid stored history content digest")?;
                        let raw = crate::storage_codec::decode(&encoded)?;
                        ensure!(content_key(&raw) == digest, "damaged history content");
                        let value = serde_json::from_slice::<Value>(&raw)?;
                        let admit = context
                            .as_deref()
                            .is_some_and(|context| context.admission_open());
                        let value = if admit {
                            let value = std::sync::Arc::new(value);
                            let bytes = crate::memory::json_retained_bytes(&value)
                                .saturating_add(std::mem::size_of::<(i64, (std::sync::Arc<Value>, usize))>())
                                .saturating_add(2 * std::mem::size_of::<usize>());
                            context
                                .as_deref_mut()
                                .expect("open content admission")
                                .stage_content(id, value.clone(), bytes);
                            RestoredContent::Shared(value)
                        } else {
                            RestoredContent::Owned(value)
                        };
                        values.insert(id, value);
                        content_reads += 1;
                    }
                }
            }
            for id in &needed {
                ensure!(
                    values.contains_key(id),
                    "missing history content"
                );
            }
            drop(needed);

            let mut restored = Vec::with_capacity(nodes.len());
            for ((stored, (mut data, plan)), mapping) in
                nodes.into_iter().zip(plans).zip(mappings)
            {
                for (pointer, ordinal) in plan {
                    let content_id = mapping[ordinal];
                    let slot = data
                        .pointer_mut(&pointer)
                        .context("invalid history slot")?;
                    ensure!(slot.is_null(), "history slot overwrites data");
                    let remaining = uses
                        .get_mut(&content_id)
                        .context("missing history content")?;
                    ensure!(*remaining > 0, "history content use underflow");
                    *remaining -= 1;
                    // Each JSON slot needs an independent Value, but the final
                    // occurrence consumes the uncached decoded tree instead of
                    // cloning it and immediately dropping the original.
                    *slot = if *remaining == 0 {
                        values
                            .remove(&content_id)
                            .context("missing history content")?
                            .into_value()
                    } else {
                        values
                            .get(&content_id)
                            .context("missing history content")?
                            .value()
                            .clone()
                    };
                }
                let node = TurnNode {
                    id: stored.id,
                    kind: stored.kind,
                    parent_id: stored.parent_id,
                    payload_version: stored.payload_version,
                    payload: data,
                };
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
    lookup_sqlite,
    intern_sqlite,
    link_sqlite,
    restore_sqlite,
    restore_sqlite_context,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>,
    ""
);
backend!(
    put_postgres,
    lookup_postgres,
    intern_postgres,
    link_postgres,
    restore_postgres,
    restore_postgres_context,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    " FOR KEY SHARE"
);
