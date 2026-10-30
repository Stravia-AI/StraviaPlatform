use super::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn content_of(raw: Value) -> super::content::ContentDigest {
    super::content::content_key(&serde_json::to_vec(&raw).expect("serialize expected content"))
}

fn slot_map(slots: &[(String, usize)]) -> HashMap<&str, usize> {
    slots
        .iter()
        .map(|(pointer, ordinal)| (pointer.as_str(), *ordinal))
        .collect()
}

fn descriptor(payload: &[u8]) -> Value {
    let raw = crate::storage_codec::decode(payload).expect("descriptor codec");
    serde_json::from_slice(&raw).expect("descriptor JSON")
}

fn contract_payload() -> Value {
    let item = json!({
        "role": "user",
        "content": "short",
        "meta": {"a/b~c": null, "unknown": [true, {"deep": "x"}]}
    });
    let huge = json!({
        "role": "tool",
        "content": "界\n\"e\u{301}".repeat(200_000),
        "id": "opaque-item-id"
    });
    let big_number: Value =
        serde_json::from_str("123456789012345678901234567890.123456789").expect("big number");
    json!({
        "client_delta": {"system": "", "messages": [item.clone(), null, item.clone()]},
        "client_output": [{}, [], ""],
        "effective_history_mutation": {"type": "replace", "items": [item.clone(), huge]},
        "effective_system": {"n": big_number.clone()},
        "effective_request": {
            "instructions": "nested \"quoted\"\n",
            "tools": {"kinds": []},
            "__open_responses_effective_request": {
                "system": "profile system",
                "tools": ["t"],
                "extra": {"items": [item.clone()]}
            }
        },
        "vendor/a~b": {
            "__open_responses_response_profile": {
                "system": "profile system",
                "tools": ["t"],
                "extra": {"messages": [item]}
            }
        },
        "unknown": {"arbitrary": big_number, "null": null, "text": "opaque"}
    })
}

#[test]
fn extraction_externalizes_empty_short_and_large_units_without_duplicate_bodies()
-> anyhow::Result<()> {
    let big = "x".repeat(1_000_000);
    let payload = json!({
        "system": "",
        "tools": [],
        "messages": ["", "a", {}, [], null, {"k": "v"}, big],
        "unknown": {"big": "inline", "null": null, "number": 12345678901234567890u64}
    });
    let extracted = super::content::extract(payload.clone())?;
    let slots = slot_map(&extracted.slots);
    assert_eq!(slots.len(), 8);
    assert!(
        !slots.contains_key("/messages/4"),
        "null is presence, not an externalized unit"
    );
    assert_eq!(slots["/system"], slots["/messages/0"]);
    assert_eq!(slots["/tools"], slots["/messages/3"]);
    // Empty string, short string, empty object, empty array, item and the
    // large string are the only distinct bodies.
    assert_eq!(extracted.contents.len(), 6);
    let digest = |pointer: &str| -> super::content::ContentDigest {
        extracted.contents[slots[pointer]].digest()
    };
    assert_eq!(digest("/messages/0"), content_of(json!("")));
    assert_eq!(digest("/messages/1"), content_of(json!("a")));
    assert_eq!(digest("/messages/2"), content_of(json!({})));
    assert_eq!(digest("/messages/3"), content_of(json!([])));
    assert_eq!(digest("/messages/5"), content_of(json!({"k": "v"})));
    assert_eq!(
        digest("/messages/6"),
        content_of(json!("x".repeat(1_000_000)))
    );
    assert_eq!(digest("/system"), digest("/messages/0"));
    assert_eq!(digest("/tools"), digest("/messages/3"));
    assert!(extracted.data["system"].is_null());
    assert!(extracted.data["tools"].is_null());
    for index in [0, 1, 2, 3, 5, 6] {
        assert!(extracted.data["messages"][index].is_null());
    }
    assert_eq!(
        extracted.data["unknown"]["number"].to_string(),
        "12345678901234567890"
    );
    assert_eq!(extracted.data["unknown"]["big"], json!("inline"));
    Ok(())
}

