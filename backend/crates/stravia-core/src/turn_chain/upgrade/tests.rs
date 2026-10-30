use anyhow::{Context, ensure};
use serde_json::{Value, json};
use stravia_runtime_contract::turn_chain::{TurnNodeId, TurnNodeKind};

use crate::turn_chain::content;

use super::format2;
use super::normalize::{Limits, run_sqlite};
use super::normalize_sqlite;

const LEGACY_NODE_INSERT: &str = "INSERT INTO turn_chain_nodes \
    (id, kind, parent_id, principal, payload_version, payload, created_at, expires_at, storage_format) \
    VALUES ($1, $2, $3, $4, 1, $5, 0, $6, 2)";

async fn fixture() -> anyhow::Result<sqlx::SqliteConnection<'static>> {
    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
    let all = sqlx::migrate!("./migrations/sqlite");
    let migrator = sqlx::migrate::Migrator::with_migrations(
        all.iter()
            .filter(|migration| migration.version <= 32)
            .cloned()
            .collect(),
    );
    migrator
        .run_direct(None, &mut connection, false)
        .await
        .context("applying migrations through 0032")?;
    Ok(connection)
}

async fn insert_legacy_node(
    connection: &mut sqlx::SqliteConnection<'_>,
    id: &str,
    principal: &str,
    kind: &str,
    parent: Option<&str>,
    payload: Value,
    expires_at: i64,
) -> anyhow::Result<()> {
    let encoded = format2::encode(payload)?;
    sqlx::query(LEGACY_NODE_INSERT)
        .bind(id)
        .bind(kind)
        .bind(parent)
        .bind(principal)
        .bind(&encoded.payload)
        .bind(expires_at)
        .execute(&mut *connection)
        .await?;
    for (key, raw) in encoded.contents {
        let content = crate::storage_codec::encode(raw.into())?;
        sqlx::query(
            "INSERT INTO turn_chain_contents (principal, content_key, content) \
             VALUES ($1, $2, $3) ON CONFLICT (principal, content_key) DO NOTHING",
        )
        .bind(principal)
        .bind(&key)
        .bind(&content)
        .execute(&mut *connection)
        .await?;
        let content_id: i64 = sqlx::query_scalar(
            "SELECT id FROM turn_chain_contents WHERE principal = $1 AND content_key = $2",
        )
        .bind(principal)
        .bind(&key)
        .fetch_one(&mut *connection)
        .await?;
        sqlx::query(
            "INSERT INTO turn_chain_node_contents (node_id, principal, content_id) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(principal)
        .bind(content_id)
        .execute(&mut *connection)
        .await?;
    }
    Ok(())
}

async fn insert_raw_legacy_node(
    connection: &mut sqlx::SqliteConnection<'_>,
    id: &str,
    principal: &str,
    kind: &str,
    parent: Option<&str>,
    payload: &[u8],
    expires_at: i64,
) -> anyhow::Result<()> {
    sqlx::query(LEGACY_NODE_INSERT)
        .bind(id)
        .bind(kind)
        .bind(parent)
        .bind(principal)
        .bind(payload)
        .bind(expires_at)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

async fn insert_legacy_content(
    connection: &mut sqlx::SqliteConnection<'_>,
    principal: &str,
    key: &str,
    raw: &[u8],
) -> anyhow::Result<i64> {
    let content = crate::storage_codec::encode(raw.to_vec().into())?;
    sqlx::query(
        "INSERT INTO turn_chain_contents (principal, content_key, content) VALUES ($1, $2, $3)",
    )
    .bind(principal)
    .bind(key)
    .bind(content)
    .execute(&mut *connection)
    .await?;
    Ok(sqlx::query_scalar(
        "SELECT id FROM turn_chain_contents WHERE principal = $1 AND content_key = $2",
    )
    .bind(principal)
    .bind(key)
    .fetch_one(&mut *connection)
    .await?)
}

async fn descriptor(
    connection: &mut sqlx::SqliteConnection<'_>,
    id: &str,
) -> anyhow::Result<Value> {
    let payload: Vec<u8> = sqlx::query_scalar(
        "SELECT p.payload FROM turn_node_payloads p \
         JOIN turn_nodes n ON n.node_pk = p.node_pk WHERE n.id = $1",
    )
    .bind(id)
    .fetch_one(&mut *connection)
    .await?;
    let raw = crate::storage_codec::decode(&payload)?;
    Ok(serde_json::from_slice(&raw)?)
}

fn slot_ordinal(descriptor: &Value, pointer: &str) -> u64 {
    descriptor["slots"]
        .as_array()
        .expect("descriptor slots")
        .iter()
        .find(|slot| slot[0].as_str() == Some(pointer))
        .unwrap_or_else(|| panic!("missing slot {pointer}"))[1]
        .as_u64()
        .expect("slot ordinal")
}

async fn restore_one(
    connection: &mut sqlx::SqliteConnection<'_>,
    id: &str,
    principal: &str,
) -> anyhow::Result<Value> {
    #[derive(sqlx::FromRow)]
    struct StoredFixture {
        node_pk: i64,
        id: String,
        kind: String,
        parent_id: Option<String>,
        payload_version: i64,
        payload: Vec<u8>,
        scope_id: i64,
        content_references: sqlx::types::Json<Vec<[i64; 3]>>,
    }
    let row: StoredFixture = sqlx::query_as(concat!(
        "SELECT n.node_pk, n.id, n.kind, pn.id AS parent_id, n.payload_version, p.payload, n.scope_id,",
        content::reference_projection!(sqlite),
        " FROM turn_nodes n \
         JOIN turn_scopes s ON s.id = n.scope_id \
         JOIN turn_node_payloads p ON p.node_pk = n.node_pk \
         LEFT JOIN turn_nodes pn ON pn.node_pk = n.parent_pk \
         WHERE s.principal = $1 AND n.id = $2"
    ))
    .bind(principal)
    .bind(id)
    .fetch_one(&mut *connection)
    .await?;
    let kind = match row.kind.as_str() {
        "response" => TurnNodeKind::Response,
        "agent" => TurnNodeKind::Agent,
        "web_search" => TurnNodeKind::WebSearch,
        other => panic!("unexpected kind {other}"),
    };
    let stored = content::StoredNode {
        node_pk: row.node_pk,
        scope_id: row.scope_id,
        references: row.content_references.0,
        id: TurnNodeId::new(row.id),
        kind,
        parent_id: row.parent_id.map(TurnNodeId::new),
        payload_version: u32::try_from(row.payload_version)?,
        payload: row.payload,
    };
    let nodes = content::restore_sqlite(&mut *connection, vec![stored]).await?;
    ensure!(nodes.len() == 1, "restore returned the wrong node count");
    Ok(nodes.into_iter().next().expect("restored node").payload)
}

async fn work_state(
    connection: &mut sqlx::SqliteConnection<'_>,
) -> anyhow::Result<(i64, Option<String>, Option<i64>)> {
    Ok(sqlx::query_as(
        "SELECT stage, last_node_id, last_content_id FROM turn_history_upgrade WHERE phase = 1",
    )
    .fetch_one(&mut *connection)
    .await?)
}

#[tokio::test]
async fn historical_converter_still_writes_legacy_format_two() -> anyhow::Result<()> {
    let mut connection = sqlx::SqliteConnection::connect("sqlite::memory:").await?;
    sqlx::raw_sql(
        "CREATE TABLE turn_chain_nodes (id TEXT PRIMARY KEY, principal TEXT NOT NULL, \
         storage_format INTEGER NOT NULL, legacy_payload TEXT, payload BLOB); \
         CREATE TABLE turn_chain_legacy_refs \
         (node_id TEXT, principal TEXT, path TEXT, content_key TEXT); \
         CREATE TABLE turn_chain_legacy_contents \
         (principal TEXT, content_key TEXT, content TEXT, PRIMARY KEY(principal, content_key)); \
         CREATE TABLE turn_chain_contents \
         (id INTEGER PRIMARY KEY, principal TEXT, content_key TEXT, content BLOB, \
          UNIQUE(principal, content_key)); \
         CREATE TABLE turn_chain_node_contents \
         (node_id TEXT, principal TEXT, content_id INTEGER, PRIMARY KEY(node_id, content_id));",
    )
    .execute(&mut connection)
    .await?;
    let payload =
        json!({"items": [{"text": "x".repeat(300)}], "preserved": ["completed", null, 7]});
    sqlx::query(
        "INSERT INTO turn_chain_nodes \
         (id, principal, storage_format, legacy_payload, payload) VALUES ('n', 'a', 0, $1, X'')",
    )
    .bind(payload.to_string())
    .execute(&mut connection)
    .await?;
    super::convert_history_sqlite(&mut connection).await?;
    let (format, encoded): (i64, Vec<u8>) =
        sqlx::query_as("SELECT storage_format, payload FROM turn_chain_nodes WHERE id = 'n'")
            .fetch_one(&mut connection)
            .await?;
    assert_eq!(format, 2);
    let envelope = format2::decode_envelope(&encoded)?;
    assert_eq!(envelope["data"]["preserved"], json!(["completed", null, 7]));
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT content_key FROM turn_chain_contents WHERE principal = 'a'")
            .fetch_all(&mut connection)
            .await?;
    assert_eq!(keys.len(), 1);
    assert_eq!(envelope["contents"][0].as_str(), Some(keys[0].as_str()));
    let raw: Vec<u8> = sqlx::query_scalar(
        "SELECT content FROM turn_chain_contents WHERE principal = 'a' AND content_key = $1",
    )
    .bind(&keys[0])
    .fetch_one(&mut connection)
    .await?;
    let raw = crate::storage_codec::decode(&raw)?;
    assert_eq!(format2::content_key(&raw), keys[0]);
    Ok(())
}

#[tokio::test]
async fn normalizes_losslessly_and_is_idempotent() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let shared = json!({"text": "y".repeat(300)});
    let profile = json!({
        "system": "inner-small",
        "instructions": "i".repeat(300),
        "items": [shared.clone()],
        "keep": null,
        "padding": "p".repeat(300),
        "escaped/key": {"~": 1},
        "nested": {
            "__open_responses_effective_request": {
                "system": "deep-small",
                "items": [{"text": "d".repeat(300)}],
                "padding": "d".repeat(300)
            }
        },
        "nested_small": {
            "__open_responses_effective_request": {"system": "tiny"}
        }
    });
    let parent = json!({
        "items": [{"text": "x".repeat(400)}, null, shared.clone(), shared.clone()],
        "system": "s".repeat(300),
        "small": "keep-small",
        "nested": {"__open_responses_response_profile": profile},
        "number": serde_json::from_str::<Value>("123456789012345678901234567890")?,
        "kept": {"empty": {}, "list": [], "null": null, "bool": false}
    });
    let child = json!({
        "items": [{"text": "c".repeat(300)}],
        "system": "child-system"
    });
    let other = json!({"preserved": "value", "items": [shared.clone()]});
    insert_legacy_node(
        &mut connection,
        "p",
        "a",
        "response",
        None,
        parent.clone(),
        111,
    )
    .await?;
    insert_legacy_node(
        &mut connection,
        "c",
        "a",
        "response",
        Some("p"),
        child.clone(),
        222,
    )
    .await?;
    insert_legacy_node(&mut connection, "q", "b", "agent", None, other.clone(), 333).await?;
    sqlx::query(
        "UPDATE turn_chain_nodes SET prefix_namespace = 'chat', prefix_fingerprint = 'fp', \
         prefix_item_count = 3, prefix_completed_at = 40, prefix_projection_version = 2, \
         prefix_controls_fingerprint = 'controls', prefix_context_fingerprint = 'context' \
         WHERE id = 'p'",
    )
    .execute(&mut connection)
    .await?;

    normalize_sqlite(&mut connection).await?;

    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    assert_eq!(restore_one(&mut connection, "p", "a").await?, parent);
    assert_eq!(restore_one(&mut connection, "c", "a").await?, child);
    assert_eq!(restore_one(&mut connection, "q", "b").await?, other);

    let stored_descriptor = descriptor(&mut connection, "p").await?;
    assert!(stored_descriptor.get("contents").is_none());
    assert!(stored_descriptor["data"]["system"].is_null());
    assert!(stored_descriptor["data"]["items"][0].is_null());
    assert!(stored_descriptor["data"]["items"][1].is_null());
    assert!(stored_descriptor["data"]["items"][2].is_null());
    assert!(stored_descriptor["data"]["items"][3].is_null());
    assert!(stored_descriptor["data"]["nested"]["__open_responses_response_profile"].is_null());
    assert_eq!(stored_descriptor["data"]["small"], "keep-small");
    assert_eq!(stored_descriptor["data"]["number"], parent["number"]);
    let shared_ordinal = slot_ordinal(&stored_descriptor, "/items/2");
    assert_eq!(shared_ordinal, slot_ordinal(&stored_descriptor, "/items/3"));
    assert_ne!(shared_ordinal, slot_ordinal(&stored_descriptor, "/items/0"));
    assert!(
        stored_descriptor["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .any(
                |slot| slot[0].as_str() == Some("/nested/__open_responses_response_profile/system")
            ),
        "profile inner unit was not externalized"
    );
    let slot_pointers: Vec<&str> = stored_descriptor["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .map(|slot| slot[0].as_str().expect("slot pointer"))
        .collect();
    for pointer in [
        "/nested/__open_responses_response_profile/nested/__open_responses_effective_request",
        "/nested/__open_responses_response_profile/nested/__open_responses_effective_request/system",
        "/nested/__open_responses_response_profile/nested/__open_responses_effective_request/items/0",
        "/nested/__open_responses_response_profile/nested_small/__open_responses_effective_request",
        "/nested/__open_responses_response_profile/nested_small/__open_responses_effective_request/system",
    ] {
        assert!(
            slot_pointers.contains(&pointer),
            "missing normalized profile pointer {pointer}"
        );
    }
    let unique_pointers: std::collections::HashSet<&str> = slot_pointers.iter().copied().collect();
    assert_eq!(unique_pointers.len(), slot_pointers.len());

    let system_raw = serde_json::to_vec(&parent["system"])?;
    let old_system_id: i64 = sqlx::query_scalar(
        "SELECT id FROM turn_chain_contents WHERE principal = 'a' AND content_key = $1",
    )
    .bind(format2::content_key(&system_raw))
    .fetch_one(&mut connection)
    .await?;
    let new_system_id: i64 = sqlx::query_scalar(
        "SELECT nc.content_id FROM turn_node_contents nc \
         JOIN turn_nodes n ON n.node_pk = nc.node_pk \
         WHERE n.id = 'p' AND nc.ordinal = $1",
    )
    .bind(i64::try_from(slot_ordinal(&stored_descriptor, "/system"))?)
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(
        old_system_id, new_system_id,
        "opaque verified content id must be reused"
    );

    let other_descriptor = descriptor(&mut connection, "q").await?;
    let shared_raw = serde_json::to_vec(&shared)?;
    let old_shared: Vec<(String, i64)> = sqlx::query_as(
        "SELECT principal, id FROM turn_chain_contents WHERE content_key = $1 \
         ORDER BY principal",
    )
    .bind(format2::content_key(&shared_raw))
    .fetch_all(&mut connection)
    .await?;
    assert_eq!(old_shared.len(), 2, "fixture needs one copy per scope");
    let new_shared: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT nc.content_id FROM turn_node_contents nc \
                    JOIN turn_nodes n ON n.node_pk = nc.node_pk \
                    WHERE n.id = 'p' AND nc.ordinal = $1), \
                (SELECT nc.content_id FROM turn_node_contents nc \
                    JOIN turn_nodes n ON n.node_pk = nc.node_pk \
                    WHERE n.id = 'q' AND nc.ordinal = $2), \
                (SELECT scope_id FROM turn_nodes WHERE id = 'p'), \
                (SELECT scope_id FROM turn_nodes WHERE id = 'q')",
    )
    .bind(i64::try_from(slot_ordinal(&stored_descriptor, "/items/2"))?)
    .bind(i64::try_from(slot_ordinal(&other_descriptor, "/items/0"))?)
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(
        (new_shared.0, new_shared.1),
        (old_shared[0].1, old_shared[1].1)
    );
    assert_ne!(
        new_shared.2, new_shared.3,
        "principals must not share a scope"
    );

    let (child_parent, parent_pk, expires_at): (Option<i64>, i64, i64) = sqlx::query_as(
        "SELECT c.parent_pk, p.node_pk, r.expires_at FROM turn_nodes c \
         JOIN turn_nodes p ON p.id = 'p' \
         JOIN turn_node_retention r ON r.node_pk = c.node_pk WHERE c.id = 'c'",
    )
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(child_parent, Some(parent_pk));
    assert_eq!(expires_at, 222);
    let prefix: (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT prefix_namespace, prefix_item_count, prefix_context_fingerprint \
         FROM turn_nodes WHERE id = 'p'",
    )
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(
        prefix,
        (Some("chat".into()), Some(3), Some("context".into()))
    );

    let legacy_nodes: i64 =
        sqlx::query_scalar("SELECT CAST(COUNT(*) AS BIGINT) FROM turn_chain_nodes")
            .fetch_one(&mut connection)
            .await?;
    assert_eq!(legacy_nodes, 3);
    let contents_before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT id, content FROM turn_contents ORDER BY id")
            .fetch_all(&mut connection)
            .await?;
    normalize_sqlite(&mut connection).await?;
    let contents_after: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT id, content FROM turn_contents ORDER BY id")
            .fetch_all(&mut connection)
            .await?;
    assert_eq!(contents_before, contents_after);
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    Ok(())
}

