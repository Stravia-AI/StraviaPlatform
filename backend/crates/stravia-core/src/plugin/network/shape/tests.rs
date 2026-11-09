use std::sync::Arc;

use super::*;
use rustls::{ClientConnection, pki_types::ServerName};

struct Hello {
    suites: Vec<u16>,
    extensions: Vec<(u16, Vec<u8>)>,
}

fn u16_at(input: &[u8], offset: usize) -> usize {
    u16::from_be_bytes([input[offset], input[offset + 1]]) as usize
}

fn capture_wire(config: Arc<rustls::ClientConfig>) -> Vec<u8> {
    capture_wire_for_host(config, "localhost")
}

fn capture_wire_for_host(config: Arc<rustls::ClientConfig>, host: &str) -> Vec<u8> {
    let mut connection =
        ClientConnection::new(config, ServerName::try_from(host.to_owned()).unwrap()).unwrap();
    let mut wire = Vec::new();
    connection.write_tls(&mut wire).unwrap();
    wire
}

fn capture(config: Arc<rustls::ClientConfig>) -> Hello {
    parse_wire(&capture_wire(config))
}

fn parse_wire(wire: &[u8]) -> Hello {
    assert_eq!(wire[0], 22);
    assert_eq!(wire[5], 1);
    let mut pos = 9 + 2 + 32;
    pos += 1 + wire[pos] as usize;
    let suites_len = u16_at(wire, pos);
    pos += 2;
    let suites = wire[pos..pos + suites_len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    pos += suites_len;
    pos += 1 + wire[pos] as usize;
    let extensions_len = u16_at(wire, pos);
    pos += 2;
    let end = pos + extensions_len;
    let mut extensions = Vec::new();
    while pos < end {
        let typ = u16_at(wire, pos) as u16;
        let len = u16_at(wire, pos + 2);
        pos += 4;
        extensions.push((typ, wire[pos..pos + len].to_vec()));
        pos += len;
    }
    assert_eq!(pos, end);
    Hello { suites, extensions }
}

fn u16_list(bytes: &[u8]) -> Vec<u16> {
    let len = u16_at(bytes, 0);
    assert_eq!(len + 2, bytes.len());
    bytes[2..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect()
}

fn extension(hello: &Hello, typ: u16) -> &[u8] {
    &hello
        .extensions
        .iter()
        .find(|(id, _)| *id == typ)
        .unwrap()
        .1
}

fn list_text(items: &[u16]) -> String {
    items
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join("-")
}

fn short_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let hash = Sha256::digest(text.as_bytes());
    let mut result = String::new();
    for byte in &hash[..6] {
        write!(result, "{byte:02x}").unwrap();
    }
    result
}

fn shape_digest(shape: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut configuration = shape.clone();
    let fields = configuration.as_object_mut().unwrap();
    fields.remove("name");
    fields.remove("revision");
    fields.remove("source");
    configuration.sort_all_objects();
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(serde_json::to_vec(&configuration).unwrap()) {
        write!(digest, "{byte:02x}").unwrap();
    }
    digest
}

fn revision_preserves_identity(shape: &serde_json::Value, baseline: &serde_json::Value) -> bool {
    let revision = shape["revision"].as_u64().unwrap();
    let released = baseline["revision"].as_u64().unwrap();
    revision > released
        || (revision == released && shape_digest(shape) == baseline["sha256"].as_str().unwrap())
}

#[test]
fn builtin_shape_revisions_do_not_reuse_changed_connection_identity() {
    let released: serde_json::Value = serde_json::from_str(include_str!("released.json")).unwrap();
    for source in BUILTIN_SHAPES {
        let shape: serde_json::Value = serde_json::from_str(source).unwrap();
        let name = shape["name"].as_str().unwrap();
        if let Some(baseline) = released.get(name) {
            assert!(
                revision_preserves_identity(&shape, baseline),
                "{name}: changed connection identity requires a higher revision"
            );
        }
    }
}

#[test]
fn changed_shape_requires_revision_increase_but_provenance_does_not() {
    let mut shape =
        serde_json::json!({"name": "client", "revision": 2, "tls": {"session_tickets": true}});
    let baseline = serde_json::json!({"revision": 2, "sha256": shape_digest(&shape)});
    shape["source"] = serde_json::json!({"client": "capture provenance update"});
    assert!(revision_preserves_identity(&shape, &baseline));
    shape["tls"]["session_tickets"] = false.into();
    assert!(!revision_preserves_identity(&shape, &baseline));
    shape["revision"] = 1.into();
    assert!(!revision_preserves_identity(&shape, &baseline));
    shape["revision"] = 3.into();
    assert!(revision_preserves_identity(&shape, &baseline));
}

fn ja3(hello: &Hello) -> String {
    let groups = u16_list(extension(hello, 10));
    let formats = extension(hello, 11)[1..]
        .iter()
        .map(|n| u16::from(*n))
        .collect::<Vec<_>>();
    format!(
        "771,{},{},{},{}",
        list_text(&hello.suites),
        list_text(&hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>()),
        list_text(&groups),
        list_text(&formats)
    )
}

fn ja4(hello: &Hello) -> String {
    let mut suites = hello.suites.clone();
    suites.sort_unstable();
    let suite_part = suites
        .iter()
        .map(|n| format!("{n:04x}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut ids = hello
        .extensions
        .iter()
        .map(|e| e.0)
        .filter(|n| *n != 0 && *n != 16)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    let mut ext_part = ids
        .iter()
        .map(|n| format!("{n:04x}"))
        .collect::<Vec<_>>()
        .join(",");
    ext_part.push('_');
    ext_part.push_str(
        &u16_list(extension(hello, 13))
            .iter()
            .map(|n| format!("{n:04x}"))
            .collect::<Vec<_>>()
            .join(","),
    );
    let alpn = hello.extensions.iter().find(|e| e.0 == 16).map_or_else(
        || "00".to_owned(),
        |(_, data)| {
            let protocol = &data[3..3 + usize::from(data[2])];
            format!(
                "{}{}",
                protocol[0] as char,
                protocol[protocol.len() - 1] as char
            )
        },
    );
    format!(
        "t13d{:02}{:02}{}_{}_{}",
        suites.len(),
        hello.extensions.len(),
        alpn,
        short_sha256(&suite_part),
        short_sha256(&ext_part)
    )
}

#[test]
fn clienthello_matches_independently_captured_fingerprints() {
    let registry = ShapeRegistry::builtin().unwrap();
    for (name, fixture, websocket, random_order) in [
        (
            "claude-code",
            include_str!("fixtures/claude-code.json"),
            false,
            false,
        ),
        (
            "antigravity",
            include_str!("fixtures/antigravity-inference.json"),
            false,
            false,
        ),
        (
            "bun",
            include_str!("fixtures/bun-1.4.2-http.json"),
            false,
            false,
        ),
        (
            "bun",
            include_str!("fixtures/bun-1.4.2-ws.json"),
            true,
            false,
        ),
        (
            "bun-1-3-14",
            include_str!("fixtures/bun-1.3.14-http.json"),
            false,
            false,
        ),
        (
            "bun-1-3-14",
            include_str!("fixtures/bun-1.3.14-ws.json"),
            true,
            false,
        ),
        (
            "codex-http",
            include_str!("fixtures/codex-http.json"),
            false,
            false,
        ),
        (
            "bun-1-3-14",
            include_str!("fixtures/opencode-interactive.json"),
            false,
            false,
        ),
        (
            "command-code",
            include_str!("fixtures/command-code-interactive.json"),
            false,
            false,
        ),
        (
            "devin",
            include_str!("fixtures/devin-interactive.json"),
            false,
            true,
        ),
        (
            "grok",
            include_str!("fixtures/grok-interactive.json"),
            false,
            true,
        ),
        (
            "codex-ws",
            include_str!("fixtures/codex-ws.json"),
            true,
            true,
        ),
    ] {
        let wire = capture_wire(registry.get(name).unwrap().tls_for("direct", websocket));
        assert_wire_matches_fixture(name, &wire, fixture, random_order);
    }
}

pub(crate) fn assert_wire_server_name(wire: &[u8], host: &str) {
    let hello = parse_wire(wire);
    let sni = extension(&hello, 0);
    assert_eq!(sni.len(), host.len() + 5);
    assert_eq!(u16_at(sni, 0), host.len() + 3);
    assert_eq!(sni[2], 0);
    assert_eq!(u16_at(sni, 3), host.len());
    assert_eq!(&sni[5..], host.as_bytes());
}

#[test]
fn bun_padding_tracks_sni_length_and_stays_zero_filled() {
    let registry = ShapeRegistry::builtin().unwrap();
    let config = registry.get("bun-1-3-14").unwrap().tls_for("direct", false);
    let fixture = include_str!("fixtures/opencode-interactive.json");
    for (host, padding_len) in [("localhost", 239), ("opencode.ai", 237)] {
        let wire = capture_wire_for_host(config.clone(), host);
        assert_wire_server_name(&wire, host);
        assert_eq!(wire.len() - 5, 512);
        assert_eq!(extension(&parse_wire(&wire), 21), vec![0; padding_len]);
        assert_wire_matches_fixture("bun-1-3-14", &wire, fixture, false);
    }
}

pub(crate) fn assert_wire_matches_fixture(
    name: &str,
    wire: &[u8],
    fixture: &str,
    random_order: bool,
) {
    let sample: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let expected = &sample["clienthello"];
    let hello = parse_wire(wire);
    let expected_suites: Vec<u16> =
        serde_json::from_value(expected["cipher_suites"].clone()).unwrap();
    assert_eq!(hello.suites, expected_suites, "{name}: suites");
    let mut actual_exts: Vec<u16> = hello.extensions.iter().map(|e| e.0).collect();
    let mut expected_exts: Vec<u16> =
        serde_json::from_value(expected["extensions"].clone()).unwrap();
    if random_order {
        actual_exts.sort_unstable();
        expected_exts.sort_unstable();
    }
    assert_eq!(actual_exts, expected_exts, "{name}: extensions");
    for detail in expected["extension_details"].as_array().unwrap() {
        if !detail["body"].is_null() {
            let typ = u16::try_from(detail["type"].as_u64().unwrap()).unwrap();
            let body: Vec<u8> = serde_json::from_value(detail["body"].clone()).unwrap();
            if name == "bun-1-3-14" && typ == 21 {
                // The capture used localhost. BoringSSL pads the whole ClientHello,
                // so a different SNI length changes the zero padding by the same amount.
                let captured_sni_len = expected["extension_details"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|extension| extension["type"] == 0)
                    .unwrap()["length"]
                    .as_u64()
                    .unwrap() as usize;
                let actual_sni_len = extension(&hello, 0).len();
                assert!(
                    body.iter().all(|byte| *byte == 0),
                    "{name}: captured padding must be zero-filled"
                );
                let padding_len = body
                    .len()
                    .checked_add(captured_sni_len)
                    .unwrap()
                    .checked_sub(actual_sni_len)
                    .expect("SNI exceeds captured padding budget");
                assert_eq!(
                    extension(&hello, typ),
                    vec![0; padding_len],
                    "{name}: extension {typ} body"
                );
            } else {
                assert_eq!(extension(&hello, typ), body, "{name}: extension {typ} body");
            }
        }
    }
    let groups: Vec<u16> = serde_json::from_value(expected["supported_groups"].clone()).unwrap();
    let signatures: Vec<u16> =
        serde_json::from_value(expected["signature_algorithms"].clone()).unwrap();
    assert_eq!(u16_list(extension(&hello, 10)), groups, "{name}: groups");
    assert_eq!(
        u16_list(extension(&hello, 13)),
        signatures,
        "{name}: signatures"
    );
    let shares = extension(&hello, 51);
    let mut position = 2;
    let mut actual_shares = Vec::new();
    while position < shares.len() {
        actual_shares.push(u16_at(shares, position) as u16);
        let length = u16_at(shares, position + 2);
        position += 4 + length;
    }
    assert_eq!(position, shares.len());
    let expected_shares: Vec<u16> =
        serde_json::from_value(expected["key_share_groups"].clone()).unwrap();
    assert_eq!(actual_shares, expected_shares, "{name}: key shares");
    let expected_alpn: Vec<String> = serde_json::from_value(expected["alpn"].clone()).unwrap();
    let actual_alpn = hello
        .extensions
        .iter()
        .find(|e| e.0 == 16)
        .map(|(_, data)| {
            let mut offset = 2;
            let mut protocols = Vec::new();
            while offset < data.len() {
                let len = data[offset] as usize;
                offset += 1;
                protocols.push(String::from_utf8(data[offset..offset + len].to_vec()).unwrap());
                offset += len;
            }
            protocols
        })
        .unwrap_or_default();
    assert_eq!(actual_alpn, expected_alpn, "{name}: ALPN");
    if !random_order {
        assert_eq!(
            ja3(&hello),
            expected["ja3"].as_str().unwrap(),
            "{name}: JA3"
        );
    }
    assert_eq!(
        ja4(&hello),
        expected["ja4"].as_str().unwrap(),
        "{name}: JA4"
    );
}

#[test]
fn builtin_shapes_load_and_are_isolated_by_proxy_and_protocol() {
    let registry = ShapeRegistry::builtin().unwrap();
    for name in [
        "default",
        "claude-code",
        "bun",
        "antigravity",
        "codex-http",
        "codex-ws",
    ] {
        assert!(registry.get(name).is_some());
    }
    assert!(registry.get("missing").is_none());
    let shape = registry.get("default").unwrap();
    let http = shape.tls_for("direct", false);
    assert!(Arc::ptr_eq(&http, &shape.tls_for("direct", false)));
    assert!(!Arc::ptr_eq(&http, &shape.tls_for("proxy-a", false)));
    assert!(!Arc::ptr_eq(&http, &shape.tls_for("direct", true)));
    assert_eq!(http.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
    assert_eq!(
        shape.tls_for("direct", true).alpn_protocols,
        [b"http/1.1".to_vec()]
    );
}

#[test]
fn captured_claude_hello_matches_order_and_declared_algorithms() {
    let registry = ShapeRegistry::builtin().unwrap();
    let hello = capture(
        registry
            .get("claude-code")
            .unwrap()
            .tls_for("direct", false),
    );
    assert_eq!(
        hello.suites,
        [
            0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc009, 0xc013,
            0xc00a, 0xc014, 0x009c, 0x009d, 0x002f, 0x0035
        ]
    );
    assert_eq!(
        hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>(),
        [0, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43]
    );
    assert_eq!(u16_list(extension(&hello, 10)), [0x11ec, 29, 23, 24]);
    assert_eq!(
        u16_list(extension(&hello, 13)),
        [
            0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501, 0x0806, 0x0601, 0x0807, 0x0603, 0x0201
        ]
    );
    assert_eq!(extension(&hello, 11), [1, 0]);
    assert!(extension(&hello, 18).is_empty());
    assert_eq!(
        extension(&hello, 16),
        [0, 9, 8, b'h', b't', b't', b'p', b'/', b'1', b'.', b'1']
    );
}

#[test]
fn captured_codex_http_hello_keeps_unimplemented_offers_without_implementing_them() {
    let registry = ShapeRegistry::builtin().unwrap();
    let hello = capture(registry.get("codex-http").unwrap().tls_for("direct", false));
    assert_eq!(
        hello.suites,
        [
            0x1302, 0x1303, 0x1301, 0xc02c, 0xc030, 0x009f, 0xcca9, 0xcca8, 0xccaa, 0xc02b, 0xc02f,
            0x009e, 0xc024, 0xc028, 0x006b, 0xc023, 0xc027, 0x0067, 0xc00a, 0xc014, 0x0039, 0xc009,
            0xc013, 0x0033, 0x009d, 0x009c, 0x003d, 0x003c, 0x0035, 0x002f
        ]
    );
    assert_eq!(
        hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>(),
        [65281, 0, 11, 10, 35, 22, 23, 13, 43, 45, 51]
    );
    assert_eq!(
        u16_list(extension(&hello, 10)),
        [0x11ec, 29, 23, 30, 24, 25, 256, 257]
    );
    assert_eq!(
        u16_list(extension(&hello, 13)),
        [
            0x0905, 0x0906, 0x0904, 0x0403, 0x0503, 0x0603, 0x0807, 0x0808, 0x081a, 0x081b, 0x081c,
            0x0809, 0x080a, 0x080b, 0x0804, 0x0805, 0x0806, 0x0401, 0x0501, 0x0601, 0x0303, 0x0301,
            0x0302, 0x0402, 0x0502, 0x0602
        ]
    );
    assert!(extension(&hello, 22).is_empty());
}

#[test]
fn captured_codex_ws_hello_has_no_alpn_and_random_extension_order() {
    let registry = ShapeRegistry::builtin().unwrap();
    let hello = capture(registry.get("codex-ws").unwrap().tls_for("direct", true));
    assert_eq!(
        hello.suites,
        [
            0x1302, 0x1301, 0x1303, 0xc02c, 0xc02b, 0xcca9, 0xc030, 0xc02f, 0xcca8, 0x00ff
        ]
    );
    let mut ids = hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, [0, 5, 10, 11, 13, 23, 35, 43, 45, 51]);
    assert_eq!(u16_list(extension(&hello, 10)), [0x11ec, 29, 23, 24]);
    assert_eq!(
        u16_list(extension(&hello, 13)),
        [
            0x0503, 0x0403, 0x0603, 0x0807, 0x0806, 0x0805, 0x0804, 0x0601, 0x0501, 0x0401, 0x0904,
            0x0905, 0x0906
        ]
    );
}

pub(crate) fn assert_default_matches_client_hello(wire: &[u8]) {
    let old = parse_wire(wire);
    let registry = ShapeRegistry::builtin().unwrap();
    let new = capture(registry.get("default").unwrap().tls_for("direct", false));
    assert_eq!(old.suites, new.suites);
    let mut old_ids: Vec<_> = old.extensions.iter().map(|e| e.0).collect();
    let mut new_ids: Vec<_> = new.extensions.iter().map(|e| e.0).collect();
    old_ids.sort_unstable();
    new_ids.sort_unstable();
    assert_eq!(old_ids, new_ids);
    for typ in [10, 11, 13, 16, 43, 45] {
        assert_eq!(
            extension(&old, typ),
            extension(&new, typ),
            "extension {typ}"
        );
    }
}

#[test]
fn invalid_registry_documents_fail_with_field_context() {
    let default = include_str!("data/default.json");
    let duplicate = ShapeRegistry::from_json_sources(&[default, default])
        .err()
        .unwrap();
    assert!(duplicate.to_string().contains("name"));
    let missing = ShapeRegistry::from_json_sources(&[]).err().unwrap();
    assert!(missing.to_string().contains("default"));
    let invalid = default.replace(
        "\"session_tickets\": true",
        "\"cipher_suites\": [\"0xdead\"], \"session_tickets\": true",
    );
    let error = ShapeRegistry::from_json_sources(&[&invalid]).err().unwrap();
    assert!(error.to_string().contains("cipher_suites"));
    let mut duplicate_extension: serde_json::Value = serde_json::from_str(default).unwrap();
    duplicate_extension["tls"]["raw_extensions"] = serde_json::json!([
        { "type": 18, "data": "" }, { "type": 18, "data": "" }
    ]);
    let json = duplicate_extension.to_string();
    let error = ShapeRegistry::from_json_sources(&[&json]).err().unwrap();
    assert!(error.to_string().contains("raw_extensions"));
}

#[test]
fn plugin_shapes_are_isolated_and_share_builtins() {
    let base = Arc::new(ShapeRegistry::builtin().unwrap());
    assert!(Arc::ptr_eq(
        &base,
        &base.with_plugin("component-a", &[]).unwrap()
    ));
    let mut shape: EgressShape = serde_json::from_str(include_str!("data/default.json")).unwrap();
    shape.name = "example".into();
    let a = base.with_plugin("component-a", &[shape.clone()]).unwrap();
    let b = base.with_plugin("component-b", &[shape.clone()]).unwrap();
    assert!(base.get("plugin:example").is_none());
    assert!(a.get("example").is_none());
    assert!(a.get("plugin:example").is_some());
    assert!(std::ptr::eq(
        base.get("default").unwrap(),
        a.get("default").unwrap()
    ));
    assert_ne!(
        a.get("plugin:example").unwrap().identity,
        b.get("plugin:example").unwrap().identity
    );
    assert_ne!(
        a.get("plugin:example").unwrap().identity,
        a.get("default").unwrap().identity
    );
    shape.name = "other".into();
    let c = a.with_plugin("component-c", &[shape]).unwrap();
    assert!(c.get("plugin:example").is_none());
    assert!(c.get("plugin:other").is_some());
    assert!(c.get("default").is_some());
    let empty = a.with_plugin("component-d", &[]).unwrap();
    assert!(empty.get("plugin:example").is_none());
    assert!(empty.get("default").is_some());
}

#[test]
fn plugin_shape_compilation_fails_closed() {
    let base = Arc::new(ShapeRegistry::builtin().unwrap());
    let mut shape: EgressShape = serde_json::from_str(include_str!("data/default.json")).unwrap();
    shape.name = "example".into();
    assert!(
        base.with_plugin("component", &[shape.clone(), shape.clone()])
            .is_err()
    );
    shape.revision = 0;
    assert!(base.with_plugin("component", &[shape.clone()]).is_err());
    shape.revision = 1;
    shape.name = "bad:name".into();
    assert!(base.with_plugin("component", &[shape.clone()]).is_err());
    shape.name = "example".into();
    shape.source = Some(stravia_vendor_sdk::net::shape::ShapeSource {
        client: " ".into(),
        runtime: "runtime".into(),
        captured: "capture".into(),
        ja4: "fingerprint".into(),
    });
    assert!(base.with_plugin("component", &[shape.clone()]).is_err());
    shape.source = None;
    shape.http1.slot_positions.host = Some("middle".into());
    assert!(base.with_plugin("component", &[shape]).is_err());
    assert!(base.get("plugin:example").is_none());
}
