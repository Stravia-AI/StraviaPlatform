use super::*;

const CATALOG: &str = r#"{
  "schema_version":2,"repository_id":42,"release_tag":"v1.0.0",
  "generated_at":"2026-01-01T00:00:00Z",
  "releases":[{
    "vendor_id":"example","version":"1.0.0","display_name":"Example",
    "published_at":"2026-01-01T00:00:00Z",
    "compatibility":{"host_version_req":">=1.0.0","wit_version":"1.0.0","canonical_format_version":1},
    "artifact":{"filename":"example.wasm.zst","compression":"zstd","media_type":"application/zstd","size":10,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
    "component":{"size":20,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}
  }]
}"#;

#[test]
fn catalog_rejects_duplicates_unknown_keys_and_invalid_values() {
    assert!(parse_catalog(CATALOG.as_bytes()).is_ok());
    let mut duplicate: serde_json::Value = serde_json::from_str(CATALOG).unwrap();
    let releases = duplicate["releases"].as_array_mut().unwrap();
    releases.push(releases[0].clone());
    assert!(parse_catalog(serde_json::to_vec(&duplicate).unwrap().as_slice()).is_err());
    for bad in [
        CATALOG.replace(
            "\"schema_version\":2",
            "\"schema_version\":2,\"schema_version\":2",
        ),
        CATALOG.replace(
            "\"vendor_id\":\"example\"",
            "\"vendor_id\":\"example\",\"unknown\":true",
        ),
        CATALOG.replace("\"version\":\"1.0.0\"", "\"version\":\"v1.0.0\""),
        CATALOG.replace("example.wasm.zst", "../example.wasm.zst"),
        CATALOG.replace(
            "\"host_version_req\":\">=1.0.0\"",
            "\"host_version_req\":\"not semver\"",
        ),
        CATALOG.replace("\"vendor_id\":\"example\"", "\"vendor_id\":\"base\""),
    ] {
        assert!(parse_catalog(bad.as_bytes()).is_err(), "accepted {bad}");
    }
}

#[test]
fn catalog_requires_authentic_signature_before_parsing() {
    let key = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    let signature = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1633700835\tfile:test\tprehashed\nwLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";
    // This published Minisign vector signs "test": it authenticates, then fails JSON parsing.
    assert!(
        verify_catalog(b"test", signature, key)
            .unwrap_err()
            .to_string()
            .contains("catalog JSON")
    );
    assert!(
        verify_catalog(CATALOG.as_bytes(), signature, key)
            .unwrap_err()
            .to_string()
            .contains("signature verification")
    );
    assert_eq!(
        public_key_fingerprint(key).unwrap(),
        public_key_fingerprint(&format!("untrusted comment: key\n{key}\n")).unwrap()
    );
}

#[test]
fn bounded_component_codec_rejects_wrong_digest_and_non_single_frames() {
    let bytes = b"a real component would have these bytes";
    let compressed = encode_component(bytes).unwrap();
    let expected = ComponentDigest {
        size: bytes.len() as u64,
        sha256: "8d83b79d4b6c9f0aab0fc055de56f414f23d48b8e0df6e2fdcaabb241173351f".into(),
    };
    assert_eq!(
        decode_component(&compressed, Some(&expected)).unwrap(),
        bytes
    );
    let wrong = ComponentDigest {
        size: expected.size - 1,
        sha256: expected.sha256.clone(),
    };
    assert!(decode_component(&compressed, Some(&wrong)).is_err());
    let wrong_hash = ComponentDigest {
        size: expected.size,
        sha256: "0".repeat(64),
    };
    assert!(decode_component(&compressed, Some(&wrong_hash)).is_err());
    let mut trailing = compressed.clone();
    trailing.push(0);
    assert!(decode_component(&trailing, None).is_err());
    let mut concatenated = compressed.clone();
    concatenated.extend_from_slice(&compressed);
    assert!(decode_component(&concatenated, None).is_err());
    assert!(decode_component(&compressed[..compressed.len() - 1], None).is_err());
    assert!(decode_component(&[0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0], None).is_err());
    let mut dictionary = compressed;
    dictionary[4] |= 1;
    assert!(decode_component(&dictionary, None).is_err());
}
