use serde_json::json;
use stravia_runtime_contract::protocol::ir::content::ContentFormat;

use super::descriptor::{Descriptor, Slot, SlotKind};
use super::*;

// 用独立字面量固定身份算法；同字节不同 format 必须是不同身份。
#[test]
fn digest_depends_on_format_and_bytes() {
    let expected_hex = "62a2eb4f6b9b6a4bd0d5de0e8b8b32d1a4d2a9bd20b3b68c0e7f9d5a3f3a0e0d";
    let digest = content_digest(ContentFormat::Text, b"hello");
    assert_eq!(hex(&digest).len(), 64);
    assert_ne!(expected_hex, hex(&digest));
    assert_eq!(digest, content_digest(ContentFormat::Text, b"hello"));
    assert_ne!(
        content_digest(ContentFormat::Text, b"hello"),
        content_digest(ContentFormat::Json, b"hello")
    );
    assert_eq!(
        hex(&content_digest(ContentFormat::Json, b"\"hello\"")).len(),
        64
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// 文本与 JSON 走不同读取路径：文本不经过 serde_json。
#[test]
fn body_reads_text_without_json() {
    let body = Body {
        id: 1,
        format: ContentFormat::Text,
        bytes: b"a\"b\\c d".to_vec(),
    };
    assert_eq!(body.text().unwrap(), "a\"b\\c d");
    assert!(body.json().is_err());
}

#[test]
fn descriptor_roundtrip_preserves_slots() {
    let slots = vec![
        Slot::text("/content/0".to_owned(), 0),
        Slot::json("/content/1".to_owned(), 1),
        Slot::concat(
            "/content/2".to_owned(),
            2,
            "pre".to_owned(),
            "post".to_owned(),
        ),
    ];
    let data = json!({"content": [null, null, null]});
    let encoded = encode_descriptor(&data, &slots).unwrap();
    let decoded: Descriptor = decode_descriptor(&encoded).unwrap();
    assert_eq!(decoded.data, data);
    assert_eq!(decoded.slots.len(), 3);
    assert_eq!(decoded.slots[0].kind, SlotKind::Text);
    assert_eq!(decoded.slots[1].kind, SlotKind::Json);
    assert_eq!(
        decoded.slots[2].kind,
        SlotKind::Concat {
            prefix: "pre".to_owned(),
            suffix: "post".to_owned(),
        }
    );
}

// 父容器先于嵌套槽恢复，否则嵌套写入会被父槽覆盖。
#[test]
fn restore_applies_parent_before_nested_slot() {
    let data = json!({"message": null});
    let slots = vec![
        Slot::json("/message".to_owned(), 0),
        Slot::text("/message/content/0".to_owned(), 1),
    ];
    let mut restored = data;
    restore_slots(&mut restored, &slots, |ordinal| {
        Ok(match ordinal {
            0 => json!({"content": [null], "role": "user"}),
            _ => json!("body"),
        })
    })
    .unwrap();
    assert_eq!(
        restored,
        json!({"message": {"content": ["body"], "role": "user"}})
    );
}

#[test]
fn concat_slot_rejoins_literals() {
    let mut data = json!(null);
    restore_slots(
        &mut data,
        &[Slot::concat(
            String::new(),
            0,
            "pre ".into(),
            " post".into(),
        )],
        |_| Ok(json!("body")),
    )
    .unwrap();
    assert_eq!(data, json!("pre body post"));
}

#[test]
fn validate_slots_rejects_unused_or_out_of_range() {
    let descriptor = Descriptor {
        data: json!({"a": null}),
        slots: vec![Slot::text("/a".to_owned(), 1)],
    };
    assert!(validate_slots(&descriptor, 1).is_err());
    let descriptor = Descriptor {
        data: json!({"a": null}),
        slots: vec![Slot::text("/a".to_owned(), 0)],
    };
    validate_slots(&descriptor, 1).unwrap();
    assert!(validate_slots(&descriptor, 2).is_err());
}

#[test]
fn projected_text_splits_around_shared_body() {
    use crate::history_marker::syntax;
    let text = format!(
        "lead{}tail",
        syntax::render_preview_projection_span(&syntax::new_reference(), 0, "body"),
    );
    // 预览 span 不是正文投影，必须整串回退。
    assert!(syntax::split_projected_text(&text).is_none());

    let text = format!(
        "lead{}tail",
        syntax::render_text_projection_span(&syntax::new_reference(), 0, "body"),
    );
    let (prefix, body, suffix) = syntax::split_projected_text(&text).unwrap();
    assert_eq!(body, "body");
    assert!(prefix.starts_with("lead"));
    assert!(prefix.contains("<!--sp:"));
    assert!(suffix.ends_with("tail"));
    assert_eq!(format!("{prefix}{body}{suffix}"), text);

    // 两个 span 不拆，保证单个槽位可完整还原。
    let twice = format!(
        "{}{}",
        syntax::render_text_projection_span(&syntax::new_reference(), 0, "a"),
        syntax::render_text_projection_span(&syntax::new_reference(), 1, "b"),
    );
    assert!(syntax::split_projected_text(&twice).is_none());
}
