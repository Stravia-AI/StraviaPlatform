//! 节点载荷的正文外置。一条 stored node payload 是一个 descriptor：
//! `{"data": <正文位置为 null 的载荷>, "slots": [...]}`；`turn_node_contents`
//! 持有权威的 `ordinal -> content_id` 映射，descriptor 不携带内容 ID。
//!
//! 正文单元由 IR 的 [`item_content_units`] 唯一确定：历史条目先整体作为 JSON
//! 信封单元，再把它内部的文本/JSON 正文取成嵌套槽。数组元素按原字节保存，
//! 类型化解析只用于确定位置；解析失败时整条保持不透明 JSON 单元，绝不重排或
//! 改写原字节。文本单元保存原始 UTF-8，跨协议投影文本因此可以逐字节往返。
//!
//! 槽位顺序保证父容器先于嵌套槽恢复：信封槽先于自己的子槽；vendor profile
//! 容器先于其内部槽。共享单元的恢复只在最后一次使用处移动值，其余位置克隆。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::{Context, ensure};
use serde_json::Value;
use stravia_runtime_contract::protocol::ir::AiItem;
use stravia_runtime_contract::protocol::ir::content::{
    ContentFormat, ContentUnitBody, item_content_units,
};
use stravia_runtime_contract::turn_chain::{TurnNode, TurnNodeId, TurnNodeKind, TurnReadContext};

use crate::content_store::{self, Content, ContentDigest, Descriptor, Slot};

pub(super) use crate::content_store::LookupScope;
pub(super) use crate::content_store::prepare_missing;

/// 数组元素是原始历史条目。元素整体是信封单元，内部正文由 IR 定义。
const ITEM_ARRAY_KEYS: &[&str] = &[
    "items",
    "input",
    "output",
    "messages",
    "transcript",
    "client_output",
];
/// 整段是共享配置 JSON 单元。
const SCALAR_KEYS: &[&str] = &["system", "effective_system", "instructions", "tools"];
/// vendor 容器先遍历内部槽，再整体外置；父槽排在内部槽之前。
const PROFILE_KEYS: &[&str] = &[
    "__open_responses_response_profile",
    "__open_responses_effective_request",
];

pub(super) struct Extracted {
    pub data: Value,
    pub slots: Vec<Slot>,
    pub contents: Vec<Content>,
}

pub(crate) struct Encoded {
    pub(crate) payload: Vec<u8>,
    pub(crate) contents: Vec<Content>,
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

#[derive(Default)]
struct Extraction {
    slots: Vec<Slot>,
    contents: Vec<Content>,
    index: HashMap<ContentDigest, usize>,
    /// 被移动进 `contents` 后重新分配；重复正文不会保留第二份。
    scratch: Vec<u8>,
}

impl Extraction {
    fn intern(&mut self, digest: ContentDigest, format: ContentFormat, raw: Vec<u8>) -> usize {
        if let Some(ordinal) = self.index.get(&digest) {
            return *ordinal;
        }
        let ordinal = self.contents.len();
        self.index.insert(digest, ordinal);
        self.contents
            .push(Content::with_digest(digest, format, raw));
        ordinal
    }

    fn intern_text(&mut self, text: &str) -> usize {
        let digest = content_store::content_digest(ContentFormat::Text, text.as_bytes());
        if let Some(ordinal) = self.index.get(&digest) {
            return *ordinal;
        }
        self.intern(digest, ContentFormat::Text, text.as_bytes().to_vec())
    }

