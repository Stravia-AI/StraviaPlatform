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

fn capture(config: Arc<rustls::ClientConfig>) -> Hello {
    let mut connection = ClientConnection::new(config, ServerName::try_from("localhost").unwrap()).unwrap();
    let mut wire = Vec::new();
    connection.write_tls(&mut wire).unwrap();
    parse_wire(&wire)
}

fn parse_wire(wire: &[u8]) -> Hello {
    assert_eq!(wire[0], 22);
    assert_eq!(wire[5], 1);
    let mut pos = 9 + 2 + 32;
    pos += 1 + wire[pos] as usize;
    let suites_len = u16_at(&wire, pos);
    pos += 2;
    let suites = wire[pos..pos + suites_len].chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
    pos += suites_len;
    pos += 1 + wire[pos] as usize;
    let extensions_len = u16_at(&wire, pos);
    pos += 2;
    let end = pos + extensions_len;
    let mut extensions = Vec::new();
    while pos < end {
        let typ = u16_at(&wire, pos) as u16;
        let len = u16_at(&wire, pos + 2);
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
    bytes[2..].chunks_exact(2).map(|p| u16::from_be_bytes([p[0], p[1]])).collect()
}

fn extension<'a>(hello: &'a Hello, typ: u16) -> &'a [u8] {
    &hello.extensions.iter().find(|(id, _)| *id == typ).unwrap().1
}

fn list_text(items: &[u16]) -> String {
    items.iter().map(u16::to_string).collect::<Vec<_>>().join("-")
}

fn short_sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let hash = Sha256::digest(text.as_bytes());
    let mut result = String::new();
    for byte in &hash[..6] { write!(result, "{byte:02x}").unwrap(); }
    result
}

fn ja3(hello: &Hello) -> String {
    let groups = u16_list(extension(hello, 10));
    let formats = extension(hello, 11)[1..].iter().map(|n| u16::from(*n)).collect::<Vec<_>>();
    format!("771,{},{},{},{}", list_text(&hello.suites), list_text(&hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>()), list_text(&groups), list_text(&formats))
}

fn ja4(hello: &Hello) -> String {
    let mut suites = hello.suites.clone();
    suites.sort_unstable();
    let suite_part = suites.iter().map(|n| format!("{n:04x}")).collect::<Vec<_>>().join(",");
    let mut ids = hello.extensions.iter().map(|e| e.0).filter(|n| *n != 0 && *n != 16).collect::<Vec<_>>();
    ids.sort_unstable();
    let mut ext_part = ids.iter().map(|n| format!("{n:04x}")).collect::<Vec<_>>().join(",");
    ext_part.push('_');
    ext_part.push_str(&u16_list(extension(hello, 13)).iter().map(|n| format!("{n:04x}")).collect::<Vec<_>>().join(","));
    let alpn = if hello.extensions.iter().any(|e| e.0 == 16) { "h1" } else { "00" };
    format!("t13d{:02}{:02}{}_{}_{}", suites.len(), hello.extensions.len(), alpn, short_sha256(&suite_part), short_sha256(&ext_part))
}