#[tokio::test]
async fn resumes_after_budgeted_interruptions() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let mut payloads = Vec::new();
    for index in 0..5 {
        let payload = json!({
            "items": [{"text": format!("{index}").repeat(300)}],
            "tag": index,
        });
        insert_legacy_node(
            &mut connection,
            &format!("n{index}"),
            "a",
            "response",
            None,
            payload.clone(),
            100 + i64::from(index),
        )
        .await?;
        payloads.push(payload);
    }
    let limits = Limits {
        node_batch: 1,
        content_batch: 1,
        content_bytes: 1,
        budget: Some(1),
    };
    let mut saw_content_cursor = false;
    let mut saw_descriptor_cursor = false;
    let mut calls = 0;
    loop {
        let (stage, _, _) = work_state(&mut connection).await?;
        if stage == 4 {
            break;
        }
        run_sqlite(&mut connection, limits).await?;
        calls += 1;
        ensure!(calls < 100, "normalization made no progress");
        let (stage, node_cursor, content_cursor) = work_state(&mut connection).await?;
        if stage == 1 && content_cursor.is_some() {
            saw_content_cursor = true;
        }
        if stage == 2 && node_cursor.is_some() {
            saw_descriptor_cursor = true;
        }
    }
    assert!(saw_content_cursor, "content batches were not interrupted");
    assert!(
        saw_descriptor_cursor,
        "descriptor batches were not interrupted"
    );
    for (index, payload) in payloads.into_iter().enumerate() {
        assert_eq!(
            restore_one(&mut connection, &format!("n{index}"), "a").await?,
            payload
        );
    }
    Ok(())
}