#[test]
fn profile_slots_restore_parent_before_nested_and_escape_pointer_tokens() -> anyhow::Result<()> {
    let payload = json!({
        "vendor/a~b": {
            "__open_responses_response_profile": {
                "system": "s",
                "instructions": "i",
                "inner": {"messages": [{"part": "opaque"}]}
            }
        }
    });
    let extracted = super::content::extract(payload)?;
    let parent_pointer = "/vendor~1a~0b/__open_responses_response_profile";
    assert_eq!(extracted.slots[0].0, parent_pointer);
    assert_eq!(extracted.slots.len(), 4);
    assert_eq!(
        extracted
            .slots
            .iter()
            .filter(|(pointer, _)| pointer == parent_pointer)
            .count(),
        1
    );
    for (pointer, _) in extracted.slots.iter().skip(1) {
        assert!(pointer.starts_with(parent_pointer));
    }
    assert_eq!(extracted.contents.len(), 4);
    let profile_pointer = &extracted.slots[0].0;
    let profile_content = &extracted.contents[extracted.slots[0].1];
    // The parent body contains the nested units as null; nested pointers
    // therefore resolve only after the parent has been spliced. Build the
    // expected body from the same value shape so key order is irrelevant.
    let mut expected = json!({
        "system": "s",
        "instructions": "i",
        "inner": {"messages": [{"part": "opaque"}]},
    });
    expected["inner"]["messages"][0] = Value::Null;
    expected["instructions"] = Value::Null;
    expected["system"] = Value::Null;
    assert_eq!(profile_content.digest(), content_of(expected));
    assert!(extracted.data["vendor~1a~0b"]["__open_responses_response_profile"].is_null());
    assert_eq!(profile_pointer.as_str(), parent_pointer);
    Ok(())
}

#[test]
fn descriptor_contains_only_data_and_slots() -> anyhow::Result<()> {
    let encoded = super::content::encode(json!({
        "messages": ["one", "two", "one"],
        "system": "instructions"
    }))?;
    let descriptor = descriptor(&encoded.payload);
    let object = descriptor.as_object().expect("descriptor object");
    assert_eq!(object.len(), 2);
    assert!(object.contains_key("data"));
    assert!(object.contains_key("slots"));
    let slots: Vec<(String, usize)> =
        serde_json::from_value(descriptor["slots"].clone()).expect("descriptor slot pairs");
    let slots = slot_map(&slots);
    assert_eq!(slots.len(), 4);
    assert_eq!(slots["/messages/0"], slots["/messages/2"]);
    assert_eq!(encoded.contents.len(), 3);
    let digest = |pointer: &str| -> super::content::ContentDigest {
        encoded.contents[slots[pointer]].digest()
    };
    assert_eq!(digest("/messages/0"), content_of(json!("one")));
    assert_eq!(digest("/messages/1"), content_of(json!("two")));
    assert_eq!(digest("/system"), content_of(json!("instructions")));
    assert_eq!(digest("/messages/0"), digest("/messages/2"));
    Ok(())
}

