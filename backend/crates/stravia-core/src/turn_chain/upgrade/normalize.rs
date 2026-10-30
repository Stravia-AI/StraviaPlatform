//! Resumable format-2 -> normalized v3 conversion.
//!
//! Every batch commits data and the `turn_history_upgrade` cursor/stage in one
//! transaction. The old `turn_chain_*` rows are read-only here and remain the
//! authority until migration 0033. Opaque item/system/tool contents keep their
//! already-verified numeric ids and codec bytes; only inline semantic units and
//! externalized profile skeletons are decoded and re-extracted.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail, ensure};
use serde_json::{Map, Value};

use crate::turn_chain::content;
use crate::turn_chain::upgrade::format2;

const STAGE_DONE: i64 = 4;
const DEFAULT_NODE_BATCH: usize = 64;
const DEFAULT_CONTENT_BATCH: usize = 512;
const DEFAULT_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const VALIDATION_BATCH: usize = 256;

/// Batch and interruption controls. Product entry points use [`Limits::default`];
/// tests use smaller batches and a batch budget to exercise every resume point.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub(crate) node_batch: usize,
    pub(crate) content_batch: usize,
    /// Logical source-byte work budget for a descriptor prefix and its unique
    /// profiles, using `max(encoded_len, decoded_hint)`. This is not an RSS
    /// limit; decode still validates the actual frame.
    pub(crate) content_bytes: usize,
    /// Test-only cap on committed batches per call. `None` runs to completion.
    pub(crate) budget: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            node_batch: DEFAULT_NODE_BATCH,
            content_batch: DEFAULT_CONTENT_BATCH,
            content_bytes: DEFAULT_CONTENT_BYTES,
            budget: None,
        }
    }
}

struct Budget {
    remaining: Option<usize>,
}

impl Budget {
    fn new(limit: Option<usize>) -> Self {
        Self { remaining: limit }
    }

    /// Returns false when a test budget wants the caller to stop before the
    /// next batch. Batch writes already committed stay resumable.
    fn take(&mut self) -> bool {
        match self.remaining.as_mut() {
            None => true,
            Some(remaining) if *remaining == 0 => false,
            Some(remaining) => {
                *remaining -= 1;
                true
            }
        }
    }
}

struct Work {
    stage: i64,
    last_node_id: Option<String>,
    last_content_id: Option<i64>,
}

/// Thin legacy metadata copied by stage 0.
#[derive(sqlx::FromRow)]
struct LegacyMeta {
    id: String,
    principal: String,
    kind: String,
    parent_id: Option<String>,
    payload_version: i64,
    created_at: i64,
    expires_at: i64,
    prefix_namespace: Option<String>,
    prefix_fingerprint: Option<String>,
    prefix_item_count: Option<i64>,
    prefix_completed_at: Option<i64>,
    prefix_projection_version: Option<i64>,
    prefix_controls_fingerprint: Option<String>,
    prefix_context_fingerprint: Option<String>,
    storage_format: i64,
}

/// Stage 2 row: thin metadata plus the legacy envelope.
#[derive(sqlx::FromRow)]
struct LegacyNode {
    #[sqlx(flatten)]
    meta: LegacyMeta,
    payload: Vec<u8>,
}

/// Legacy `turn_chain_nodes` metadata shared by stage 0 and stage 2.
///
/// `storage_format` (baseline) and `prefix_projection_version` (0017) are
/// PostgreSQL INT4 while the Rust fields are i64; both are projected
/// explicitly because sqlx rejects a non-NULL INT4 for an i64 target.
macro_rules! legacy_node_columns {
    () => {
        "id, principal, kind, parent_id, payload_version, created_at, expires_at, \
         prefix_namespace, prefix_fingerprint, prefix_item_count, prefix_completed_at, \
         CAST(prefix_projection_version AS BIGINT) AS prefix_projection_version, \
         prefix_controls_fingerprint, prefix_context_fingerprint, \
         CAST(storage_format AS BIGINT) AS storage_format"
    };
}

const LEGACY_META_SELECT: &str =
    concat!("SELECT ", legacy_node_columns!(), " FROM turn_chain_nodes");
const LEGACY_NODE_SELECT: &str = concat!(
    "SELECT ",
    legacy_node_columns!(),
    ", payload FROM turn_chain_nodes"
);

#[derive(sqlx::FromRow)]
struct LegacyContentMeta {
    id: i64,
    content_bytes: i64,
}