#[tokio::test]
async fn bad_sha_fails_before_advancing_the_content_cursor() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let payload = json!({"items": [{"text": "x".repeat(300)}]});
    insert_legacy_node(
        &mut connection,
        "n",
        "a",
        "response",
        None,
        payload.clone(),
        10,
    )
    .await?;
    let bad_content = crate::storage_codec::encode(b"unverified".to_vec().into())?;
    sqlx::query(
        "INSERT INTO turn_chain_contents (principal, content_key, content) \
         VALUES ('a', 'legacy-bad-key', $1)",
    )
    .bind(bad_content)
    .execute(&mut connection)
    .await?;

    run_sqlite(
        &mut connection,
        Limits {
            node_batch: 64,
            content_batch: 512,
            content_bytes: 8 * 1024 * 1024,
            budget: Some(1),
        },
    )
    .await?;
    let (stage, _, _) = work_state(&mut connection).await?;
    assert_eq!(stage, 1);

    let result = run_sqlite(
        &mut connection,
        Limits {
            node_batch: 64,
            content_batch: 1,
            content_bytes: 8 * 1024 * 1024,
            budget: None,
        },
    )
    .await;
    ensure!(result.is_err(), "damaged content was accepted");
    assert_eq!(work_state(&mut connection).await?, (1, None, Some(1)));

    let raw = b"{}".to_vec();
    let key = format2::content_key(&raw);
    let encoded = crate::storage_codec::encode(raw.into())?;
    sqlx::query("UPDATE turn_chain_contents SET content_key = $1, content = $2 WHERE id = 2")
        .bind(key)
        .bind(encoded)
        .execute(&mut connection)
        .await?;
    normalize_sqlite(&mut connection).await?;
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    assert_eq!(restore_one(&mut connection, "n", "a").await?, payload);
    Ok(())
}