#[test]
fn content_identity_preserves_raw_bytes_and_empty_value_types() {
    // Identity is the raw serialized bytes, not a canonical projection.
    assert_ne!(
        super::content::content_key(br#"{"b":1,"a":2}"#),
        super::content::content_key(br#"{"a":2,"b":1}"#)
    );
    let empty_string = content_of(json!(""));
    let empty_object = content_of(json!({}));
    let empty_array = content_of(json!([]));
    assert_ne!(empty_string, empty_object);
    assert_ne!(empty_object, empty_array);
    assert_ne!(empty_string, empty_array);
}

async fn sqlite_scope(connection: &mut sqlx::SqliteConnection<'_>, principal: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO turn_scopes (principal) VALUES (?) RETURNING id")
        .bind(principal)
        .fetch_one(&mut *connection)
        .await
        .unwrap()
}

async fn sqlite_node(
    connection: &mut sqlx::SqliteConnection<'_>,
    scope_id: i64,
    id: &TurnNodeId,
    kind: TurnNodeKind,
    parent_pk: Option<i64>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO turn_nodes (id, scope_id, kind, parent_pk, payload_version, created_at) \
         VALUES (?, ?, ?, ?, 6, ?) RETURNING node_pk",
    )
    .bind(id.as_str())
    .bind(scope_id)
    .bind(kind.as_str())
    .bind(parent_pk)
    .bind(chrono::Utc::now().timestamp_millis())
    .fetch_one(&mut *connection)
    .await
    .unwrap()
}

async fn sqlite_put(
    connection: &mut sqlx::SqliteConnection<'_>,
    node_pk: i64,
    scope_id: i64,
    encoded: &mut super::content::Encoded,
) {
    let mut transaction = connection.begin().await.unwrap();
    super::content::put_sqlite(&mut transaction, node_pk, scope_id, &mut encoded.contents)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
}

async fn sqlite_scalar(connection: &mut sqlx::SqliteConnection<'_>, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&mut *connection)
        .await
        .unwrap()
}

macro_rules! restore_fixture {
    ($stored:ident, $restore:ident, $restore_content:ident, $backend:ident, $conn:ty) => {
        async fn $stored(
            connection: &mut $conn,
            principal: &str,
            node_pk: i64,
            id: TurnNodeId,
            kind: TurnNodeKind,
            payload: Vec<u8>,
        ) -> anyhow::Result<super::content::StoredNode> {
            let (scope_id, references): (i64, sqlx::types::Json<Vec<[i64; 3]>>) =
                sqlx::query_as(concat!(
                    "SELECT n.scope_id,",
                    super::content::reference_projection!($backend),
                    " FROM turn_nodes n JOIN turn_scopes s ON s.id=n.scope_id \
                     WHERE n.node_pk=$1 AND s.principal=$2 AND n.id=$3 AND n.kind=$4"
                ))
                .bind(node_pk)
                .bind(principal)
                .bind(id.as_str())
                .bind(kind.as_str())
                .fetch_one(&mut *connection)
                .await?;
            Ok(super::content::StoredNode {
                node_pk,
                scope_id,
                references: references.0,
                id,
                kind,
                parent_id: None,
                payload_version: 6,
                payload,
            })
        }

        async fn $restore(
            connection: &mut $conn,
            principal: &str,
            node_pk: i64,
            id: TurnNodeId,
            kind: TurnNodeKind,
            payload: Vec<u8>,
        ) -> anyhow::Result<Vec<TurnNode>> {
            let node = $stored(connection, principal, node_pk, id, kind, payload).await?;
            super::content::$restore_content(connection, vec![node]).await
        }
    };
}
restore_fixture!(
    sqlite_stored,
    sqlite_restore,
    restore_sqlite,
    sqlite,
    sqlx::SqliteConnection<'_>
);
restore_fixture!(
    postgres_stored,
    postgres_restore,
    restore_postgres,
    postgres,
    sqlx::PgConnection<'_>
);

#[tokio::test]
async fn sqlite_content_contract_round_trips_and_isolates_principals() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let owner = Principal::new("normalized-owner");
    let other = Principal::new("normalized-other");
    let owner_scope = sqlite_scope(&mut connection, &owner.continuation_key()).await;
    let other_scope = sqlite_scope(&mut connection, &other.continuation_key()).await;
    let root = TurnNodeId::new("normalized-root");
    let agent = TurnNodeId::new("normalized-agent");
    let foreign = TurnNodeId::new("normalized-foreign");
    let root_pk = sqlite_node(
        &mut connection,
        owner_scope,
        &root,
        TurnNodeKind::Response,
        None,
    )
    .await;
    let agent_pk = sqlite_node(
        &mut connection,
        owner_scope,
        &agent,
        TurnNodeKind::Agent,
        None,
    )
    .await;
    let foreign_pk = sqlite_node(
        &mut connection,
        other_scope,
        &foreign,
        TurnNodeKind::Response,
        None,
    )
    .await;

    let original = contract_payload();
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let root_payload = encoded.payload.clone();
    let distinct = encoded.contents.len();
    assert!(distinct > 0);
    sqlite_put(&mut connection, root_pk, owner_scope, &mut encoded).await;
    assert_eq!(
        sqlite_scalar(&mut connection, "SELECT COUNT(*) FROM turn_contents").await,
        i64::try_from(distinct).unwrap()
    );
    assert_eq!(
        sqlite_scalar(
            &mut connection,
            "SELECT COUNT(*) FROM turn_node_contents WHERE node_pk = (SELECT node_pk FROM turn_nodes WHERE id = 'normalized-root')"
        )
        .await,
        i64::try_from(distinct).unwrap()
    );
    let descriptor = descriptor(&root_payload);
    let slots = descriptor["slots"].as_array().unwrap();
    assert!(
        slots.len() > distinct,
        "repeated occurrences reuse ordinals"
    );
    // Digests are stored as raw 32-byte values, never textual keys.
    let digest_rows: Vec<(Vec<u8>,)> =
        sqlx::query_as("SELECT digest FROM turn_contents ORDER BY id")
            .fetch_all(&mut *connection)
            .await
            .unwrap();
    assert!(digest_rows.iter().all(|(digest,)| digest.len() == 32));

    let restored = sqlite_restore(
        &mut connection,
        &owner.continuation_key(),
        root_pk,
        root.clone(),
        TurnNodeKind::Response,
        root_payload.clone(),
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
    assert_eq!(
        restored[0].payload["unknown"]["arbitrary"].to_string(),
        "123456789012345678901234567890.123456789"
    );

    // The same payload under another kind in the same scope deduplicates to the
    // existing content rows; another Principal owns an independent copy.
    let mut same_scope = super::content::encode(original.clone()).unwrap();
    sqlite_put(&mut connection, agent_pk, owner_scope, &mut same_scope).await;
    assert_eq!(
        sqlite_scalar(&mut connection, "SELECT COUNT(*) FROM turn_contents").await,
        i64::try_from(distinct).unwrap()
    );
    let mut foreign_encoded = super::content::encode(original.clone()).unwrap();
    sqlite_put(
        &mut connection,
        foreign_pk,
        other_scope,
        &mut foreign_encoded,
    )
    .await;
    assert_eq!(
        sqlite_scalar(&mut connection, "SELECT COUNT(*) FROM turn_contents").await,
        i64::try_from(distinct * 2).unwrap()
    );
    let foreign_payload = foreign_encoded.payload.clone();
    assert_ne!(root_pk, foreign_pk);

    // Existence probes are scope-scoped.
    let probe = super::content::encode(original.clone()).unwrap();
    let hint = super::content::lookup_sqlite(
        &mut connection,
        super::content::LookupScope::Principal(&owner.continuation_key()),
        &probe.contents,
        false,
    )
    .await
    .unwrap();
    assert!(hint.iter().all(Option::is_some));
    let foreign_hint = super::content::lookup_sqlite(
        &mut connection,
        super::content::LookupScope::Principal(&other.continuation_key()),
        &probe.contents,
        false,
    )
    .await
    .unwrap();
    assert!(foreign_hint.iter().all(Option::is_some));
    assert_ne!(hint, foreign_hint);

    // 归属检查在读取节点及其 ordinal 映射时完成，不能向恢复器传入别人的节点。
    let error = sqlite_restore(
        &mut connection,
        &owner.continuation_key(),
        foreign_pk,
        foreign.clone(),
        TurnNodeKind::Response,
        foreign_payload.clone(),
    )
    .await;
    assert!(error.is_err(), "cross-Principal restore must fail");
    let error = sqlite_restore(
        &mut connection,
        &other.continuation_key(),
        root_pk,
        root.clone(),
        TurnNodeKind::Response,
        root_payload.clone(),
    )
    .await;
    assert!(
        error.is_err(),
        "owner node must not resolve for another Principal"
    );
}

#[tokio::test]
async fn sqlite_prepare_missing_inserts_prepared_rows_once() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("prepare-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let id = TurnNodeId::new("prepare-node");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let original = json!({"messages": ["prepared", {"opaque": true}], "system": ""});
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let ids = vec![None; encoded.contents.len()];
    super::content::prepare_missing(&mut encoded.contents, &ids)
        .await
        .unwrap();
    let mut transaction = connection.begin().await.unwrap();
    let inserted = super::content::intern_sqlite(&mut transaction, scope, &mut encoded.contents)
        .await
        .unwrap();
    assert_eq!(inserted.len(), encoded.contents.len());
    assert!(inserted.iter().all(|id| *id > 0));
    super::content::link_sqlite(&mut transaction, node_pk, scope, &inserted)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert_eq!(
        sqlite_scalar(&mut connection, "SELECT COUNT(*) FROM turn_node_contents").await,
        3
    );
    let restored = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        encoded.payload.clone(),
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
}

#[tokio::test]
async fn sqlite_stale_hint_rebuilds_bodies_inside_the_write_transaction() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("stale-hint-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let original = contract_payload();
    let first = TurnNodeId::new("stale-hint-first");
    let first_pk = sqlite_node(&mut connection, scope, &first, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, first_pk, scope, &mut encoded).await;

    // Hints are collected before the transaction; GC removes every content row
    // and reference before the transaction starts.
    let mut recovery = super::content::encode(original.clone()).unwrap();
    let hints = super::content::lookup_sqlite(
        &mut connection,
        super::content::LookupScope::Id(scope),
        &recovery.contents,
        false,
    )
    .await
    .unwrap();
    assert!(hints.iter().all(Option::is_some));
    sqlx::query("DELETE FROM turn_node_contents")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("DELETE FROM turn_contents")
        .execute(&mut *connection)
        .await
        .unwrap();
    super::content::prepare_missing(&mut recovery.contents, &hints)
        .await
        .unwrap();

    let second = TurnNodeId::new("stale-hint-second");
    let second_pk = sqlite_node(
        &mut connection,
        scope,
        &second,
        TurnNodeKind::Response,
        None,
    )
    .await;
    let mut transaction = connection.begin().await.unwrap();
    super::content::put_sqlite(&mut transaction, second_pk, scope, &mut recovery.contents)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    assert!(sqlite_scalar(&mut connection, "SELECT COUNT(*) FROM turn_contents").await > 0);
    let restored = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        second_pk,
        second,
        TurnNodeKind::Response,
        payload,
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
}

#[tokio::test]
async fn sqlite_empty_and_null_only_payloads_use_no_content_rows() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("empty-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    for value in [json!({}), json!({"unknown": null, "missing": null})] {
        let id = TurnNodeId::new(format!("empty-{}", uuid::Uuid::new_v4().simple()));
        let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
        let mut encoded = super::content::encode(value.clone()).unwrap();
        assert!(encoded.contents.is_empty());
        sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
        let restored = sqlite_restore(
            &mut connection,
            &principal.continuation_key(),
            node_pk,
            id,
            TurnNodeKind::Response,
            encoded.payload,
        )
        .await
        .unwrap();
        assert_eq!(restored[0].payload, value);
    }
    assert_eq!(
        sqlite_scalar(&mut connection, "SELECT COUNT(*) FROM turn_contents").await,
        0
    );
}