#[test]
fn clienthello_matches_independently_captured_fingerprints() {
    let registry = ShapeRegistry::builtin().unwrap();
    for (name, fixture, websocket, random_order) in [
        ("claude-code", include_str!("fixtures/claude-code.json"), false, false),
        ("antigravity", include_str!("fixtures/antigravity-inference.json"), false, false),
        ("bun", include_str!("fixtures/bun-1.4.2-http.json"), false, false),
        ("bun", include_str!("fixtures/bun-1.4.2-ws.json"), true, false),
        ("codex-http", include_str!("fixtures/codex-http.json"), false, false),
        ("codex-ws", include_str!("fixtures/codex-ws.json"), true, true),
    ] {
        let sample: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let expected = &sample["clienthello"];
        let hello = capture(registry.get(name).unwrap().tls_for("direct", websocket));
        let expected_suites: Vec<u16> = serde_json::from_value(expected["cipher_suites"].clone()).unwrap();
        assert_eq!(hello.suites, expected_suites, "{name}: suites");
        let mut actual_exts: Vec<u16> = hello.extensions.iter().map(|e| e.0).collect();
        let mut expected_exts: Vec<u16> = serde_json::from_value(expected["extensions"].clone()).unwrap();
        if random_order { actual_exts.sort_unstable(); expected_exts.sort_unstable(); }
        assert_eq!(actual_exts, expected_exts, "{name}: extensions");
        let groups: Vec<u16> = serde_json::from_value(expected["supported_groups"].clone()).unwrap();
        let signatures: Vec<u16> = serde_json::from_value(expected["signature_algorithms"].clone()).unwrap();
        assert_eq!(u16_list(extension(&hello, 10)), groups, "{name}: groups");
        assert_eq!(u16_list(extension(&hello, 13)), signatures, "{name}: signatures");
        let shares = extension(&hello, 51);
        let mut position = 2;
        let mut actual_shares = Vec::new();
        while position < shares.len() {
            actual_shares.push(u16_at(shares, position) as u16);
            let length = u16_at(shares, position + 2);
            position += 4 + length;
        }
        assert_eq!(position, shares.len());
        let expected_shares: Vec<u16> = serde_json::from_value(expected["key_share_groups"].clone()).unwrap();
        assert_eq!(actual_shares, expected_shares, "{name}: key shares");
        let expected_alpn: Vec<String> = serde_json::from_value(expected["alpn"].clone()).unwrap();
        let actual_alpn = hello.extensions.iter().find(|e| e.0 == 16).map(|(_, data)| {
            let mut offset = 2;
            let mut protocols = Vec::new();
            while offset < data.len() {
                let len = data[offset] as usize;
                offset += 1;
                protocols.push(String::from_utf8(data[offset..offset + len].to_vec()).unwrap());
                offset += len;
            }
            protocols
        }).unwrap_or_default();
        assert_eq!(actual_alpn, expected_alpn, "{name}: ALPN");
        if !random_order { assert_eq!(ja3(&hello), expected["ja3"].as_str().unwrap(), "{name}: JA3"); }
        assert_eq!(ja4(&hello), expected["ja4"].as_str().unwrap(), "{name}: JA4");
    }
}

#[test]
fn builtin_shapes_load_and_are_isolated_by_proxy_and_protocol() {
    let registry = ShapeRegistry::builtin().unwrap();
    for name in ["default", "claude-code", "bun", "antigravity", "codex-http", "codex-ws"] { assert!(registry.get(name).is_some()); }
    assert!(registry.get("missing").is_none());
    let shape = registry.get("default").unwrap();
    let http = shape.tls_for("direct", false);
    assert!(Arc::ptr_eq(&http, &shape.tls_for("direct", false)));
    assert!(!Arc::ptr_eq(&http, &shape.tls_for("proxy-a", false)));
    assert!(!Arc::ptr_eq(&http, &shape.tls_for("direct", true)));
    assert_eq!(http.alpn_protocols, [b"h2".to_vec(), b"http/1.1".to_vec()]);
    assert_eq!(shape.tls_for("direct", true).alpn_protocols, [b"http/1.1".to_vec()]);
}

#[test]
fn captured_claude_hello_matches_order_and_declared_algorithms() {
    let registry = ShapeRegistry::builtin().unwrap();
    let hello = capture(registry.get("claude-code").unwrap().tls_for("direct", false));
    assert_eq!(hello.suites, [0x1301,0x1302,0x1303,0xc02b,0xc02f,0xc02c,0xc030,0xcca9,0xcca8,0xc009,0xc013,0xc00a,0xc014,0x009c,0x009d,0x002f,0x0035]);
    assert_eq!(hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>(), [0,23,65281,10,11,35,16,5,13,18,51,45,43]);
    assert_eq!(u16_list(extension(&hello, 10)), [0x11ec,29,23,24]);
    assert_eq!(u16_list(extension(&hello, 13)), [0x0403,0x0804,0x0401,0x0503,0x0805,0x0501,0x0806,0x0601,0x0807,0x0603,0x0201]);
    assert_eq!(extension(&hello, 11), [1,0]);
    assert!(extension(&hello, 18).is_empty());
    assert_eq!(extension(&hello, 16), [0,9,8,b'h',b't',b't',b'p',b'/',b'1',b'.',b'1']);
}