#[tokio::test]
async fn bad_slot_fails_before_advancing_the_node_cursor() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let good = json!({"items": [{"text": "x".repeat(300)}]});
    insert_legacy_node(
        &mut connection,
        "a",
        "a",
        "response",
        None,
        good.clone(),
        10,
    )
    .await?;
    let raw = b"true".to_vec();
    let key = format2::content_key(&raw);
    let content_id = insert_legacy_content(&mut connection, "a", &key, &raw).await?;
    let bad_envelope = json!({
        "data": {"slot": true},
        "slots": [["/slot", 0]],
        "contents": [key.clone()]
    });
    let bad_payload = crate::storage_codec::encode(serde_json::to_vec(&bad_envelope)?.into())?;
    insert_raw_legacy_node(
        &mut connection,
        "b",
        "a",
        "response",
        None,
        &bad_payload,
        20,
    )
    .await?;
    sqlx::query(
        "INSERT INTO turn_chain_node_contents (node_id, principal, content_id) \
         VALUES ('b', 'a', $1)",
    )
    .bind(content_id)
    .execute(&mut connection)
    .await?;

    run_sqlite(
        &mut connection,
        Limits {
            node_batch: 1,
            content_batch: 512,
            content_bytes: 8 * 1024 * 1024,
            budget: Some(3),
        },
    )
    .await?;
    let (stage, _, _) = work_state(&mut connection).await?;
    assert_eq!(stage, 1);
    run_sqlite(
        &mut connection,
        Limits {
            node_batch: 1,
            content_batch: 512,
            content_bytes: 8 * 1024 * 1024,
            budget: Some(1),
        },
    )
    .await?;
    let (stage, _, _) = work_state(&mut connection).await?;
    assert_eq!(stage, 2);

    let result = run_sqlite(
        &mut connection,
        Limits {
            node_batch: 1,
            content_batch: 512,
            content_bytes: 8 * 1024 * 1024,
            budget: None,
        },
    )
    .await;
    ensure!(result.is_err(), "slot overwrite was accepted");
    assert_eq!(
        work_state(&mut connection).await?,
        (2, Some("a".into()), None)
    );
    let a_payload: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_node_payloads p \
         JOIN turn_nodes n ON n.node_pk = p.node_pk WHERE n.id = 'a'",
    )
    .fetch_one(&mut connection)
    .await?;
    let b_payload: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_node_payloads p \
         JOIN turn_nodes n ON n.node_pk = p.node_pk WHERE n.id = 'b'",
    )
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(a_payload, 1);
    assert_eq!(b_payload, 0);

    let fixed_envelope = json!({
        "data": {"slot": null},
        "slots": [["/slot", 0]],
        "contents": [key]
    });
    let fixed_payload = crate::storage_codec::encode(serde_json::to_vec(&fixed_envelope)?.into())?;
    sqlx::query("UPDATE turn_chain_nodes SET payload = $1 WHERE id = 'b'")
        .bind(fixed_payload)
        .execute(&mut connection)
        .await?;
    normalize_sqlite(&mut connection).await?;
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    assert_eq!(
        restore_one(&mut connection, "b", "a").await?,
        json!({"slot": true})
    );
    assert_eq!(restore_one(&mut connection, "a", "a").await?, good);
    Ok(())
}