#[tokio::test]
async fn sqlite_corruption_and_bad_ordinal_maps_are_rejected() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("corrupt-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let original = json!({"messages": ["first", "second", "third"]});

    let id = TurnNodeId::new("corrupt-reference-scope");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let other_scope = sqlite_scope(&mut connection, "another-scope").await;
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("UPDATE turn_node_contents SET scope_id = ? WHERE node_pk = ?")
        .bind(other_scope)
        .bind(node_pk)
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *connection)
        .await
        .unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await;
    assert!(
        error.is_err(),
        "a reference with wrong scope must fail even when its content belongs to the node"
    );

    let id = TurnNodeId::new("corrupt-missing");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let victim: i64 = sqlx::query_scalar(
        "SELECT content_id FROM turn_node_contents WHERE node_pk = ? ORDER BY ordinal LIMIT 1",
    )
    .bind(node_pk)
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("DELETE FROM turn_contents WHERE id = ?")
        .bind(victim)
        .execute(&mut *connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *connection)
        .await
        .unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await;
    assert!(error.is_err(), "missing content row must fail restore");

    let id = TurnNodeId::new("corrupt-digest");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let tampered = crate::storage_codec::encode(std::borrow::Cow::Owned(
        serde_json::to_vec(&json!("different")).unwrap(),
    ))
    .unwrap();
    sqlx::query(
        "UPDATE turn_contents SET content = ? WHERE id = (SELECT content_id FROM turn_node_contents WHERE node_pk = ? ORDER BY ordinal LIMIT 1)",
    )
    .bind(tampered)
    .bind(node_pk)
    .execute(&mut *connection)
    .await
    .unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await;
    assert!(
        error.is_err(),
        "tampered content body must fail digest verification"
    );

    let id = TurnNodeId::new("corrupt-ordinal");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    sqlx::query("UPDATE turn_node_contents SET ordinal = 9 WHERE node_pk = ? AND ordinal = 1")
        .bind(node_pk)
        .execute(&mut *connection)
        .await
        .unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await;
    assert!(
        error.is_err(),
        "non-contiguous ordinal map must fail restore"
    );

    let id = TurnNodeId::new("corrupt-missing-map");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    sqlx::query("DELETE FROM turn_node_contents WHERE node_pk = ?")
        .bind(node_pk)
        .execute(&mut *connection)
        .await
        .unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await;
    assert!(error.is_err(), "missing ordinal map must fail restore");
}