/// Stage 2 thin row: the payload length and codec trailer are enough to pick a
/// logical-byte-bounded node prefix without decoding any body.
#[derive(sqlx::FromRow)]
struct ThinNode {
    id: String,
    encoded_len: i64,
    trailer: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct LegacyProfileMeta {
    principal: String,
    content_key: String,
    content_bytes: i64,
    trailer: Vec<u8>,
}

/// Logical source-byte cost of one unique profile body. This is a worklist
/// budget hint, not an RSS limit; `decode` still validates the actual frame.
#[derive(Clone, Copy)]
struct ProfileCost {
    encoded: usize,
    decoded: usize,
}

impl ProfileCost {
    fn budget(self) -> usize {
        self.encoded.max(self.decoded)
    }
}

#[derive(Default)]
struct ProfileCosts {
    by_principal: HashMap<String, HashMap<String, ProfileCost>>,
}

impl ProfileCosts {
    fn get(&self, principal: &str, key: &str) -> Option<ProfileCost> {
        self.by_principal
            .get(principal)
            .and_then(|costs| costs.get(key))
            .copied()
    }
}

#[derive(sqlx::FromRow)]
struct LegacyContent {
    id: i64,
    principal: String,
    content_key: String,
    content: Vec<u8>,
}

struct PreparedContent {
    id: i64,
    principal: String,
    digest: content::ContentDigest,
    content: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct ExistingNode {
    id: String,
    node_pk: i64,
    scope_id: i64,
    principal: String,
    kind: String,
}

/// One decoded legacy envelope. Slot indices point into `keys`; `ancestors`
/// records the nearest containing slot so nested pointers are validated against
/// the profile body rather than the null root.
struct Parsed {
    data: Value,
    slots: Vec<(String, usize)>,
    keys: Vec<String>,
    ancestors: Vec<Option<usize>>,
}

struct Ready {
    meta: LegacyMeta,
    parsed: Parsed,
}

/// Batch-scoped reads for the current node worklist. Only content ids and
/// profile bodies referenced by these nodes are held.
struct LegacyBatch {
    ids: HashMap<String, HashMap<String, i64>>,
    profiles: ProfileStore,
}

impl LegacyBatch {
    fn content_id(&self, principal: &str, key: &str) -> Option<i64> {
        self.ids
            .get(principal)
            .and_then(|ids| ids.get(key))
            .copied()
    }
}

/// Distinct profile bodies for the worklist, keyed by principal and legacy
/// content key. Shared bodies keep one decoded/extracted copy.
#[derive(Default)]
struct ProfileStore {
    entries: Vec<PreparedProfile>,
    index: HashMap<String, HashMap<String, usize>>,
}

impl ProfileStore {
    fn get(&self, principal: &str, key: &str) -> Option<usize> {
        self.index
            .get(principal)
            .and_then(|index| index.get(key))
            .copied()
    }
}

/// One legacy profile body re-extracted exactly once per `(principal, key)`
/// for the whole node worklist. Nodes sharing the body also share its content
/// bodies and inner slot layout; only their absolute pointers differ.
struct PreparedProfile {
    contents: Vec<content::Content>,
    body: usize,
    inner: Vec<(usize, String)>,
}

/// A profile row queued for off-thread decode, nested-pointer validation and
/// re-extraction. `relatives` are every nested slot path recorded under this
/// body anywhere in the current worklist.
struct RawProfile {
    principal: String,
    key: String,
    encoded: Vec<u8>,
    relatives: Vec<String>,
}

enum SlotRef {
    Preserved(i64),
    Fresh(usize),
    Profile { profile: usize, content: usize },
}

struct Plan {
    meta: LegacyMeta,
    data: Value,
    slots: Vec<(String, SlotRef)>,
    contents: Vec<content::Content>,
}

/// Stage 3 row: new immutable metadata, payload/retention and the old facts.
#[derive(sqlx::FromRow)]
struct ValidationRow {
    node_pk: i64,
    id: String,
    kind: String,
    parent_pk: Option<i64>,
    payload_version: i64,
    created_at: i64,
    prefix_namespace: Option<String>,
    prefix_fingerprint: Option<String>,
    prefix_item_count: Option<i64>,
    prefix_completed_at: Option<i64>,
    prefix_projection_version: Option<i64>,
    prefix_controls_fingerprint: Option<String>,
    prefix_context_fingerprint: Option<String>,
    payload: Vec<u8>,
    expires_at: i64,
    old_principal: String,
    old_kind: String,
    old_parent_id: Option<String>,
    old_payload_version: i64,
    old_created_at: i64,
    old_expires_at: i64,
    old_prefix_namespace: Option<String>,
    old_prefix_fingerprint: Option<String>,
    old_prefix_item_count: Option<i64>,
    old_prefix_completed_at: Option<i64>,
    old_prefix_projection_version: Option<i64>,
    old_prefix_controls_fingerprint: Option<String>,
    old_prefix_context_fingerprint: Option<String>,
    scope_principal: String,
    parent_identity: Option<String>,
    parent_kind: Option<String>,
    parent_principal: Option<String>,
    old_parent_kind: Option<String>,
}

fn ensure_kind(kind: &str) -> anyhow::Result<()> {
    match kind {
        "response" | "agent" | "web_search" => Ok(()),
        _ => bail!("invalid history node kind"),
    }
}

fn thin_node_cost(row: ThinNode) -> anyhow::Result<(String, usize)> {
    let encoded =
        usize::try_from(row.encoded_len).context("invalid legacy history payload encoding")?;
    let decoded = crate::storage_codec::decoded_size_hint(&row.trailer)
        .context("invalid legacy history payload encoding")?;
    ensure!(
        encoded >= row.trailer.len(),
        "invalid legacy history payload encoding"
    );
    Ok((row.id, encoded.max(decoded)))
}

fn profile_keys(meta: &LegacyMeta, parsed: &Parsed) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for (pointer, key_index) in &parsed.slots {
        if !format2::is_profile_pointer(pointer) {
            continue;
        }
        let key = &parsed.keys[*key_index];
        if seen.insert(key.as_str()) {
            keys.push((meta.principal.clone(), key.clone()));
        }
    }
    keys
}

fn parse_node(row: LegacyNode) -> anyhow::Result<(LegacyMeta, Parsed)> {
    let LegacyNode { meta, payload } = row;
    ensure!(meta.storage_format == 2, "unconverted legacy history node");
    ensure_kind(&meta.kind)?;
    ensure!(meta.payload_version > 0, "invalid history payload version");
    Ok((meta, parse_envelope(&payload)?))
}

fn parse_envelope(raw_payload: &[u8]) -> anyhow::Result<Parsed> {
    let raw = crate::storage_codec::decode(raw_payload)
        .context("invalid legacy history payload encoding")?;
    let mut value: Value =
        serde_json::from_slice(&raw).context("invalid legacy history payload")?;
    let object = value
        .as_object_mut()
        .context("invalid legacy history payload")?;
    let data = object
        .remove("data")
        .context("missing legacy history data")?;
    let slot_values = object
        .remove("slots")
        .context("missing legacy history slots")?;
    let key_values = object
        .remove("contents")
        .context("missing legacy history contents")?;
    ensure!(object.is_empty(), "invalid legacy history payload");
    let slot_values = slot_values
        .as_array()
        .context("missing legacy history slots")?;
    let key_values = key_values
        .as_array()
        .context("missing legacy history contents")?;
    let mut keys = Vec::with_capacity(key_values.len());
    let mut unique_keys = HashSet::with_capacity(key_values.len());
    for key in key_values {
        let key = key.as_str().context("invalid legacy history content key")?;
        ensure!(
            unique_keys.insert(key),
            "duplicate legacy history content key"
        );
        keys.push(key.to_owned());
    }
    let mut slots = Vec::with_capacity(slot_values.len());
    let mut pointers = HashSet::with_capacity(slot_values.len());
    for slot in slot_values {
        let pair = slot.as_array().context("invalid legacy history slot")?;
        ensure!(pair.len() == 2, "invalid legacy history slot");
        let pointer = pair[0]
            .as_str()
            .context("invalid legacy history slot pointer")?;
        format2::validate_pointer(pointer)?;
        ensure!(
            pointers.insert(pointer),
            "duplicate legacy history slot pointer"
        );
        let index = usize::try_from(
            pair[1]
                .as_u64()
                .context("invalid legacy history slot index")?,
        )
        .context("invalid legacy history slot index")?;
        ensure!(index < keys.len(), "legacy history slot index out of range");
        slots.push((pointer.to_owned(), index));
    }
    let ancestors = pointer_ancestors(&slots)?;
    Ok(Parsed {
        data,
        slots,
        keys,
        ancestors,
    })
}

fn pointer_ancestors(slots: &[(String, usize)]) -> anyhow::Result<Vec<Option<usize>>> {
    let positions: HashMap<&str, usize> = slots
        .iter()
        .enumerate()
        .map(|(index, (pointer, _))| (pointer.as_str(), index))
        .collect();
    let mut ancestors = Vec::with_capacity(slots.len());
    for (index, (pointer, _)) in slots.iter().enumerate() {
        let mut ancestor = None;
        for (offset, _) in pointer
            .char_indices()
            .filter(|(_, character)| *character == '/')
        {
            if offset == 0 {
                continue;
            }
            if let Some(parent) = positions.get(&pointer[..offset]) {
                ensure!(
                    *parent < index,
                    "legacy history parent slot must precede nested slot"
                );
                ancestor = Some(*parent);
            }
        }
        ancestors.push(ancestor);
    }
    Ok(ancestors)
}

fn validate_old_slots(parsed: &Parsed) -> anyhow::Result<()> {
    for (index, (pointer, _)) in parsed.slots.iter().enumerate() {
        match parsed.ancestors[index] {
            None => {
                let slot = parsed
                    .data
                    .pointer(pointer)
                    .context("invalid legacy history slot pointer")?;
                ensure!(slot.is_null(), "legacy history slot overwrites data");
            }
            Some(ancestor) => {
                ensure!(
                    format2::is_profile_pointer(&parsed.slots[ancestor].0),
                    "legacy history slot nested under opaque content"
                );
            }
        }
    }
    Ok(())
}

/// Decodes one legacy profile body once per `(principal, key)` worklist entry,
/// validates every nested legacy slot recorded under it, then re-extracts its
/// inline units and whole-body content with the same boundaries as the product
/// module. The wrapper name only supplies the stripped pointer prefix; the
/// decoded body and inner unit layout are shared by every referencing node.
fn prepare_profile(raw: RawProfile) -> anyhow::Result<(String, String, PreparedProfile)> {
    let RawProfile {
        principal,
        key,
        encoded,
        relatives,
    } = raw;
    let raw_body = crate::storage_codec::decode(&encoded)
        .context("invalid legacy history profile encoding")?;
    let body: Value =
        serde_json::from_slice(&raw_body).context("invalid legacy history profile content")?;
    ensure!(!body.is_null(), "invalid legacy history profile content");
    for relative in &relatives {
        let value = body
            .pointer(relative)
            .context("invalid legacy history slot pointer")?;
        ensure!(value.is_null(), "legacy history slot overwrites data");
    }
    let profile = extract_profile(body)?;
    Ok((principal, key, profile))
}

fn extract_profile(body: Value) -> anyhow::Result<PreparedProfile> {
    let name = format2::PROFILE_KEYS[0];
    let wrapper = Value::Object(Map::from_iter([(name.to_owned(), body)]));
    let extracted = content::extract(wrapper).context("extracting legacy history profile")?;
    let content::Extracted {
        slots: profile_slots,
        contents,
        ..
    } = extracted;
    let body_pointer = format2::pointer_token(name);
    let body_position = profile_slots
        .iter()
        .position(|(pointer, _)| *pointer == body_pointer)
        .context("legacy history profile boundary lost")?;
    let body = profile_slots[body_position].1;
    let prefix = format!("{body_pointer}/");
    let mut inner = Vec::with_capacity(profile_slots.len().saturating_sub(1));
    for (position, (slot_pointer, content_index)) in profile_slots.into_iter().enumerate() {
        if position == body_position {
            continue;
        }
        let relative = slot_pointer
            .strip_prefix(prefix.as_str())
            .context("invalid legacy history profile slot")?;
        inner.push((content_index, relative.to_owned()));
    }
    Ok(PreparedProfile {
        contents,
        body,
        inner,
    })
}

/// Converts one decoded descriptor without restoring opaque contents. Preserved
/// slots keep their numeric ids; inline units and profile skeletons are
/// extracted into fresh contents whose ordinals are assigned at write time.
fn convert_node(ready: Ready, batch: &LegacyBatch) -> anyhow::Result<Plan> {
    let Ready { meta, parsed } = ready;
    validate_old_slots(&parsed)?;
    let Parsed {
        data, slots, keys, ..
    } = parsed;
    let extracted = content::extract(data).context("extracting normalized history")?;
    let content::Extracted {
        data,
        slots: fresh_slots,
        contents,
    } = extracted;
    let mut output: Vec<(String, SlotRef)> = fresh_slots
        .into_iter()
        .map(|(pointer, index)| (pointer, SlotRef::Fresh(index)))
        .collect();
    // Legacy profile bodies are stored with their externalized children
    // already nulled. v3 extraction of such a body only re-creates the inline
    // children, so these envelope slots (including nested profile bodies and
    // preserved units) are disjoint and must still be emitted here.
    for (pointer, key_index) in &slots {
        let key = &keys[*key_index];
        if format2::is_profile_pointer(pointer) {
            let profile = batch
                .profiles
                .get(&meta.principal, key)
                .context("missing legacy history profile content")?;
            let prepared = &batch.profiles.entries[profile];
            output.push((
                pointer.clone(),
                SlotRef::Profile {
                    profile,
                    content: prepared.body,
                },
            ));
            for (content, relative) in &prepared.inner {
                output.push((
                    format!("{pointer}/{relative}"),
                    SlotRef::Profile {
                        profile,
                        content: *content,
                    },
                ));
            }
        } else {
            let id = batch
                .content_id(&meta.principal, key)
                .context("missing legacy history content id")?;
            output.push((pointer.clone(), SlotRef::Preserved(id)));
        }
    }
    Ok(Plan {
        meta,
        data,
        slots: output,
        contents,
    })
}
fn has_ancestor_pointer(pointer: &str, pointers: &HashSet<&str>) -> bool {
    pointer
        .char_indices()
        .filter(|(_, character)| *character == '/')
        .any(|(offset, _)| offset > 0 && pointers.contains(&pointer[..offset]))
}

fn validate_descriptor(payload: &[u8], links: &[(i64, i64)]) -> anyhow::Result<()> {
    for (expected, (ordinal, _)) in links.iter().enumerate() {
        ensure!(
            *ordinal == i64::try_from(expected)?,
            "non-contiguous history content ordinal"
        );
    }
    let raw = crate::storage_codec::decode(payload)
        .context("invalid normalized history payload encoding")?;
    let value: Value =
        serde_json::from_slice(&raw).context("invalid normalized history descriptor")?;
    let object = value
        .as_object()
        .context("invalid normalized history descriptor")?;
    ensure!(
        object.len() == 2 && object.contains_key("data") && object.contains_key("slots"),
        "invalid normalized history descriptor"
    );
    let data = &object["data"];
    let slots = object["slots"]
        .as_array()
        .context("invalid normalized history slots")?;
    let mut seen = vec![false; links.len()];
    let mut pointers = HashSet::with_capacity(slots.len());
    let mut positions: HashMap<&str, usize> = HashMap::with_capacity(slots.len());
    let mut ordered: Vec<&str> = Vec::with_capacity(slots.len());
    for (slot_index, slot) in slots.iter().enumerate() {
        let pair = slot.as_array().context("invalid normalized history slot")?;
        ensure!(pair.len() == 2, "invalid normalized history slot");
        let pointer = pair[0]
            .as_str()
            .context("invalid normalized history slot pointer")?;
        format2::validate_pointer(pointer)?;
        ensure!(
            pointers.insert(pointer),
            "duplicate normalized history slot pointer"
        );
        positions.insert(pointer, slot_index);
        ordered.push(pointer);
        let ordinal = usize::try_from(
            pair[1]
                .as_u64()
                .context("invalid normalized history slot ordinal")?,
        )
        .context("invalid normalized history slot ordinal")?;
        ensure!(
            ordinal < links.len(),
            "normalized history slot ordinal out of range"
        );
        seen[ordinal] = true;
    }
    ensure!(
        seen.iter().all(|referenced| *referenced),
        "unreferenced normalized history content ordinal"
    );
    for (slot_index, pointer) in ordered.iter().copied().enumerate() {
        for (offset, _) in pointer
            .char_indices()
            .filter(|(_, character)| *character == '/')
        {
            if offset == 0 {
                continue;
            }
            if let Some(parent) = positions.get(&pointer[..offset]) {
                ensure!(
                    *parent < slot_index,
                    "normalized history parent slot must precede nested slot"
                );
            }
        }
    }
    for pointer in &pointers {
        if !has_ancestor_pointer(pointer, &pointers) {
            ensure!(
                data.pointer(pointer).is_some_and(Value::is_null),
                "normalized history slot does not address null"
            );
        }
    }
    Ok(())
}

fn validate_row(row: &ValidationRow, links: &[(i64, i64)]) -> anyhow::Result<()> {
    ensure!(
        row.scope_principal == row.old_principal,
        "normalized history scope ownership mismatch"
    );
    ensure!(
        row.kind == row.old_kind,
        "normalized history node kind mismatch"
    );
    ensure!(
        row.payload_version == row.old_payload_version,
        "normalized history payload version mismatch"
    );
    ensure!(
        row.created_at == row.old_created_at,
        "normalized history creation time mismatch"
    );
    ensure!(
        row.expires_at == row.old_expires_at,
        "normalized history retention mismatch"
    );
    ensure!(
        row.prefix_namespace == row.old_prefix_namespace
            && row.prefix_fingerprint == row.old_prefix_fingerprint
            && row.prefix_item_count == row.old_prefix_item_count
            && row.prefix_completed_at == row.old_prefix_completed_at
            && row.prefix_projection_version == row.old_prefix_projection_version
            && row.prefix_controls_fingerprint == row.old_prefix_controls_fingerprint
            && row.prefix_context_fingerprint == row.old_prefix_context_fingerprint,
        "normalized history prefix metadata mismatch"
    );
    match (
        row.parent_pk,
        row.old_parent_id.as_deref(),
        row.parent_identity.as_deref(),
    ) {
        (None, None, None) => {}
        (Some(_), Some(old_parent), Some(parent_identity)) => {
            ensure!(
                old_parent == parent_identity,
                "normalized history parent identity mismatch"
            );
            ensure!(
                row.parent_kind.as_deref() == row.old_parent_kind.as_deref(),
                "normalized history parent kind mismatch"
            );
            ensure!(
                row.parent_principal.as_deref() == Some(row.scope_principal.as_str()),
                "normalized history parent ownership mismatch"
            );
            ensure!(
                parent_identity != row.id.as_str(),
                "history node is its own parent"
            );
        }
        _ => bail!("normalized history parent edge mismatch"),
    }
    validate_descriptor(&row.payload, links)
}

macro_rules! backend {
    (
        $module:ident,
        $db:ty,
        $conn:ty,
        $lock_nodes:literal,
        $lock_contents:literal,
        $intern:ident,
        $link:ident,
        $reset:literal
    ) => {
        mod $module {
            use std::collections::{HashMap, HashSet};
            use std::sync::Arc;

            use anyhow::{bail, ensure, Context};

            use super::*;

            const VALIDATION_SELECT: &str = "SELECT n.node_pk AS node_pk, n.id AS id, \
                n.scope_id AS scope_id, n.kind AS kind, n.parent_pk AS parent_pk, \
                n.payload_version AS payload_version, n.created_at AS created_at, \
                n.prefix_namespace AS prefix_namespace, \
                n.prefix_fingerprint AS prefix_fingerprint, \
                n.prefix_item_count AS prefix_item_count, \
                n.prefix_completed_at AS prefix_completed_at, \
                n.prefix_projection_version AS prefix_projection_version, \
                n.prefix_controls_fingerprint AS prefix_controls_fingerprint, \
                n.prefix_context_fingerprint AS prefix_context_fingerprint, \
                p.payload AS payload, r.expires_at AS expires_at, \
                o.principal AS old_principal, o.kind AS old_kind, \
                o.parent_id AS old_parent_id, o.payload_version AS old_payload_version, \
                o.created_at AS old_created_at, o.expires_at AS old_expires_at, \
                o.prefix_namespace AS old_prefix_namespace, \
                o.prefix_fingerprint AS old_prefix_fingerprint, \
                o.prefix_item_count AS old_prefix_item_count, \
                o.prefix_completed_at AS old_prefix_completed_at, \
                CAST(o.prefix_projection_version AS BIGINT) AS old_prefix_projection_version, \
                o.prefix_controls_fingerprint AS old_prefix_controls_fingerprint, \
                o.prefix_context_fingerprint AS old_prefix_context_fingerprint, \
                s.principal AS scope_principal, pn.id AS parent_identity, \
                pn.kind AS parent_kind, ps.principal AS parent_principal, \
                po.kind AS old_parent_kind \
                FROM turn_nodes n \
                JOIN turn_node_payloads p ON p.node_pk = n.node_pk \
                JOIN turn_node_retention r ON r.node_pk = n.node_pk \
                JOIN turn_scopes s ON s.id = n.scope_id \
                JOIN turn_chain_nodes o ON o.id = n.id \
                LEFT JOIN turn_nodes pn ON pn.node_pk = n.parent_pk \
                LEFT JOIN turn_scopes ps ON ps.id = pn.scope_id \
                LEFT JOIN turn_chain_nodes po ON po.id = o.parent_id AND po.principal = o.principal";

            pub(crate) async fn run(
                connection: &mut $conn,
                limits: Limits,
            ) -> anyhow::Result<()> {
                ensure!(
                    limits.node_batch > 0
                        && limits.content_batch > 0
                        && limits.content_bytes > 0,
                    "invalid history normalization batch limits"
                );
                let mut budget = Budget::new(limits.budget);
                loop {
                    let work = load_work(connection).await?;
                    match work.stage {
                        0 => {
                            if !stage_metadata(connection, &work, limits, &mut budget).await? {
                                return Ok(());
                            }
                        }
                        1 => {
                            if !stage_contents(connection, &work, limits, &mut budget, $reset)
                                .await?
                            {
                                return Ok(());
                            }
                        }
                        2 => {
                            if !stage_descriptors(connection, &work, limits, &mut budget).await? {
                                return Ok(());
                            }
                        }
                        3 => {
                            if !stage_finalize(connection, &work, &mut budget).await? {
                                return Ok(());
                            }
                        }
                        4 => return Ok(()),
                        other => bail!("invalid history upgrade stage {other}"),
                    }
                }
            }

            async fn load_work(connection: &mut $conn) -> anyhow::Result<Work> {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(stage AS BIGINT), last_node_id, last_content_id \
                     FROM turn_history_upgrade WHERE phase = 1",
                );
                let row: Option<(i64, Option<String>, Option<i64>)> = query
                    .build_query_as()
                    .fetch_optional(&mut *connection)
                    .await?;
                let (stage, last_node_id, last_content_id) =
                    row.context("missing history upgrade work state")?;
                ensure!(
                    (0..=STAGE_DONE).contains(&stage),
                    "invalid history upgrade stage"
                );
                Ok(Work {
                    stage,
                    last_node_id,
                    last_content_id,
                })
            }

            async fn mark_work(
                connection: &mut $conn,
                stage: i64,
                last_node_id: Option<&str>,
                last_content_id: Option<i64>,
            ) -> anyhow::Result<()> {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "UPDATE turn_history_upgrade SET stage = ",
                );
                query
                    .push_bind(stage)
                    .push(", last_node_id = ")
                    .push_bind(last_node_id)
                    .push(", last_content_id = ")
                    .push_bind(last_content_id)
                    .push(" WHERE phase = 1");
                let affected = query
                    .build()
                    .execute(&mut *connection)
                    .await?
                    .rows_affected();
                ensure!(affected == 1, "missing history upgrade work state");
                Ok(())
            }

            async fn ensure_scopes(
                connection: &mut $conn,
                principals: &[&str],
                create: bool,
            ) -> anyhow::Result<HashMap<String, i64>> {
                let mut scopes = HashMap::with_capacity(principals.len());
                if principals.is_empty() {
                    return Ok(scopes);
                }
                if create {
                    let mut query =
                        sqlx::QueryBuilder::<$db>::new("INSERT INTO turn_scopes (principal) ");
                    query.push_values(principals.iter(), |mut values, principal| {
                        values.push_bind(*principal);
                    });
                    query.push(" ON CONFLICT (principal) DO NOTHING");
                    query.build().execute(&mut *connection).await?;
                }
                for chunk in principals.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT principal, id FROM turn_scopes WHERE principal IN (",
                    );
                    let mut list = query.separated(",");
                    for principal in chunk {
                        list.push_bind(*principal);
                    }
                    list.push_unseparated(")");
                    let rows: Vec<(String, i64)> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    for (principal, id) in rows {
                        scopes.insert(principal, id);
                    }
                }
                ensure!(
                    scopes.len() == principals.len(),
                    "missing history scope"
                );
                Ok(scopes)
            }