#[tokio::test]
async fn shared_profiles_survive_node_batch_boundaries() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    // Padding keeps the outer and nested profile bodies above the legacy
    // 256-byte threshold even after their inner units are externalized, so
    // this exercises the shared externalized-profile path.
    let profile = json!({
        "system": "shared",
        "instructions": "i".repeat(300),
        "items": [{"text": "s".repeat(300)}],
        "padding": "p".repeat(300),
        "nested": {
            "__open_responses_effective_request": {
                "system": "deep",
                "items": [{"text": "d".repeat(300)}],
                "padding": "d".repeat(300)
            }
        }
    });
    let first = json!({
        "items": [{"text": "a".repeat(300)}],
        "nested": {"__open_responses_response_profile": profile.clone()}
    });
    let second = json!({
        "small": "keep",
        "nested": {"__open_responses_response_profile": profile.clone()}
    });
    for (id, payload, expires_at) in [
        ("n0", first.clone(), 10i64),
        ("n1", second.clone(), 11i64),
        ("n2", first.clone(), 12i64),
        ("n3", second.clone(), 13i64),
    ] {
        insert_legacy_node(
            &mut connection,
            id,
            "a",
            "response",
            None,
            payload,
            expires_at,
        )
        .await?;
    }
    run_sqlite(
        &mut connection,
        Limits {
            node_batch: 2,
            content_batch: 1,
            content_bytes: 8 * 1024 * 1024,
            budget: None,
        },
    )
    .await?;
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    for (id, payload) in [
        ("n0", &first),
        ("n1", &second),
        ("n2", &first),
        ("n3", &second),
    ] {
        assert_eq!(restore_one(&mut connection, id, "a").await?, *payload);
    }

    let body_pointer = "/nested/__open_responses_response_profile";
    let nested_pointer =
        "/nested/__open_responses_response_profile/nested/__open_responses_effective_request";
    let mut body_ids = Vec::new();
    for id in ["n0", "n1", "n2", "n3"] {
        let stored = descriptor(&mut connection, id).await?;
        let slots = stored["slots"].as_array().expect("slots");
        let body = slots
            .iter()
            .position(|slot| slot[0].as_str() == Some(body_pointer))
            .expect("profile body slot");
        let nested = slots
            .iter()
            .position(|slot| slot[0].as_str() == Some(nested_pointer))
            .expect("nested profile slot");
        assert!(
            body < nested,
            "profile body slot must precede its nested slots"
        );
        let ordinal = i64::try_from(slot_ordinal(&stored, body_pointer))?;
        let content_id: i64 = sqlx::query_scalar(
            "SELECT nc.content_id FROM turn_node_contents nc \
             JOIN turn_nodes n ON n.node_pk = nc.node_pk \
             WHERE n.id = $1 AND nc.ordinal = $2",
        )
        .bind(id)
        .bind(ordinal)
        .fetch_one(&mut connection)
        .await?;
        body_ids.push(content_id);
    }
    assert!(
        body_ids.iter().all(|id| *id == body_ids[0]),
        "shared profile body must reuse one content row"
    );
    Ok(())
}