#[tokio::test]
async fn sqlite_descriptor_corruption_is_rejected() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("descriptor-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let original = json!({"messages": ["alpha", "beta"]});
    let id = TurnNodeId::new("descriptor-node");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let parsed = descriptor(&payload);
    let slots: Vec<(String, usize)> = serde_json::from_value(parsed["slots"].clone()).unwrap();

    let mut data = parsed["data"].clone();
    *data.pointer_mut(&slots[0].0).unwrap() = json!("not null");
    let overwrite = super::content::encode_descriptor(&data, &slots).unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id.clone(),
        TurnNodeKind::Response,
        overwrite,
    )
    .await;
    assert!(error.is_err(), "slot must overwrite only null data");

    let mut out_of_range = slots.clone();
    out_of_range[0].1 = 99;
    let bad = super::content::encode_descriptor(&parsed["data"], &out_of_range).unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id.clone(),
        TurnNodeKind::Response,
        bad,
    )
    .await;
    assert!(error.is_err(), "referenced ordinal must exist");

    let unreferenced = super::content::encode_descriptor(&parsed["data"], &[]).unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id.clone(),
        TurnNodeKind::Response,
        unreferenced,
    )
    .await;
    assert!(error.is_err(), "every mapping ordinal must be referenced");

    // A legacy body with a contents array is not a second accepted format.
    let legacy = json!({
        "data": parsed["data"].clone(),
        "slots": parsed["slots"].clone(),
        "contents": []
    });
    let legacy = crate::storage_codec::encode(std::borrow::Cow::Owned(
        serde_json::to_vec(&legacy).unwrap(),
    ))
    .unwrap();
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        legacy,
    )
    .await;
    assert!(error.is_err(), "old descriptor shape must not restore");
}