            async fn stage_metadata(
                connection: &mut $conn,
                work: &Work,
                limits: Limits,
                budget: &mut Budget,
            ) -> anyhow::Result<bool> {
                let total: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_chain_nodes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let total = u64::try_from(total)?;
                let mut completed = 0u64;
                let mut cursor = work.last_node_id.clone();
                crate::startup_progress::report(
                    "history_data",
                    "Normalizing history identities",
                    0,
                    Some(total),
                );
                loop {
                    if !budget.take() {
                        return Ok(false);
                    }
                    let mut query = sqlx::QueryBuilder::<$db>::new(LEGACY_META_SELECT);
                    if let Some(cursor) = &cursor {
                        query.push(" WHERE id > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY id LIMIT ")
                        .push_bind(i64::try_from(limits.node_batch)?);
                    let rows: Vec<LegacyMeta> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if rows.is_empty() {
                        let mut transaction = connection.begin().await?;
                        mark_work(&mut transaction, 1, None, None).await?;
                        transaction.commit().await?;
                        return Ok(true);
                    }
                    for row in &rows {
                        ensure!(
                            row.storage_format == 2,
                            "unconverted legacy history node"
                        );
                        ensure_kind(&row.kind)?;
                        ensure!(
                            row.payload_version > 0,
                            "invalid history payload version"
                        );
                    }
                    let done = rows.len() < limits.node_batch;
                    let last = rows
                        .last()
                        .map(|row| row.id.clone())
                        .expect("history metadata batch");
                    let mut principals = Vec::new();
                    let mut seen = HashSet::new();
                    for row in &rows {
                        if seen.insert(row.principal.as_str()) {
                            principals.push(row.principal.as_str());
                        }
                    }
                    let mut transaction = connection.begin().await?;
                    let scopes = ensure_scopes(&mut transaction, &principals, true).await?;
                    let mut insert = sqlx::QueryBuilder::<$db>::new(
                        "INSERT INTO turn_nodes (id, scope_id, kind, payload_version, \
                         created_at, prefix_namespace, prefix_fingerprint, prefix_item_count, \
                         prefix_completed_at, prefix_projection_version, \
                         prefix_controls_fingerprint, prefix_context_fingerprint) ",
                    );
                    insert.push_values(rows.iter(), |mut values, row| {
                        let scope_id = scopes
                            .get(row.principal.as_str())
                            .copied()
                            .expect("history batch scope");
                        values
                            .push_bind(row.id.as_str())
                            .push_bind(scope_id)
                            .push_bind(row.kind.as_str())
                            .push_bind(row.payload_version)
                            .push_bind(row.created_at)
                            .push_bind(row.prefix_namespace.as_deref())
                            .push_bind(row.prefix_fingerprint.as_deref())
                            .push_bind(row.prefix_item_count)
                            .push_bind(row.prefix_completed_at)
                            .push_bind(row.prefix_projection_version)
                            .push_bind(row.prefix_controls_fingerprint.as_deref())
                            .push_bind(row.prefix_context_fingerprint.as_deref());
                    });
                    insert.build().execute(&mut *transaction).await?;
                    if done {
                        mark_work(&mut transaction, 1, None, None).await?;
                    } else {
                        mark_work(&mut transaction, 0, Some(last.as_str()), None).await?;
                    }
                    transaction.commit().await?;
                    completed += rows.len() as u64;
                    crate::startup_progress::report(
                        "history_data",
                        "Normalizing history identities",
                        completed,
                        Some(total),
                    );
                    if done {
                        return Ok(true);
                    }
                    cursor = Some(last);
                }
            }

            async fn stage_contents(
                connection: &mut $conn,
                work: &Work,
                limits: Limits,
                budget: &mut Budget,
                identity_reset: &'static str,
            ) -> anyhow::Result<bool> {
                let total: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_chain_contents",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let total = u64::try_from(total)?;
                let mut completed = 0u64;
                let mut cursor = work.last_content_id;
                crate::startup_progress::report(
                    "history_data",
                    "Normalizing history content",
                    0,
                    Some(total),
                );
                loop {
                    if !budget.take() {
                        return Ok(false);
                    }
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, CAST(length(content) AS BIGINT) AS content_bytes \
                         FROM turn_chain_contents",
                    );
                    if let Some(cursor) = cursor {
                        query.push(" WHERE id > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY id LIMIT ")
                        .push_bind(i64::try_from(limits.content_batch)?);
                    let metas: Vec<LegacyContentMeta> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if metas.is_empty() {
                        let mut transaction = connection.begin().await?;
                        if !identity_reset.is_empty() {
                            sqlx::query(identity_reset)
                                .execute(&mut *transaction)
                                .await?;
                        }
                        mark_work(&mut transaction, 2, None, None).await?;
                        transaction.commit().await?;
                        return Ok(true);
                    }
                    let batch_exhausted = metas.len() < limits.content_batch;
                    let mut chosen = 0usize;
                    let mut bytes = 0usize;
                    for meta in &metas {
                        let size = usize::try_from(meta.content_bytes)
                            .context("invalid legacy history content size")?;
                        bytes = bytes
                            .checked_add(size)
                            .context("legacy history content batch size overflow")?;
                        chosen += 1;
                        if bytes >= limits.content_bytes {
                            break;
                        }
                    }
                    let done = batch_exhausted && chosen == metas.len();
                    let last_id = metas[chosen - 1].id;
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, principal, content_key, content FROM turn_chain_contents \
                         WHERE id > ",
                    );
                    query
                        .push_bind(cursor.unwrap_or(i64::MIN))
                        .push(" AND id <= ")
                        .push_bind(last_id)
                        .push(" ORDER BY id");
                    let rows: Vec<LegacyContent> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    ensure!(
                        rows.len() == chosen,
                        "legacy history content batch changed"
                    );
                    let prepared = super::super::blocking_chunks(rows, |row| {
                        let raw = crate::storage_codec::decode(&row.content)
                            .context("invalid legacy history content encoding")?;
                        ensure!(
                            format2::content_key(&raw) == row.content_key,
                            "damaged legacy history content"
                        );
                        Ok(PreparedContent {
                            id: row.id,
                            principal: row.principal,
                            digest: content::content_key(&raw),
                            content: row.content,
                        })
                    })
                    .await?;
                    let mut principals = Vec::new();
                    let mut seen = HashSet::new();
                    for content in &prepared {
                        if seen.insert(content.principal.as_str()) {
                            principals.push(content.principal.as_str());
                        }
                    }
                    let mut transaction = connection.begin().await?;
                    let scopes = ensure_scopes(&mut transaction, &principals, true).await?;
                    for chunk in prepared.chunks(200) {
                        let mut insert = sqlx::QueryBuilder::<$db>::new(
                            "INSERT INTO turn_contents (id, scope_id, digest, content) ",
                        );
                        insert.push_values(chunk.iter(), |mut values, content| {
                            let scope_id = scopes
                                .get(content.principal.as_str())
                                .copied()
                                .expect("history content scope");
                            values
                                .push_bind(content.id)
                                .push_bind(scope_id)
                                .push_bind(content.digest.as_slice())
                                .push_bind(content.content.as_slice());
                        });
                        insert.build().execute(&mut *transaction).await?;
                    }
                    if done {
                        if !identity_reset.is_empty() {
                            sqlx::query(identity_reset)
                                .execute(&mut *transaction)
                                .await?;
                        }
                        mark_work(&mut transaction, 2, None, None).await?;
                    } else {
                        mark_work(&mut transaction, 1, None, Some(last_id)).await?;
                    }
                    transaction.commit().await?;
                    completed += u64::try_from(prepared.len())?;
                    crate::startup_progress::report(
                        "history_data",
                        "Normalizing history content",
                        completed,
                        Some(total),
                    );
                    if done {
                        return Ok(true);
                    }
                    cursor = Some(last_id);
                }
            }
            async fn stage_descriptors(
                connection: &mut $conn,
                work: &Work,
                limits: Limits,
                budget: &mut Budget,
            ) -> anyhow::Result<bool> {
                let total: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_chain_nodes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let total = u64::try_from(total)?;
                let mut completed = 0u64;
                let mut cursor = work.last_node_id.clone();
                crate::startup_progress::report(
                    "history_data",
                    "Normalizing history descriptors",
                    0,
                    Some(total),
                );
                loop {
                    if !budget.take() {
                        return Ok(false);
                    }
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT id, CAST(length(payload) AS BIGINT) AS encoded_len, \
                         substr(payload, length(payload) - 13, 14) AS trailer \
                         FROM turn_chain_nodes",
                    );
                    if let Some(cursor) = &cursor {
                        query.push(" WHERE id > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY id LIMIT ")
                        .push_bind(i64::try_from(limits.node_batch)?);
                    let thin: Vec<ThinNode> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if thin.is_empty() {
                        let mut transaction = connection.begin().await?;
                        mark_work(&mut transaction, 3, None, None).await?;
                        transaction.commit().await?;
                        return Ok(true);
                    }
                    let thin_len = thin.len();
                    let mut thin_ids = Vec::with_capacity(thin_len);
                    let mut node_costs = Vec::with_capacity(thin_len);
                    for row in thin {
                        let (id, cost) = thin_node_cost(row)?;
                        thin_ids.push(id);
                        node_costs.push(cost);
                    }
                    // Pick the largest database-id-ordered prefix whose own
                    // source bytes fit. The first node is always kept so one
                    // oversized payload still makes progress.
                    let mut initial = 0usize;
                    let mut node_bytes = 0usize;
                    for cost in &node_costs {
                        let next = node_bytes.saturating_add(*cost);
                        if initial > 0 && next > limits.content_bytes {
                            break;
                        }
                        node_bytes = next;
                        initial += 1;
                    }
                    ensure!(initial > 0, "legacy history node batch changed");

                    let mut query = sqlx::QueryBuilder::<$db>::new(LEGACY_NODE_SELECT);
                    if let Some(cursor) = &cursor {
                        query.push(" WHERE id > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY id LIMIT ")
                        .push_bind(i64::try_from(initial)?);
                    let full: Vec<LegacyNode> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    ensure!(
                        full.len() == initial,
                        "legacy history node batch changed"
                    );
                    for (expected, row) in thin_ids[..initial].iter().zip(&full) {
                        ensure!(
                            expected == &row.meta.id,
                            "legacy history node batch changed"
                        );
                    }
                    let mut parsed: Vec<(LegacyMeta, Parsed)> =
                        super::super::blocking_chunks(full, parse_node).await?;
                    ensure!(parsed.len() == initial, "legacy history node batch changed");

                    // Profile bodies are only known after parsing the candidate
                    // prefix. Price each unique (principal, key) once, then
                    // shrink the contiguous prefix by the same source-byte
                    // budget. The first node keeps all of its profiles even if
                    // that single node is over budget.
                    let mut node_profiles = Vec::with_capacity(parsed.len());
                    let mut candidate_wanted: HashMap<String, HashSet<String>> = HashMap::new();
                    for (meta, parsed) in &parsed {
                        let keys = profile_keys(meta, parsed);
                        for (principal, key) in &keys {
                            candidate_wanted
                                .entry(principal.clone())
                                .or_default()
                                .insert(key.clone());
                        }
                        node_profiles.push(keys);
                    }
                    let profile_costs = if candidate_wanted.is_empty() {
                        ProfileCosts::default()
                    } else {
                        load_profile_costs(&mut *connection, &candidate_wanted).await?
                    };
                    let mut selected = 0usize;
                    let mut selected_node_bytes = 0usize;
                    let mut selected_profile_bytes = 0usize;
                    let mut selected_profiles: HashSet<(String, String)> = HashSet::new();
                    for index in 0..parsed.len() {
                        let mut added_profile_bytes = 0usize;
                        let mut new_keys: Vec<&(String, String)> = Vec::new();
                        for key in &node_profiles[index] {
                            if selected_profiles.contains(key) {
                                continue;
                            }
                            let cost = profile_costs
                                .get(key.0.as_str(), key.1.as_str())
                                .context("missing legacy history profile content")?;
                            added_profile_bytes =
                                added_profile_bytes.saturating_add(cost.budget());
                            new_keys.push(key);
                        }
                        let next_node = selected_node_bytes.saturating_add(node_costs[index]);
                        let next_profile =
                            selected_profile_bytes.saturating_add(added_profile_bytes);
                        if selected > 0
                            && next_node.saturating_add(next_profile) > limits.content_bytes
                        {
                            break;
                        }
                        selected += 1;
                        selected_node_bytes = next_node;
                        selected_profile_bytes = next_profile;
                        for key in new_keys {
                            selected_profiles.insert((*key).clone());
                        }
                    }
                    ensure!(selected > 0, "legacy history node batch changed");
                    let selected_all = selected == thin_len;
                    let done = selected_all && thin_len < limits.node_batch;
                    let last = parsed[selected - 1].0.id.clone();
                    parsed.truncate(selected);
                    drop(node_profiles);
                    drop(candidate_wanted);
                    drop(selected_profiles);
                    drop(thin_ids);
                    let batch = Arc::new(
                        load_batch(&mut *connection, &parsed, &profile_costs, limits).await?,
                    );
                    drop(profile_costs);
                    drop(node_costs);
                    let conversion = Arc::clone(&batch);
                    let ready: Vec<Ready> = parsed
                        .into_iter()
                        .map(|(meta, parsed)| Ready { meta, parsed })
                        .collect();
                    let mut plans = super::super::blocking_chunks(ready, move |ready| {
                        convert_node(ready, &conversion)
                    })
                    .await?;
                    let batch = Arc::try_unwrap(batch)
                        .map_err(|_| anyhow::anyhow!("history profile batch still in use"))?;
                    let mut identity_ids: Vec<&str> = Vec::new();
                    let mut seen = HashSet::new();
                    for plan in &plans {
                        if seen.insert(plan.meta.id.as_str()) {
                            identity_ids.push(plan.meta.id.as_str());
                        }
                        if let Some(parent) = plan.meta.parent_id.as_deref() {
                            if seen.insert(parent) {
                                identity_ids.push(parent);
                            }
                        }
                    }
                    let identities = load_identities(connection, &identity_ids).await?;
                    drop(seen);
                    drop(identity_ids);
                    let mut transaction = connection.begin().await?;
                    if !$lock_nodes.is_empty() {
                        sqlx::query($lock_nodes)
                            .execute(&mut *transaction)
                            .await?;
                    }
                    if !$lock_contents.is_empty() {
                        sqlx::query($lock_contents)
                            .execute(&mut *transaction)
                            .await?;
                    }
                    intern_profiles(&mut transaction, &mut plans, batch, &identities).await?;
                    let batch_len = u64::try_from(plans.len())?;
                    for plan in plans {
                        write_plan(&mut transaction, plan, &identities).await?;
                    }
                    if done {
                        mark_work(&mut transaction, 3, None, None).await?;
                    } else {
                        mark_work(&mut transaction, 2, Some(last.as_str()), None).await?;
                    }
                    transaction.commit().await?;
                    completed += batch_len;
                    crate::startup_progress::report(
                        "history_data",
                        "Normalizing history descriptors",
                        completed,
                        Some(total),
                    );
                    if done {
                        return Ok(true);
                    }
                    cursor = Some(last);
                }
            }

            /// Reads every legacy content id and profile body required by this
            /// node worklist in bounded, principal-scoped batches. Ids and
            /// profile bodies are read once per distinct key; an empty profile
            /// set performs no profile SQL.
            async fn load_batch(
                connection: &mut $conn,
                parsed: &[(LegacyMeta, Parsed)],
                profile_costs: &ProfileCosts,
                limits: Limits,
            ) -> anyhow::Result<LegacyBatch> {
                let mut ids_wanted: HashMap<&str, HashSet<&str>> = HashMap::new();
                let mut profiles_wanted: HashMap<&str, HashSet<&str>> = HashMap::new();
                let mut relatives: HashMap<(&str, &str), Vec<String>> = HashMap::new();
                for (meta, parsed) in parsed {
                    let principal = meta.principal.as_str();
                    let keys = ids_wanted.entry(principal).or_default();
                    for key in &parsed.keys {
                        keys.insert(key.as_str());
                    }
                    for (pointer, key_index) in &parsed.slots {
                        if format2::is_profile_pointer(pointer) {
                            profiles_wanted
                                .entry(principal)
                                .or_default()
                                .insert(parsed.keys[*key_index].as_str());
                        }
                    }
                    for (child, ancestor) in parsed.ancestors.iter().enumerate() {
                        let Some(ancestor) = *ancestor else {
                            continue;
                        };
                        let (pointer, key_index) = &parsed.slots[ancestor];
                        if !format2::is_profile_pointer(pointer) {
                            continue;
                        }
                        let relative = &parsed.slots[child].0[pointer.len()..];
                        ensure!(
                            relative.starts_with('/'),
                            "invalid legacy history slot pointer"
                        );
                        relatives
                            .entry((principal, parsed.keys[*key_index].as_str()))
                            .or_default()
                            .push(relative.to_owned());
                    }
                }
                let ids = load_content_ids(&mut *connection, &ids_wanted).await?;
                validate_legacy_references(&mut *connection, parsed, &ids).await?;
                let profiles = if profiles_wanted.is_empty() {
                    ProfileStore::default()
                } else {
                    load_profiles(
                        &mut *connection,
                        &profiles_wanted,
                        &relatives,
                        profile_costs,
                        limits,
                    )
                    .await?
                };
                Ok(LegacyBatch { ids, profiles })
            }

            /// 旧 descriptor 与回收引用必须一致；不能凭同 Principal 的其它内容静默修复坏引用。
            async fn validate_legacy_references(
                connection: &mut $conn,
                parsed: &[(LegacyMeta, Parsed)],
                content_ids: &HashMap<String, HashMap<String, i64>>,
            ) -> anyhow::Result<()> {
                for chunk in parsed.chunks(400) {
                    let mut expected: HashMap<&str, HashSet<i64>> = HashMap::with_capacity(chunk.len());
                    for (meta, descriptor) in chunk {
                        let mut ids = HashSet::with_capacity(descriptor.keys.len());
                        for key in &descriptor.keys {
                            let id = content_ids.get(meta.principal.as_str())
                                .and_then(|ids| ids.get(key.as_str()))
                                .context("missing legacy history content")?;
                            ensure!(ids.insert(*id), "duplicate legacy history content reference");
                        }
                        expected.insert(meta.id.as_str(), ids);
                    }
                    // SQLite 从有界 node worklist 驱动旧引用索引，不扫描全部存量引用。
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT n.id, r.content_id, r.principal = n.principal \
                         FROM turn_chain_nodes n CROSS JOIN turn_chain_node_contents r \
                         WHERE r.node_id = n.id AND n.id IN (",
                    );
                    let mut list = query.separated(",");
                    for (meta, _) in chunk {
                        list.push_bind(meta.id.as_str());
                    }
                    list.push_unseparated(")");
                    let rows: Vec<(String, i64, bool)> = query.build_query_as()
                        .fetch_all(&mut *connection).await?;
                    for (node, content_id, owned) in rows {
                        ensure!(owned, "legacy history reference ownership mismatch");
                        let remaining = expected.get_mut(node.as_str())
                            .context("unexpected legacy history reference node")?;
                        ensure!(remaining.remove(&content_id), "unexpected legacy history content reference");
                    }
                    ensure!(expected.values().all(HashSet::is_empty), "missing legacy history content reference");
                }
                Ok(())
            }

            async fn load_content_ids(
                connection: &mut $conn,
                wanted: &HashMap<&str, HashSet<&str>>,
            ) -> anyhow::Result<HashMap<String, HashMap<String, i64>>> {
                let mut ids = HashMap::with_capacity(wanted.len());
                for (principal, keys) in wanted {
                    let mut ordered: Vec<&str> = keys.iter().copied().collect();
                    ordered.sort_unstable();
                    let mut found = HashMap::with_capacity(ordered.len());
                    for chunk in ordered.chunks(400) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "SELECT content_key, id FROM turn_chain_contents \
                             WHERE principal = ",
                        );
                        query.push_bind(*principal).push(" AND content_key IN (");
                        let mut list = query.separated(",");
                        for key in chunk {
                            list.push_bind(*key);
                        }
                        list.push_unseparated(")");
                        let rows: Vec<(String, i64)> = query
                            .build_query_as()
                            .fetch_all(&mut *connection)
                            .await?;
                        for (key, id) in rows {
                            ensure!(
                                found.insert(key, id).is_none(),
                                "duplicate legacy history content"
                            );
                        }
                    }
                    ensure!(
                        found.len() == ordered.len(),
                        "missing legacy history content"
                    );
                    ids.insert((*principal).to_owned(), found);
                }
                Ok(ids)
            }

            /// Reads only metadata for every distinct profile required by the
            /// candidate prefix: encoded row length plus the codec trailer.
            /// No profile body is fetched here; the caller prices unique
            /// `(principal, key)` pairs before shrinking the node prefix.
            async fn load_profile_costs(
                connection: &mut $conn,
                wanted: &HashMap<String, HashSet<String>>,
            ) -> anyhow::Result<ProfileCosts> {
                let mut costs = ProfileCosts::default();
                for (principal, keys) in wanted {
                    let mut ordered: Vec<&str> =
                        keys.iter().map(String::as_str).collect();
                    ordered.sort_unstable();
                    let mut found = 0usize;
                    for chunk in ordered.chunks(400) {
                        let mut query = sqlx::QueryBuilder::<$db>::new(
                            "SELECT principal, content_key, \
                             CAST(length(content) AS BIGINT) AS content_bytes, \
                             substr(content, length(content) - 13, 14) AS trailer \
                             FROM turn_chain_contents WHERE principal = ",
                        );
                        query.push_bind(principal.as_str()).push(" AND content_key IN (");
                        let mut list = query.separated(",");
                        for key in chunk {
                            list.push_bind(*key);
                        }
                        list.push_unseparated(")");
                        let rows: Vec<LegacyProfileMeta> = query
                            .build_query_as()
                            .fetch_all(&mut *connection)
                            .await?;
                        for row in rows {
                            let LegacyProfileMeta {
                                principal: row_principal,
                                content_key,
                                content_bytes,
                                trailer,
                            } = row;
                            ensure!(
                                row_principal.as_str() == principal.as_str(),
                                "legacy history profile batch changed"
                            );
                            let encoded = usize::try_from(content_bytes)
                                .context("invalid legacy history profile size")?;
                            let decoded =
                                crate::storage_codec::decoded_size_hint(&trailer)
                                    .context("invalid legacy history profile encoding/content")?;
                            ensure!(
                                encoded >= trailer.len(),
                                "invalid legacy history profile size"
                            );
                            ensure!(
                                costs
                                    .by_principal
                                    .entry(row_principal)
                                    .or_default()
                                    .insert(
                                        content_key,
                                        ProfileCost { encoded, decoded },
                                    )
                                    .is_none(),
                                "duplicate legacy history profile content"
                            );
                            found += 1;
                        }
                    }
                    ensure!(
                        found == ordered.len(),
                        "missing legacy history profile content"
                    );
                }
                Ok(costs)
            }

            /// Loads distinct profile bodies for the worklist in row- and
            /// encoded-byte-bounded batches. At least one body is chosen per
            /// batch so a single oversized profile still makes progress.
            async fn load_profiles(
                connection: &mut $conn,
                wanted: &HashMap<&str, HashSet<&str>>,
                relatives: &HashMap<(&str, &str), Vec<String>>,
                costs: &ProfileCosts,
                limits: Limits,
            ) -> anyhow::Result<ProfileStore> {
                let mut raw = Vec::new();
                for (principal, keys) in wanted {
                    let mut remaining: Vec<&str> = keys.iter().copied().collect();
                    remaining.sort_unstable();
                    while !remaining.is_empty() {
                        let take = remaining.len().min(limits.content_batch).min(400);
                        let window = &remaining[..take];
                        let mut chosen = 0usize;
                        let mut bytes = 0usize;
                        for key in window {
                            let cost = costs
                                .get(*principal, *key)
                                .context("missing legacy history profile content")?;
                            bytes = bytes
                                .checked_add(cost.encoded)
                                .context("legacy history profile batch size overflow")?;
                            chosen += 1;
                            if bytes >= limits.content_bytes {
                                break;
                            }
                        }
                        let chosen_keys = &window[..chosen];
                        let rows =
                            load_profile_rows(&mut *connection, *principal, chosen_keys).await?;
                        ensure!(
                            rows.len() == chosen,
                            "legacy history profile batch changed"
                        );
                        let mut rows: HashMap<String, Vec<u8>> = rows.into_iter().collect();
                        ensure!(
                            rows.len() == chosen,
                            "duplicate legacy history profile content"
                        );
                        for key in chosen_keys {
                            let encoded = rows
                                .remove(*key)
                                .expect("legacy history profile batch changed");
                            raw.push(RawProfile {
                                principal: (*principal).to_owned(),
                                key: (*key).to_owned(),
                                encoded,
                                relatives: relatives
                                    .get(&(*principal, *key))
                                    .cloned()
                                    .unwrap_or_default(),
                            });
                        }
                        remaining.drain(..chosen);
                    }
                }
                let prepared = super::super::blocking_chunks(raw, prepare_profile).await?;
                let mut store = ProfileStore::default();
                for (principal, key, profile) in prepared {
                    let index = store.entries.len();
                    ensure!(
                        store
                            .index
                            .entry(principal)
                            .or_default()
                            .insert(key, index)
                            .is_none(),
                        "duplicate legacy history content"
                    );
                    store.entries.push(profile);
                }
                Ok(store)
            }

            async fn load_profile_rows(
                connection: &mut $conn,
                principal: &str,
                keys: &[&str],
            ) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "SELECT content_key, content FROM turn_chain_contents \
                     WHERE principal = ",
                );
                query.push_bind(principal).push(" AND content_key IN (");
                let mut list = query.separated(",");
                for key in keys {
                    list.push_bind(*key);
                }
                list.push_unseparated(")");
                Ok(query.build_query_as().fetch_all(&mut *connection).await?)
            }

            /// Interns every shared profile content once per principal, then
            /// replaces the plan's shared references with the resolved ids.
            async fn intern_profiles(
                connection: &mut $conn,
                plans: &mut [Plan],
                batch: LegacyBatch,
                identities: &HashMap<String, ExistingNode>,
            ) -> anyhow::Result<()> {
                let mut profiles = batch.profiles;
                let mut used: HashMap<String, Vec<usize>> = HashMap::new();
                let mut seen = HashSet::new();
                for plan in plans.iter() {
                    for (_, slot) in &plan.slots {
                        if let SlotRef::Profile { profile, .. } = slot {
                            if seen.insert(*profile) {
                                used.entry(plan.meta.principal.clone())
                                    .or_default()
                                    .push(*profile);
                            }
                        }
                    }
                }
                if used.is_empty() {
                    return Ok(());
                }
                let mut scopes: HashMap<String, i64> = HashMap::new();
                for plan in plans.iter() {
                    if !scopes.contains_key(plan.meta.principal.as_str()) {
                        let existing = identities
                            .get(&plan.meta.id)
                            .context("missing normalized history node")?;
                        scopes.insert(plan.meta.principal.clone(), existing.scope_id);
                    }
                }
                let mut resolved: HashMap<(usize, usize), i64> = HashMap::new();
                for (principal, entries) in used {
                    let scope_id = *scopes
                        .get(principal.as_str())
                        .context("missing normalized history scope")?;
                    let mut flat = Vec::new();
                    let mut layout = Vec::with_capacity(entries.len());
                    for entry in entries {
                        let contents = &mut profiles.entries[entry].contents;
                        layout.push((entry, flat.len(), contents.len()));
                        flat.append(contents);
                    }
                    let ids = content::$intern(&mut *connection, scope_id, &mut flat).await?;
                    for (entry, base, len) in layout {
                        for offset in 0..len {
                            ensure!(
                                resolved
                                    .insert((entry, offset), ids[base + offset])
                                    .is_none(),
                                "duplicate normalized history profile content"
                            );
                        }
                    }
                }
                for plan in plans.iter_mut() {
                    for (_, slot) in &mut plan.slots {
                        if let SlotRef::Profile { profile, content } = slot {
                            let profile = *profile;
                            let content = *content;
                            *slot = SlotRef::Preserved(
                                *resolved
                                    .get(&(profile, content))
                                    .context("missing normalized history profile content id")?,
                            );
                        }
                    }
                }
                Ok(())
            }

            async fn load_identities(
                connection: &mut $conn,
                ids: &[&str],
            ) -> anyhow::Result<HashMap<String, ExistingNode>> {
                let mut identities = HashMap::with_capacity(ids.len());
                for chunk in ids.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT n.id AS id, n.node_pk AS node_pk, n.scope_id AS scope_id, \
                         s.principal AS principal, n.kind AS kind FROM turn_nodes n \
                         JOIN turn_scopes s ON s.id = n.scope_id WHERE n.id IN (",
                    );
                    let mut list = query.separated(",");
                    for id in chunk {
                        list.push_bind(*id);
                    }
                    list.push_unseparated(")");
                    let rows: Vec<ExistingNode> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    for row in rows {
                        identities.insert(row.id.clone(), row);
                    }
                }
                Ok(identities)
            }

            async fn write_plan(
                connection: &mut $conn,
                plan: Plan,
                identities: &HashMap<String, ExistingNode>,
            ) -> anyhow::Result<()> {
                let Plan {
                    meta,
                    data,
                    slots,
                    mut contents,
                } = plan;
                let existing = identities
                    .get(&meta.id)
                    .context("missing normalized history node")?;
                ensure!(
                    existing.principal == meta.principal && existing.kind == meta.kind,
                    "normalized history node ownership mismatch"
                );
                let parent_pk = match meta.parent_id.as_deref() {
                    Some(parent_id) => {
                        let parent = identities
                            .get(parent_id)
                            .context("missing normalized history parent")?;
                        ensure!(
                            parent.principal == meta.principal && parent.kind == meta.kind,
                            "normalized history parent ownership mismatch"
                        );
                        ensure!(
                            parent.node_pk != existing.node_pk,
                            "history node is its own parent"
                        );
                        Some(parent.node_pk)
                    }
                    None => None,
                };
                let scope_id = existing.scope_id;
                let new_ids = if contents.is_empty() {
                    Vec::new()
                } else {
                    content::$intern(connection, scope_id, &mut contents).await?
                };
                let mut distinct = Vec::new();
                let mut ordinals: HashMap<i64, usize> = HashMap::new();
                let mut encoded_slots = Vec::with_capacity(slots.len());
                for (pointer, slot) in slots {
                    let id = match slot {
                        SlotRef::Preserved(id) => id,
                        SlotRef::Fresh(index) => *new_ids
                            .get(index)
                            .context("missing normalized history content id")?,
                        SlotRef::Profile { .. } => {
                            bail!("unresolved normalized history profile slot")
                        }
                    };
                    let ordinal = match ordinals.get(&id) {
                        Some(ordinal) => *ordinal,
                        None => {
                            let ordinal = distinct.len();
                            distinct.push(id);
                            ordinals.insert(id, ordinal);
                            ordinal
                        }
                    };
                    encoded_slots.push((pointer, ordinal));
                }
                let payload = content::encode_descriptor(&data, &encoded_slots)?;
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_node_payloads (node_pk, payload) VALUES (",
                );
                query
                    .push_bind(existing.node_pk)
                    .push(",")
                    .push_bind(payload)
                    .push(")");
                let affected = query
                    .build()
                    .execute(&mut *connection)
                    .await?
                    .rows_affected();
                ensure!(affected == 1, "history node payload already normalized");
                let mut query = sqlx::QueryBuilder::<$db>::new(
                    "INSERT INTO turn_node_retention (node_pk, expires_at) VALUES (",
                );
                query
                    .push_bind(existing.node_pk)
                    .push(",")
                    .push_bind(meta.expires_at)
                    .push(")");
                let affected = query
                    .build()
                    .execute(&mut *connection)
                    .await?
                    .rows_affected();
                ensure!(affected == 1, "history node retention already normalized");
                if let Some(parent_pk) = parent_pk {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "UPDATE turn_nodes SET parent_pk = ",
                    );
                    query
                        .push_bind(parent_pk)
                        .push(" WHERE node_pk = ")
                        .push_bind(existing.node_pk)
                        .push(" AND parent_pk IS NULL");
                    let affected = query
                        .build()
                        .execute(&mut *connection)
                        .await?
                        .rows_affected();
                    ensure!(affected == 1, "history node parent already normalized");
                }
                content::$link(connection, existing.node_pk, scope_id, &distinct).await?;
                Ok(())
            }

            async fn load_links(
                connection: &mut $conn,
                node_pks: &[i64],
            ) -> anyhow::Result<HashMap<i64, Vec<(i64, i64)>>> {
                let mut links: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
                for chunk in node_pks.chunks(400) {
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT node_pk, ordinal, content_id FROM turn_node_contents \
                         WHERE node_pk IN (",
                    );
                    let mut list = query.separated(",");
                    for node_pk in chunk {
                        list.push_bind(*node_pk);
                    }
                    list.push_unseparated(") ORDER BY node_pk, ordinal");
                    let rows: Vec<(i64, i64, i64)> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    for (node_pk, ordinal, content_id) in rows {
                        links
                            .entry(node_pk)
                            .or_default()
                            .push((ordinal, content_id));
                    }
                }
                Ok(links)
            }

            async fn validate_counts(connection: &mut $conn) -> anyhow::Result<()> {
                let old_nodes: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_chain_nodes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let nodes: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_nodes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                ensure!(
                    old_nodes == nodes,
                    "normalized history node count mismatch"
                );
                let payloads: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_node_payloads",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let retention: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_node_retention",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                ensure!(
                    nodes == payloads && nodes == retention,
                    "normalized history node facts are incomplete"
                );
                let old_parents: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_chain_nodes \
                     WHERE parent_id IS NOT NULL",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let parents: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_nodes \
                     WHERE parent_pk IS NOT NULL",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                ensure!(
                    old_parents == parents,
                    "normalized history parent count mismatch"
                );
                let old_scopes: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(DISTINCT principal) AS BIGINT) FROM turn_chain_nodes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let scopes: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_scopes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                ensure!(
                    scopes >= old_scopes,
                    "normalized history scope count mismatch"
                );
                let bad_links: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_node_contents nc \
                     LEFT JOIN turn_contents c \
                     ON c.id = nc.content_id AND c.scope_id = nc.scope_id \
                     WHERE c.id IS NULL",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                ensure!(
                    bad_links == 0,
                    "normalized history content ownership mismatch"
                );
                let missing_compaction_source: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM native_compactions c \
                     LEFT JOIN turn_nodes n ON n.id=c.source_generation_id \
                     WHERE c.source_generation_id IS NOT NULL AND n.node_pk IS NULL)",
                )
                .fetch_one(&mut *connection)
                .await?;
                ensure!(!missing_compaction_source, "normalized compaction source is missing");
                Ok(())
            }

            async fn stage_finalize(
                connection: &mut $conn,
                work: &Work,
                budget: &mut Budget,
            ) -> anyhow::Result<bool> {
                validate_counts(connection).await?;
                let total: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_nodes",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                let total = u64::try_from(total)?;
                let mut processed = 0u64;
                let mut cursor: Option<String> = None;
                crate::startup_progress::report(
                    "history_data",
                    "Validating normalized history",
                    0,
                    Some(total),
                );
                loop {
                    let mut query =
                        sqlx::QueryBuilder::<$db>::new(VALIDATION_SELECT);
                    if let Some(cursor) = &cursor {
                        query.push(" WHERE n.id > ").push_bind(cursor);
                    }
                    query
                        .push(" ORDER BY n.id LIMIT ")
                        .push_bind(i64::try_from(VALIDATION_BATCH)?);
                    let rows: Vec<ValidationRow> = query
                        .build_query_as()
                        .fetch_all(&mut *connection)
                        .await?;
                    if rows.is_empty() {
                        break;
                    }
                    let done = rows.len() < VALIDATION_BATCH;
                    let last = rows
                        .last()
                        .map(|row| row.id.clone())
                        .expect("history validation batch");
                    let node_pks: Vec<i64> = rows.iter().map(|row| row.node_pk).collect();
                    let links = load_links(connection, &node_pks).await?;
                    for row in &rows {
                        let node_links = links
                            .get(&row.node_pk)
                            .map(Vec::as_slice)
                            .unwrap_or(&[]);
                        validate_row(row, node_links)?;
                    }
                    processed += u64::try_from(rows.len())?;
                    crate::startup_progress::report(
                        "history_data",
                        "Validating normalized history",
                        processed,
                        Some(total),
                    );
                    if done {
                        break;
                    }
                    cursor = Some(last);
                }
                ensure!(
                    processed == total,
                    "normalized history validation is incomplete"
                );
                let mut orphan_cursor = work.last_content_id;
                loop {
                    if !budget.take() {
                        return Ok(false);
                    }
                    // 单调游标避免每删一批孤儿都重扫前面的 live 内容。
                    let mut query = sqlx::QueryBuilder::<$db>::new(
                        "SELECT c.id,c.scope_id,CAST(length(c.content) AS BIGINT) FROM turn_contents c \
                         WHERE NOT EXISTS(SELECT 1 FROM turn_node_contents nc WHERE nc.content_id=c.id)",
                    );
                    if let Some(cursor) = orphan_cursor {
                        query.push(" AND c.id>").push_bind(cursor);
                    }
                    query.push(" ORDER BY c.id LIMIT 500");
                    let candidates: Vec<(i64,i64,i64)> = query.build_query_as()
                        .fetch_all(&mut *connection).await?;
                    if candidates.is_empty() {
                        break;
                    }
                    let mut ids = Vec::with_capacity(candidates.len());
                    let mut scopes = Vec::new();
                    let mut bytes = 0usize;
                    for (id, scope_id, length) in candidates {
                        let length = usize::try_from(length)?;
                        if !ids.is_empty() && bytes.saturating_add(length) > DEFAULT_CONTENT_BYTES {
                            break;
                        }
                        bytes = bytes.saturating_add(length);
                        ids.push(id);
                        scopes.push(scope_id);
                    }
                    scopes.sort_unstable();
                    scopes.dedup();
                    let last = *ids.last().expect("nonempty orphan batch");
                    let mut transaction = connection.begin().await?;
                    let mut remove = sqlx::QueryBuilder::<$db>::new("DELETE FROM turn_contents WHERE id IN (");
                    let mut list = remove.separated(",");
                    for id in &ids { list.push_bind(*id); }
                    list.push_unseparated(")");
                    remove.push(" AND NOT EXISTS(SELECT 1 FROM turn_node_contents nc WHERE nc.content_id=turn_contents.id)");
                    let deleted = remove.build().execute(&mut *transaction).await?.rows_affected();
                    ensure!(deleted == u64::try_from(ids.len())?, "history orphan batch changed during conversion");
                    // Stage 1 also interns scopes for principals that only owned
                    // orphan contents. Runtime GC only revisits scopes of deleted
                    // nodes, so prune this batch's scopes after their contents are
                    // gone; the guards keep any scope that still owns nodes or
                    // contents from a later batch.
                    let mut prune = sqlx::QueryBuilder::<$db>::new("DELETE FROM turn_scopes WHERE id IN (");
                    let mut list = prune.separated(",");
                    for scope in &scopes { list.push_bind(*scope); }
                    list.push_unseparated(")");
                    prune.push(" AND NOT EXISTS(SELECT 1 FROM turn_nodes n WHERE n.scope_id=turn_scopes.id) \
                                AND NOT EXISTS(SELECT 1 FROM turn_contents c WHERE c.scope_id=turn_scopes.id)");
                    prune.build().execute(&mut *transaction).await?;
                    mark_work(&mut transaction, 3, None, Some(last)).await?;
                    transaction.commit().await?;
                    orphan_cursor = Some(last);
                }
                let orphans: i64 = sqlx::QueryBuilder::<$db>::new(
                    "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_contents c \
                     WHERE NOT EXISTS (SELECT 1 FROM turn_node_contents nc \
                     WHERE nc.content_id = c.id)",
                )
                .build_query_scalar()
                .fetch_one(&mut *connection)
                .await?;
                ensure!(
                    orphans == 0,
                    "normalized history has unreferenced content"
                );
                let mut transaction = connection.begin().await?;
                mark_work(&mut transaction, STAGE_DONE, None, None).await?;
                transaction.commit().await?;
                Ok(true)
            }
        }
    };
}

backend!(
    sqlite,
    sqlx::Sqlite,
    sqlx::SqliteConnection<'_>,
    "",
    "",
    intern_sqlite,
    link_sqlite,
    ""
);
backend!(
    postgres,
    sqlx::Postgres,
    sqlx::PgConnection<'_>,
    "LOCK TABLE turn_nodes IN ROW EXCLUSIVE MODE",
    "LOCK TABLE turn_contents IN ROW EXCLUSIVE MODE",
    intern_postgres,
    link_postgres,
    "SELECT setval(pg_get_serial_sequence('turn_contents', 'id'), \
     GREATEST(COALESCE(MAX(id), 0), 1), MAX(id) IS NOT NULL) FROM turn_contents"
);

pub(crate) use postgres::run as run_postgres;
pub(crate) use sqlite::run as run_sqlite;