#[tokio::test]
async fn rejects_missing_legacy_node_content_reference() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let payload = json!({"items": [{"text": "x".repeat(300)}]});
    for (id, expires_at) in [("n0", 10i64), ("n1", 11i64)] {
        insert_legacy_node(
            &mut connection,
            id,
            "a",
            "response",
            None,
            payload.clone(),
            expires_at,
        )
        .await?;
    }
    let content_id: i64 = sqlx::query_scalar(
        "SELECT content_id FROM turn_chain_node_contents \
         WHERE node_id = 'n0' AND principal = 'a'",
    )
    .fetch_one(&mut connection)
    .await?;
    sqlx::query(
        "DELETE FROM turn_chain_node_contents \
         WHERE node_id = 'n0' AND principal = 'a' AND content_id = $1",
    )
    .bind(content_id)
    .execute(&mut connection)
    .await?;

    let result = normalize_sqlite(&mut connection).await;
    ensure!(
        result.is_err(),
        "missing legacy node content reference was accepted"
    );
    assert_eq!(
        work_state(&mut connection).await?,
        (2, None, None),
        "missing legacy reference must not advance the node cursor"
    );
    Ok(())
}

#[tokio::test]
async fn rejects_extra_legacy_node_content_reference() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let payload = json!({"items": [{"text": "x".repeat(300)}]});
    insert_legacy_node(&mut connection, "n", "a", "response", None, payload, 10).await?;
    let extra = b"{\"extra\":true}".to_vec();
    let extra_id =
        insert_legacy_content(&mut connection, "a", &format2::content_key(&extra), &extra).await?;
    sqlx::query(
        "INSERT INTO turn_chain_node_contents (node_id, principal, content_id) \
         VALUES ('n', 'a', $1)",
    )
    .bind(extra_id)
    .execute(&mut connection)
    .await?;

    let result = normalize_sqlite(&mut connection).await;
    ensure!(
        result.is_err(),
        "extra same-principal legacy node content reference was accepted"
    );
    assert_eq!(
        work_state(&mut connection).await?,
        (2, None, None),
        "extra legacy reference must not advance the node cursor"
    );
    Ok(())
}