#[test]
fn captured_codex_http_hello_keeps_unimplemented_offers_without_implementing_them() {
    let registry = ShapeRegistry::builtin().unwrap();
    let hello = capture(registry.get("codex-http").unwrap().tls_for("direct", false));
    assert_eq!(hello.suites, [0x1302,0x1303,0x1301,0xc02c,0xc030,0x009f,0xcca9,0xcca8,0xccaa,0xc02b,0xc02f,0x009e,0xc024,0xc028,0x006b,0xc023,0xc027,0x0067,0xc00a,0xc014,0x0039,0xc009,0xc013,0x0033,0x009d,0x009c,0x003d,0x003c,0x0035,0x002f]);
    assert_eq!(hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>(), [65281,0,11,10,35,22,23,13,43,45,51]);
    assert_eq!(u16_list(extension(&hello, 10)), [0x11ec,29,23,30,24,25,256,257]);
    assert_eq!(u16_list(extension(&hello, 13)), [0x0905,0x0906,0x0904,0x0403,0x0503,0x0603,0x0807,0x0808,0x081a,0x081b,0x081c,0x0809,0x080a,0x080b,0x0804,0x0805,0x0806,0x0401,0x0501,0x0601,0x0303,0x0301,0x0302,0x0402,0x0502,0x0602]);
    assert!(extension(&hello, 22).is_empty());
}

#[test]
fn captured_codex_ws_hello_has_no_alpn_and_random_extension_order() {
    let registry = ShapeRegistry::builtin().unwrap();
    let hello = capture(registry.get("codex-ws").unwrap().tls_for("direct", true));
    assert_eq!(hello.suites, [0x1302,0x1301,0x1303,0xc02c,0xc02b,0xcca9,0xc030,0xc02f,0xcca8,0x00ff]);
    let mut ids = hello.extensions.iter().map(|e| e.0).collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, [0,5,10,11,13,23,35,43,45,51]);
    assert_eq!(u16_list(extension(&hello, 10)), [0x11ec,29,23,24]);
    assert_eq!(u16_list(extension(&hello, 13)), [0x0503,0x0403,0x0603,0x0807,0x0806,0x0805,0x0804,0x0601,0x0501,0x0401,0x0904,0x0905,0x0906]);
}

#[tokio::test]
async fn default_shape_matches_existing_reqwest_client_hello() {
    use tokio::io::AsyncReadExt;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let request = tokio::spawn(async move {
        let _ = client.get(format!("https://localhost:{}/", address.port())).send().await;
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).await.unwrap();
    let len = u16::from_be_bytes([header[3], header[4]]) as usize;
    let mut record = header.to_vec();
    record.resize(5 + len, 0);
    stream.read_exact(&mut record[5..]).await.unwrap();
    drop(stream);
    let _ = request.await;
    let old = parse_wire(&record);
    let registry = ShapeRegistry::builtin().unwrap();
    let new = capture(registry.get("default").unwrap().tls_for("direct", false));
    assert_eq!(old.suites, new.suites);
    let mut old_ids: Vec<_> = old.extensions.iter().map(|e| e.0).collect();
    let mut new_ids: Vec<_> = new.extensions.iter().map(|e| e.0).collect();
    old_ids.sort_unstable();
    new_ids.sort_unstable();
    assert_eq!(old_ids, new_ids);
    for typ in [10, 11, 13, 16, 43, 45] {
        assert_eq!(extension(&old, typ), extension(&new, typ), "extension {typ}");
    }
}

#[test]
fn invalid_registry_documents_fail_with_field_context() {
    let default = include_str!("data/default.json");
    let duplicate = ShapeRegistry::from_json_sources(&[default, default]).err().unwrap();
    assert!(duplicate.to_string().contains("name"));
    let missing = ShapeRegistry::from_json_sources(&[]).err().unwrap();
    assert!(missing.to_string().contains("default"));
    let invalid = default.replace("\"session_tickets\": true", "\"cipher_suites\": [\"0xdead\"], \"session_tickets\": true");
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