#[tokio::test]
async fn sqlite_request_context_cache_uses_numeric_content_ids_and_budget() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("context-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let original = json!({"messages": ["cached value", "cached value"]});
    let id = TurnNodeId::new("context-node");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    assert_eq!(encoded.contents.len(), 1);
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let (content_id, stored): (i64, Vec<u8>) = sqlx::query_as(
        "SELECT c.id, c.content FROM turn_node_contents n JOIN turn_contents c ON c.id = n.content_id \
         WHERE n.node_pk = ? AND n.ordinal = 0",
    )
    .bind(node_pk)
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    let raw = crate::storage_codec::decode(&stored).unwrap();
    let cached: Value = serde_json::from_slice(&raw).unwrap();

    // A zero budget refuses to stage optional content but still restores.
    let mut budget = TurnReadContext::with_budget(&principal, TurnNodeKind::Response, 0);
    let references = sqlite_stored(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id.clone(),
        TurnNodeKind::Response,
        payload.clone(),
    )
    .await
    .unwrap()
    .references;
    let stored_node = || super::content::StoredNode {
        node_pk,
        scope_id: scope,
        references: references.clone(),
        id: id.clone(),
        kind: TurnNodeKind::Response,
        parent_id: None,
        payload_version: 6,
        payload: payload.clone(),
    };
    let restored = super::content::restore_sqlite_context(
        &mut connection,
        vec![stored_node()],
        Some(&mut budget),
        |node, _| Ok(node),
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
    assert!(!budget.admission_open());
    assert!(budget.cached_content(content_id).is_none());

    // Admitted numeric content ids serve the restore even when the stored row
    // is unreadable, proving the cache key path and one-fetch-per-id rule.
    let mut context = TurnReadContext::with_budget(
        &principal,
        TurnNodeKind::Response,
        TurnReadContext::BYTE_LIMIT,
    );
    context.stage_content(content_id, Arc::new(cached), 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    let chain = SharedMaterializedTurnChain {
        expires_at: deadline,
        nodes: vec![SharedTurnNode {
            id: id.clone(),
            kind: TurnNodeKind::Response,
            parent_id: None,
            payload_version: 6,
            expires_at: deadline,
            raw: None,
            decoded: None,
        }],
    };
    let decoded: Arc<dyn std::any::Any + Send + Sync> = Arc::new("decoded node");
    context.admit(&chain, vec![(decoded, 0)]);
    assert!(context.cached_content(content_id).is_some());
    let tampered = crate::storage_codec::encode(std::borrow::Cow::Owned(b"{}".to_vec())).unwrap();
    sqlx::query("UPDATE turn_contents SET content = ? WHERE id = ?")
        .bind(tampered)
        .bind(content_id)
        .execute(&mut *connection)
        .await
        .unwrap();
    let restored = super::content::restore_sqlite_context(
        &mut connection,
        vec![stored_node()],
        Some(&mut context),
        |node, _| Ok(node),
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
    assert!(context.cached_content(content_id).is_some());
    let error = sqlite_restore(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await;
    assert!(error.is_err(), "a tampered row fails without a cache hit");
}

#[tokio::test]
async fn sqlite_restore_uses_ordinals_not_projection_row_order() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("ordinal-order-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let original = json!({"messages": ["first", {"opaque": []}, "last"], "system": "config"});
    let id = TurnNodeId::new("ordinal-order-node");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let mut node = sqlite_stored(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await
    .unwrap();
    node.references.reverse();
    let restored = super::content::restore_sqlite(&mut connection, vec![node])
        .await
        .unwrap();
    assert_eq!(restored[0].payload, original);
}

#[tokio::test]
async fn sqlite_request_context_accounts_for_many_small_contents() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("small-content-budget-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let original = json!({"messages": (0..64).map(|index| index.to_string()).collect::<Vec<_>>()});
    let id = TurnNodeId::new("small-content-budget-node");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let mut context = TurnReadContext::with_budget(&principal, TurnNodeKind::Response, 4096);
    let node = sqlite_stored(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await
    .unwrap();
    let restored = super::content::restore_sqlite_context(
        &mut connection,
        vec![node],
        Some(&mut context),
        |node, _| Ok(node),
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
    assert!(
        !context.admission_open(),
        "small contents must include their cache entry and shared allocation overhead"
    );
}

#[tokio::test]
async fn sqlite_request_context_does_not_admit_oversized_json_numbers() {
    let pool = crate::test_support::migrated_sqlite_pool().await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    let principal = Principal::new("number-budget-owner");
    let scope = sqlite_scope(&mut connection, &principal.continuation_key()).await;
    let number: Value = serde_json::from_str(&"1".repeat(300 * 1024)).unwrap();
    let original = json!({"messages": [{"number": number}]});
    let id = TurnNodeId::new("number-budget-node");
    let node_pk = sqlite_node(&mut connection, scope, &id, TurnNodeKind::Response, None).await;
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    sqlite_put(&mut connection, node_pk, scope, &mut encoded).await;
    let mut context = TurnReadContext::with_budget(&principal, TurnNodeKind::Response, 256 * 1024);
    let node = sqlite_stored(
        &mut connection,
        &principal.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await
    .unwrap();
    let restored = super::content::restore_sqlite_context(
        &mut connection,
        vec![node],
        Some(&mut context),
        |node, _| Ok(node),
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
    assert!(
        !context.admission_open(),
        "arbitrary-precision number storage must count toward the request cache budget"
    );
}

async fn postgres_content_contract(pool: &PgPool) {
    let mut connection = pool.acquire().await.unwrap();
    let owner = Principal::new("pg-normalized-owner");
    let other = Principal::new("pg-normalized-other");
    let owner_scope: i64 =
        sqlx::query_scalar("INSERT INTO turn_scopes (principal) VALUES ($1) RETURNING id")
            .bind(owner.continuation_key())
            .fetch_one(&mut *connection)
            .await
            .unwrap();
    let other_scope: i64 =
        sqlx::query_scalar("INSERT INTO turn_scopes (principal) VALUES ($1) RETURNING id")
            .bind(other.continuation_key())
            .fetch_one(&mut *connection)
            .await
            .unwrap();
    let id = TurnNodeId::new("pg-normalized-node");
    let node_pk: i64 = sqlx::query_scalar(
        "INSERT INTO turn_nodes (id, scope_id, kind, parent_pk, payload_version, created_at) \
         VALUES ($1, $2, $3, NULL, 6, $4) RETURNING node_pk",
    )
    .bind(id.as_str())
    .bind(owner_scope)
    .bind(TurnNodeKind::Response.as_str())
    .bind(chrono::Utc::now().timestamp_millis())
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    let foreign_id = TurnNodeId::new("pg-normalized-foreign");
    let foreign_pk: i64 = sqlx::query_scalar(
        "INSERT INTO turn_nodes (id, scope_id, kind, parent_pk, payload_version, created_at) \
         VALUES ($1, $2, $3, NULL, 6, $4) RETURNING node_pk",
    )
    .bind(foreign_id.as_str())
    .bind(other_scope)
    .bind(TurnNodeKind::Response.as_str())
    .bind(chrono::Utc::now().timestamp_millis())
    .fetch_one(&mut *connection)
    .await
    .unwrap();

    let original = contract_payload();
    let mut encoded = super::content::encode(original.clone()).unwrap();
    let payload = encoded.payload.clone();
    let distinct = encoded.contents.len();
    let mut transaction = connection.begin().await.unwrap();
    sqlx::query("LOCK TABLE turn_nodes IN ROW EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE turn_contents IN ROW EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await
        .unwrap();
    super::content::put_postgres(
        &mut transaction,
        node_pk,
        owner_scope,
        &mut encoded.contents,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    let probe = super::content::encode(original.clone()).unwrap();
    let hints = super::content::lookup_postgres(
        &mut connection,
        super::content::LookupScope::Principal(&owner.continuation_key()),
        &probe.contents,
        false,
    )
    .await
    .unwrap();
    assert_eq!(hints.iter().filter(|id| id.is_some()).count(), distinct);

    let mut foreign = super::content::encode(original.clone()).unwrap();
    let foreign_payload = foreign.payload.clone();
    let mut transaction = connection.begin().await.unwrap();
    sqlx::query("LOCK TABLE turn_nodes IN ROW EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE turn_contents IN ROW EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await
        .unwrap();
    super::content::put_postgres(
        &mut transaction,
        foreign_pk,
        other_scope,
        &mut foreign.contents,
    )
    .await
    .unwrap();
    transaction.commit().await.unwrap();
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turn_contents")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(total, i64::try_from(distinct * 2).unwrap());

    let restored = postgres_restore(
        &mut connection,
        &owner.continuation_key(),
        node_pk,
        id,
        TurnNodeKind::Response,
        payload,
    )
    .await
    .unwrap();
    assert_eq!(restored[0].payload, original);
    let error = postgres_restore(
        &mut connection,
        &owner.continuation_key(),
        foreign_pk,
        foreign_id,
        TurnNodeKind::Response,
        foreign_payload,
    )
    .await;
    assert!(error.is_err(), "cross-Principal content must not restore");
}

#[tokio::test]
async fn postgres_content_contract_when_configured() {
    let Some(url) = std::env::var("DB_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
    else {
        return;
    };
    let admin = PgPool::connect(&url).await.unwrap();
    let schema = format!("stravia_content_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.options([("search_path", schema.as_str())]))
        .await
        .unwrap();
    crate::migrations::migrate_postgres(&pool, None)
        .await
        .unwrap();
    postgres_content_contract(&pool).await;
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