#[tokio::test]
async fn profile_budget_uses_decoded_hint_and_preserves_unselected_suffix() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let mut payloads = Vec::new();
    for index in 0..2 {
        let profile = json!({
            "padding": "p".repeat(16_384),
            "tag": index,
        });
        let payload = json!({
            "nested": {"__open_responses_response_profile": profile}
        });
        insert_legacy_node(
            &mut connection,
            &format!("n{index}"),
            "a",
            "response",
            None,
            payload.clone(),
            10 + i64::from(index),
        )
        .await?;
        payloads.push(payload);
    }
    // The profile rows compress far below 4096 bytes, but their decoded hint
    // is above it. Only the first node (an allowed oversize node) may advance.
    let limits = Limits {
        node_batch: 64,
        content_batch: 512,
        content_bytes: 4096,
        budget: Some(1),
    };
    let mut calls = 0usize;
    loop {
        let (stage, node_cursor, _) = work_state(&mut connection).await?;
        if stage == 2 && node_cursor.is_some() {
            break;
        }
        ensure!(
            stage < 3,
            "descriptor budget advanced past unselected suffix"
        );
        run_sqlite(&mut connection, limits).await?;
        calls += 1;
        ensure!(calls < 100, "normalization made no progress");
    }
    assert_eq!(
        work_state(&mut connection).await?,
        (2, Some("n0".into()), None),
        "profile raw hint must shrink the prefix to one node"
    );
    normalize_sqlite(&mut connection).await?;
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    for (index, payload) in payloads.into_iter().enumerate() {
        assert_eq!(
            restore_one(&mut connection, &format!("n{index}"), "a").await?,
            payload
        );
    }
    Ok(())
}