    /// 把 `target` 序列化为紧凑 JSON 单元并置空；null 是「无内容」，保持内联。
    fn externalize_json(&mut self, target: &mut Value, path: &str) -> anyhow::Result<bool> {
        if target.is_null() {
            return Ok(false);
        }
        self.scratch.clear();
        serde_json::to_writer(&mut self.scratch, &*target)?;
        let raw = std::mem::take(&mut self.scratch);
        let digest = content_store::content_digest(ContentFormat::Json, &raw);
        let ordinal = self.intern(digest, ContentFormat::Json, raw);
        self.slots.push(Slot::json(path.to_owned(), ordinal));
        *target = Value::Null;
        Ok(true)
    }
}

/// 待外置的正文单元；信封序列化后再按顺序入槽。
enum Pending {
    Text {
        pointer: String,
        prefix: Option<String>,
        text: String,
        suffix: String,
    },
    Json {
        pointer: String,
        raw: Vec<u8>,
    },
}

/// 一个历史条目：正文位置置空后的条目信封是一个 JSON 单元，正文单元按 IR 正文
/// 规则逐个外置。类型化解析失败时整条保持不透明 JSON 单元，原字节原样保留。
fn externalize_item(target: &mut Value, path: &str, state: &mut Extraction) -> anyhow::Result<()> {
    let mut pending = Vec::new();
    state.scratch.clear();
    serde_json::to_writer(&mut state.scratch, &*target)?;
    if let Ok(item) = serde_json::from_slice::<AiItem>(&state.scratch) {
        for unit in item_content_units(&item) {
            let Some(body) = target.pointer_mut(&unit.pointer) else {
                continue;
            };
            if body.is_null() {
                continue;
            }
            let pointer = format!("{path}{}", unit.pointer);
            match unit.body {
                ContentUnitBody::Text(text) => {
                    // 跨协议投影文本拆成字面量加共享正文；拼接校验失败则整串存储。
                    pending.push(
                        match crate::history_marker::syntax::split_projected_text(text) {
                            Some((prefix, text, suffix)) => Pending::Text {
                                pointer,
                                prefix: Some(prefix),
                                text,
                                suffix,
                            },
                            None => Pending::Text {
                                pointer,
                                prefix: None,
                                text: text.to_owned(),
                                suffix: String::new(),
                            },
                        },
                    );
                }
                ContentUnitBody::Json(_) => {
                    pending.push(Pending::Json {
                        pointer,
                        raw: serde_json::to_vec(&*body)?,
                    });
                }
            }
            *body = Value::Null;
        }
    }
    state.scratch.clear();
    serde_json::to_writer(&mut state.scratch, &*target)?;
    let raw = std::mem::take(&mut state.scratch);
    let envelope = state.intern(
        content_store::content_digest(ContentFormat::Json, &raw),
        ContentFormat::Json,
        raw,
    );
    state.slots.push(Slot::json(path.to_owned(), envelope));
    for unit in pending {
        match unit {
            Pending::Text {
                pointer,
                prefix: Some(prefix),
                text,
                suffix,
            } => {
                let ordinal = state.intern_text(&text);
                state
                    .slots
                    .push(Slot::concat(pointer, ordinal, prefix, suffix));
            }
            Pending::Text {
                pointer,
                prefix: None,
                text,
                ..
            } => {
                let ordinal = state.intern_text(&text);
                state.slots.push(Slot::text(pointer, ordinal));
            }
            Pending::Json { pointer, raw } => {
                let digest = content_store::content_digest(ContentFormat::Json, &raw);
                let ordinal = state.intern(digest, ContentFormat::Json, raw);
                state.slots.push(Slot::json(pointer, ordinal));
            }
        }
    }
    *target = Value::Null;
    Ok(())
}

fn walk(target: &mut Value, path: &mut String, state: &mut Extraction) -> anyhow::Result<()> {
    match target {
        Value::Object(object) => {
            for (name, child) in object.iter_mut() {
                let base = path.len();
                push_pointer_token(path, name);
                let key = name.as_str();
                if ITEM_ARRAY_KEYS.contains(&key) && child.is_array() {
                    let array = child.as_array_mut().context("history item array")?;
                    for (index, item) in array.iter_mut().enumerate() {
                        let item_base = path.len();
                        let _ = write!(path, "/{index}");
                        externalize_item(item, path.as_str(), state)?;
                        path.truncate(item_base);
                    }
                } else if SCALAR_KEYS.contains(&key) {
                    state.externalize_json(child, path.as_str())?;
                } else if PROFILE_KEYS.contains(&key) {
                    // 内部槽先被发现，但父容器必须先恢复：把容器槽移到子树之前。
                    let first = state.slots.len();
                    walk(child, path, state)?;
                    if state.externalize_json(child, path.as_str())?
                        && state.slots.len() > first + 1
                    {
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

/// 外置全部正文单元；重复正文共享一个 `Content`，每次出现保留独立槽位，
/// 因此顺序、重复、转义与 null 都能还原。
pub(super) fn extract(payload: Value) -> anyhow::Result<Extracted> {
    let mut state = Extraction::default();
    let mut path = String::new();
    let mut payload = payload;
    walk(&mut payload, &mut path, &mut state)?;
    Ok(Extracted {
        data: payload,
        slots: state.slots,
        contents: state.contents,
    })
}

pub(crate) fn encode(payload: Value) -> anyhow::Result<Encoded> {
    let Extracted {
        data,
        slots,
        contents,
    } = extract(payload)?;
    let payload = content_store::encode_descriptor(&data, &slots)?;
    Ok(Encoded { payload, contents })
}

macro_rules! link_backend {
    ($name:ident, $db:ty, $conn:ty) => {
        /// 已 intern 的节点与去重后的正文 ID 建引用。ID 顺序就是 descriptor
        /// 的 ordinal；调用方持有写事务。
        pub(super) async fn $name(
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
    };
}
link_backend!(link_sqlite, sqlx::Sqlite, sqlx::SqliteConnection<'_>);
link_backend!(link_postgres, sqlx::Postgres, sqlx::PgConnection<'_>);

pub(super) async fn read_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    scope_id: i64,
    ids: &[i64],
) -> anyhow::Result<Vec<content_store::Body>> {
    content_store::sqlite::read(connection, scope_id, ids).await
}

pub(super) async fn read_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    scope_id: i64,
    ids: &[i64],
) -> anyhow::Result<Vec<content_store::Body>> {
    content_store::postgres::read(connection, scope_id, ids).await
}

pub(super) async fn lookup_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    scope: LookupScope<'_>,
    contents: &[Content],
    lock: bool,
) -> anyhow::Result<Vec<Option<i64>>> {
    content_store::sqlite::lookup(connection, scope, contents, lock).await
}

pub(super) async fn lookup_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    scope: LookupScope<'_>,
    contents: &[Content],
    lock: bool,
) -> anyhow::Result<Vec<Option<i64>>> {
    content_store::postgres::lookup(connection, scope, contents, lock).await
}

pub(super) async fn intern_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    scope_id: i64,
    contents: &mut [Content],
) -> anyhow::Result<Vec<i64>> {
    content_store::sqlite::intern(connection, scope_id, contents).await
}

pub(super) async fn intern_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    scope_id: i64,
    contents: &mut [Content],
) -> anyhow::Result<Vec<i64>> {
    content_store::postgres::intern(connection, scope_id, contents).await
}

pub(super) async fn put_sqlite(
    connection: &mut sqlx::SqliteConnection<'_>,
    node_pk: i64,
    scope_id: i64,
    contents: &mut [Content],
) -> anyhow::Result<()> {
    let ids = intern_sqlite(connection, scope_id, contents).await?;
    link_sqlite(connection, node_pk, scope_id, &ids).await
}

pub(super) async fn put_postgres(
    connection: &mut sqlx::PgConnection<'_>,
    node_pk: i64,
    scope_id: i64,
    contents: &mut [Content],
) -> anyhow::Result<()> {
    let ids = intern_postgres(connection, scope_id, contents).await?;
    link_postgres(connection, node_pk, scope_id, &ids).await
}

/// 恢复前的结构校验：归属、ordinal 连续、descriptor 槽位可解析。
struct RestorePlan {
    scope_id: i64,
    plans: Vec<(Value, Vec<Slot>)>,
    mappings: Vec<Vec<i64>>,
    /// 每份正文的使用次数；最后一次使用直接移动值。
    uses: HashMap<i64, usize>,
    needed: Vec<i64>,
}

fn plan_restore(nodes: &mut [StoredNode]) -> anyhow::Result<RestorePlan> {
    let scope_id = nodes[0].scope_id;
    let mut identities = HashSet::with_capacity(nodes.len());
    let mut mappings = Vec::with_capacity(nodes.len());
    for node in nodes.iter_mut() {
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
            let ordinal = usize::try_from(ordinal).context("invalid history content ordinal")?;
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
        let descriptor: Descriptor = content_store::decode_descriptor(&encoded)?;
        let mapping = &mappings[index];
        let mut seen = vec![false; mapping.len()];
        for slot in &descriptor.slots {
            ensure!(
                slot.ordinal < mapping.len(),
                "history slot ordinal out of range"
            );
            seen[slot.ordinal] = true;
            *uses.entry(mapping[slot.ordinal]).or_default() += 1;
        }
        ensure!(
            seen.iter().all(|referenced| *referenced),
            "unreferenced history content ordinal"
        );
        plans.push((descriptor.data, descriptor.slots));
    }
    let mut needed: Vec<i64> = uses.keys().copied().collect();
    needed.sort_unstable();
    Ok(RestorePlan {
        scope_id,
        plans,
        mappings,
        uses,
        needed,
    })
}

enum RestoredContent {
    Owned(Value),
    Shared(std::sync::Arc<Value>),
}

impl RestoredContent {
    fn as_value(&self) -> &Value {
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

/// 把读取到的正文并入缓存。文本正文在缓存中表示为 `Value::String`，
/// 值本身仍是原始 UTF-8，不做 JSON 转义。
fn load_bodies(
    loaded: &mut HashMap<i64, RestoredContent>,
    bodies: Vec<content_store::Body>,
    context: &mut Option<&mut TurnReadContext>,
) -> anyhow::Result<()> {
    for body in bodies {
        let value = body.value()?;
        let admit = context
            .as_deref()
            .is_some_and(|context| context.admission_open());
        let restored = if admit {
            let value = std::sync::Arc::new(value);
            let bytes = crate::memory::json_retained_bytes(&value)
                .saturating_add(std::mem::size_of::<(i64, (std::sync::Arc<Value>, usize))>())
                .saturating_add(2 * std::mem::size_of::<usize>());
            context
                .as_deref_mut()
                .expect("open content admission")
                .stage_content(body.id, value.clone(), bytes);
            RestoredContent::Shared(value)
        } else {
            RestoredContent::Owned(value)
        };
        loaded.insert(body.id, restored);
    }
    Ok(())
}

fn restore_payload(
    data: &mut Value,
    slots: &[Slot],
    mapping: &[i64],
    uses: &mut HashMap<i64, usize>,
    loaded: &mut HashMap<i64, RestoredContent>,
) -> anyhow::Result<()> {
    content_store::restore_slots(data, slots, |ordinal| {
        let content_id = *mapping
            .get(ordinal)
            .context("history slot ordinal out of range")?;
        let remaining = uses
            .get_mut(&content_id)
            .context("missing history content")?;
        ensure!(*remaining > 0, "history content use underflow");
        *remaining -= 1;
        if *remaining == 0 {
            Ok(loaded
                .remove(&content_id)
                .context("missing history content")?
                .into_value())
        } else {
            Ok(loaded
                .get(&content_id)
                .context("missing history content")?
                .as_value()
                .clone())
        }
    })
}

fn apply_plan(
    nodes: Vec<StoredNode>,
    plan: RestorePlan,
    loaded: &mut HashMap<i64, RestoredContent>,
) -> anyhow::Result<Vec<TurnNode>> {
    let RestorePlan {
        plans,
        mappings,
        mut uses,
        needed,
        ..
    } = plan;
    for id in &needed {
        ensure!(loaded.contains_key(id), "missing history content");
    }
    let mut restored = Vec::with_capacity(nodes.len());
    for ((stored, (mut data, slots)), mapping) in nodes.into_iter().zip(plans).zip(mappings) {
        restore_payload(&mut data, &slots, &mapping, &mut uses, loaded)?;
        restored.push(TurnNode {
            id: stored.id,
            kind: stored.kind,
            parent_id: stored.parent_id,
            payload_version: stored.payload_version,
            payload: data,
        });
    }
    Ok(restored)
}

macro_rules! restore_backend {
    ($read:ident, $restore:ident, $restore_context:ident, $conn:ty) => {
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
            let plan = plan_restore(&mut nodes)?;
            let mut loaded: HashMap<i64, RestoredContent> = HashMap::with_capacity(plan.needed.len());
            if let Some(context) = context.as_deref() {
                for id in &plan.needed {
                    if let Some(value) = context.cached_content(*id) {
                        loaded.insert(
                            *id,
                            RestoredContent::Shared(value),
                        );
                    }
                }
            }
            let missing: Vec<i64> = plan
                .needed
                .iter()
                .copied()
                .filter(|id| !loaded.contains_key(id))
                .collect();
            let read_count = missing.len();
            let bodies = $read(connection, plan.scope_id, &missing).await?;
            load_bodies(&mut loaded, bodies, &mut context)?;
            let nodes = apply_plan(nodes, plan, &mut loaded)?;
            let mut output = Vec::with_capacity(nodes.len());
            for node in nodes {
                output.push(finish(node, context.as_deref())?);
            }
            tracing::info!(target: "stravia::perf", content_read_count = read_count,
                reference_count = read_count, "turn_chain.content.restore");
            Ok(output)
        }
    };
}
restore_backend!(
    read_sqlite,
    restore_sqlite,
    restore_sqlite_context,
    sqlx::SqliteConnection<'_>
);
restore_backend!(
    read_postgres,
    restore_postgres,
    restore_postgres_context,
    sqlx::PgConnection<'_>
);

/// 迁移只保留当前节点批次；不能让调用方意外把整库送入 blocking 队列。
const UPGRADE_NODE_BATCH: usize = 128;
/// 沿用共享正文 GC 的 8 MiB 工作批次；超大单元独占一批，不拒绝合法大正文。
const UPGRADE_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// 同时在途的 blocking 工作至多为可用 CPU 数且不超过 8；输入所有权只移动，
/// 不为每个 worker 克隆整个节点批次。序号在完成时恢复，不依赖任务调度顺序。
async fn upgrade_cpu<T: Send + 'static, U: Send + 'static>(
    jobs: Vec<T>,
    work: fn(T) -> anyhow::Result<U>,
) -> anyhow::Result<Vec<U>> {
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8);
    let mut pending = jobs.into_iter().enumerate();
    let mut running = tokio::task::JoinSet::new();
    let mut completed = Vec::new();
    loop {
        while running.len() < workers {
            let Some((index, job)) = pending.next() else {
                break;
            };
            running.spawn_blocking(move || work(job).map(|value| (index, value)));
        }
        let Some(result) = running.join_next().await else {
            break;
        };
        completed.push(result.context("joining retention restore worker")??);
    }
    completed.sort_unstable_by_key(|(index, _)| *index);
    Ok(completed.into_iter().map(|(_, value)| value).collect())
}

type UpgradeNode = (usize, Vec<StoredNode>, RestorePlan);

fn plan_upgrade_node((index, node): (usize, StoredNode)) -> anyhow::Result<UpgradeNode> {
    let mut nodes = vec![node];
    let plan = plan_restore(&mut nodes)?;
    Ok((index, nodes, plan))
}

fn decode_upgrade_body(row: content_store::EncodedBody) -> anyhow::Result<(i64, RestoredContent)> {
    let body = content_store::decode_body(row)?;
    let value = match body.format {
        ContentFormat::Text => {
            Value::String(String::from_utf8(body.bytes).context("invalid stored text content")?)
        }
        ContentFormat::Json => body.json()?,
    };
    Ok((body.id, RestoredContent::Shared(std::sync::Arc::new(value))))
}

fn apply_upgrade_node(
    (index, nodes, plan, mut loaded): (
        usize,
        Vec<StoredNode>,
        RestorePlan,
        HashMap<i64, RestoredContent>,
    ),
) -> anyhow::Result<(usize, TurnNode)> {
    let node = apply_plan(nodes, plan, &mut loaded)?
        .pop()
        .context("missing restored history node")?;
    Ok((index, node))
}

macro_rules! restore_upgrade_backend {
    ($restore:ident, $backend:ident, $conn:ty) => {
        /// 启动迁移专用：单连接读取、按 scope 共享去重、CPU 有界多核恢复。
        /// 调用方持迁移锁；不使用运行期 TurnReadContext，不创建连接或 writer。
        pub(super) async fn $restore(
            connection: &mut $conn,
            nodes: Vec<StoredNode>,
        ) -> anyhow::Result<Vec<TurnNode>> {
            ensure!(
                nodes.len() <= UPGRADE_NODE_BATCH,
                "upgrade node batch too large"
            );
            let mut identities = HashSet::with_capacity(nodes.len());
            for node in &nodes {
                ensure!(
                    identities.insert(node.node_pk),
                    "duplicate history node identity"
                );
            }
            let planned =
                upgrade_cpu(nodes.into_iter().enumerate().collect(), plan_upgrade_node).await?;
            let mut scopes: HashMap<i64, Vec<UpgradeNode>> = HashMap::new();
            for node in planned {
                scopes.entry(node.2.scope_id).or_default().push(node);
            }
            let mut jobs = Vec::new();
            for (scope_id, nodes) in scopes {
                let mut needed: Vec<i64> = nodes
                    .iter()
                    .flat_map(|(_, _, plan)| plan.needed.iter().copied())
                    .collect();
                needed.sort_unstable();
                needed.dedup();
                let lengths = content_store::$backend::lengths(connection, &needed).await?;
                let mut ids = needed.into_iter().peekable();
                let mut loaded = HashMap::new();
                while ids.peek().is_some() {
                    let mut chunk = Vec::new();
                    let mut bytes = 0usize;
                    while let Some(&id) = ids.peek() {
                        let length = *lengths.get(&id).context("missing history content")?;
                        let length =
                            usize::try_from(length).context("invalid stored content length")?;
                        if !chunk.is_empty()
                            && (chunk.len() == 256
                                || bytes.saturating_add(length) > UPGRADE_CONTENT_BYTES)
                        {
                            break;
                        }
                        chunk.push(id);
                        ids.next();
                        bytes = bytes.saturating_add(length);
                    }
                    let encoded =
                        content_store::$backend::read_upgrade_encoded(connection, scope_id, &chunk)
                            .await?;
                    loaded.extend(upgrade_cpu(encoded, decode_upgrade_body).await?);
                }
                for (index, nodes, plan) in nodes {
                    let mut contents = HashMap::with_capacity(plan.needed.len());
                    for id in &plan.needed {
                        let value = loaded.get(id).context("missing history content")?;
                        let RestoredContent::Shared(value) = value else {
                            anyhow::bail!("unshared upgrade content");
                        };
                        contents.insert(*id, RestoredContent::Shared(value.clone()));
                    }
                    jobs.push((index, nodes, plan, contents));
                }
                // 只保留各消费节点的 Arc；最后使用者可直接移动正文。
                drop(loaded);
            }
            let mut restored = upgrade_cpu(jobs, apply_upgrade_node).await?;
            restored.sort_unstable_by_key(|(index, _)| *index);
            Ok(restored.into_iter().map(|(_, node)| node).collect())
        }
    };
}

restore_upgrade_backend!(restore_upgrade_sqlite, sqlite, sqlx::SqliteConnection<'_>);
restore_upgrade_backend!(restore_upgrade_postgres, postgres, sqlx::PgConnection<'_>);

#[cfg(test)]
mod upgrade_tests {
    use super::*;

    async fn database() -> anyhow::Result<sqlx::SqliteConnection<'static>> {
        let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
        sqlx::query(
            "CREATE TABLE contents (id INTEGER PRIMARY KEY, scope_id INTEGER NOT NULL, \
             digest BLOB NOT NULL, format INTEGER NOT NULL, raw_len INTEGER NOT NULL, body BLOB NOT NULL)",
        )
        .execute(&mut connection)
        .await?;
        Ok(connection)
    }

    async fn body(
        connection: &mut sqlx::SqliteConnection<'_>,
        id: i64,
        scope_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        let digest = content_store::content_digest(ContentFormat::Text, text.as_bytes());
        let encoded = crate::storage_codec::encode(text.as_bytes().into())?;
        sqlx::query(
            "INSERT INTO contents (id,scope_id,digest,format,raw_len,body) VALUES (?,?,?,0,?,?)",
        )
        .bind(id)
        .bind(scope_id)
        .bind(digest.to_vec())
        .bind(text.len() as i64)
        .bind(encoded)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    fn node(node_pk: i64, scope_id: i64, content_id: i64) -> anyhow::Result<StoredNode> {
        Ok(StoredNode {
            node_pk,
            scope_id,
            references: vec![[scope_id, 0, content_id]],
            id: TurnNodeId::new(format!("upgrade-{node_pk}")),
            kind: TurnNodeKind::Response,
            parent_id: None,
            payload_version: 1,
            payload: content_store::encode_descriptor(
                &serde_json::json!({"first": null, "second": null}),
                &[
                    Slot::text("/first".to_owned(), 0),
                    Slot::text("/second".to_owned(), 0),
                ],
            )?,
        })
    }

    #[tokio::test]
    async fn upgrade_preserves_order_scopes_shared_content_and_large_units() -> anyhow::Result<()> {
        let mut connection = database().await?;
        let large = "中".repeat(UPGRADE_CONTENT_BYTES / 3 + 1);
        body(&mut connection, 1, 10, "shared\n😀").await?;
        body(&mut connection, 2, 20, &large).await?;
        let restored = restore_upgrade_sqlite(
            &mut connection,
            vec![node(3, 10, 1)?, node(1, 20, 2)?, node(2, 10, 1)?],
        )
        .await?;
        for ((restored, id), text) in restored
            .iter()
            .zip(["upgrade-3", "upgrade-1", "upgrade-2"])
            .zip(["shared\n😀", large.as_str(), "shared\n😀"])
        {
            assert_eq!(restored.id.as_str(), id);
            assert_eq!(
                restored.payload,
                serde_json::json!({"first": text, "second": text})
            );
        }
        assert!(
            restore_upgrade_sqlite(&mut connection, Vec::new())
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn upgrade_rejects_corrupt_references_and_bodies() -> anyhow::Result<()> {
        let mut connection = database().await?;
        body(&mut connection, 1, 10, "verified").await?;
        let mut wrong_scope = node(1, 10, 1)?;
        wrong_scope.references[0][0] = 20;
        assert!(
            restore_upgrade_sqlite(&mut connection, vec![wrong_scope])
                .await
                .is_err()
        );
        let mut wrong_ordinal = node(1, 10, 1)?;
        wrong_ordinal.references[0][1] = 1;
        assert!(
            restore_upgrade_sqlite(&mut connection, vec![wrong_ordinal])
                .await
                .is_err()
        );
        // A self-consistent reference row still cannot read another scope's body.
        assert!(
            restore_upgrade_sqlite(&mut connection, vec![node(1, 20, 1)?])
                .await
                .is_err()
        );
        assert!(
            restore_upgrade_sqlite(&mut connection, vec![node(1, 10, 1)?, node(1, 10, 1)?],)
                .await
                .is_err()
        );
        for corruption in [
            "UPDATE contents SET format = 99 WHERE id = 1",
            "UPDATE contents SET raw_len = raw_len + 1 WHERE id = 1",
            "UPDATE contents SET body = x'00' WHERE id = 1",
        ] {
            sqlx::query(corruption).execute(&mut connection).await?;
            assert!(
                restore_upgrade_sqlite(&mut connection, vec![node(1, 10, 1)?])
                    .await
                    .is_err(),
                "{corruption}"
            );
            sqlx::query("DELETE FROM contents")
                .execute(&mut connection)
                .await?;
            body(&mut connection, 1, 10, "verified").await?;
        }
        sqlx::query("UPDATE contents SET digest = zeroblob(32) WHERE id = 1")
            .execute(&mut connection)
            .await?;
        let error = restore_upgrade_sqlite(&mut connection, vec![node(1, 10, 1)?])
            .await
            .err()
            .context("damaged body accepted")?;
        assert!(error.to_string().contains("damaged content"), "{error:#}");
        Ok(())
    }
}