#[tokio::test]
async fn shared_profiles_count_once_in_descriptor_budget() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let profile = json!({
        "padding": "p".repeat(16_384),
        "tag": "shared",
    });
    let payload = json!({
        "nested": {"__open_responses_response_profile": profile}
    });
    for (id, expires_at) in [("n0", 10i64), ("n1", 11i64)] {
        insert_legacy_node(
            &mut connection,
            id,
            "a",
            "response",
            None,
            payload.clone(),
            expires_at,
        )
        .await?;
    }
    // One decoded profile fits; charging it once per node would not.
    let limits = Limits {
        node_batch: 2,
        content_batch: 512,
        content_bytes: 24_000,
        budget: Some(1),
    };
    let mut calls = 0usize;
    loop {
        let (stage, node_cursor, _) = work_state(&mut connection).await?;
        if stage == 2 && node_cursor.is_some() {
            break;
        }
        ensure!(stage < 3, "descriptor budget stopped before descriptors");
        run_sqlite(&mut connection, limits).await?;
        calls += 1;
        ensure!(calls < 100, "normalization made no progress");
    }
    assert_eq!(
        work_state(&mut connection).await?,
        (2, Some("n1".into()), None),
        "shared profile must be charged once per unique content"
    );
    normalize_sqlite(&mut connection).await?;
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    for id in ["n0", "n1"] {
        assert_eq!(restore_one(&mut connection, id, "a").await?, payload);
    }
    Ok(())
}

#[tokio::test]
async fn prunes_scopes_created_only_for_orphan_contents() -> anyhow::Result<()> {
    let mut connection = fixture().await?;
    let payload = json!({"items": [{"text": "x".repeat(300)}]});
    insert_legacy_node(
        &mut connection,
        "n",
        "a",
        "response",
        None,
        payload.clone(),
        10,
    )
    .await?;
    // More orphan rows than the stage-3 batch limit: the first batch must keep
    // the scope because a later batch still owns contents in it.
    for index in 0..501 {
        let raw = format!("{{\"orphan\":{index}}}").into_bytes();
        let key = format2::content_key(&raw);
        insert_legacy_content(&mut connection, "orphan", &key, &raw).await?;
    }
    let node_orphan = b"{\"orphan\":\"node-principal\"}".to_vec();
    insert_legacy_content(
        &mut connection,
        "a",
        &format2::content_key(&node_orphan),
        &node_orphan,
    )
    .await?;

    run_sqlite(
        &mut connection,
        Limits {
            node_batch: 64,
            content_batch: 600,
            content_bytes: 8 * 1024 * 1024,
            budget: None,
        },
    )
    .await?;
    assert_eq!(work_state(&mut connection).await?, (4, None, None));
    let orphan_scope: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_scopes WHERE principal = 'orphan'",
    )
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(
        orphan_scope, 0,
        "scope created only for orphan contents must be pruned"
    );
    let node_scope: i64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS BIGINT) FROM turn_scopes WHERE principal = 'a'",
    )
    .fetch_one(&mut connection)
    .await?;
    assert_eq!(node_scope, 1, "scope owning a node must survive");
    assert_eq!(restore_one(&mut connection, "n", "a").await?, payload);
    Ok(())
}
