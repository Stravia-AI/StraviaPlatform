use super::*;
use crate::detection::{detector, test_text};

fn matched(rule: &str, text: &str) -> Vec<String> {
    let detector = detector().unwrap();
    let index = detector
        .catalog
        .rules
        .iter()
        .position(|item| item.id == rule)
        .unwrap();
    detector
        .find(text, index)
        .unwrap()
        .into_iter()
        .map(|finding| finding.secret.to_owned())
        .collect()
}

#[test]
fn complete_snapshot_compiles_and_hidden_context_never_becomes_a_secret() {
    let detector = detector().unwrap();
    let imported: Vec<_> = detector
        .catalog
        .rules
        .iter()
        .filter(|rule| rule.id.starts_with("kingfisher."))
        .collect();
    assert_eq!(imported.len(), 1013);
    assert_eq!(
        imported.iter().filter(|rule| !rule.skip_report).count(),
        861
    );
    assert_eq!(detector.catalog.rules.len(), 1475);
    assert!(
        detector
            .detect(&["https://tenant.azure-api.net ordinary text"])
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bare_ai_credentials_and_bearer_headers_have_exact_unicode_ranges() {
    // Synthetic fixtures of the reported shapes, never the user's credentials.
    let first = format!("sk-{}", "m7q2b9v4x0k6n3r8s1t5w9y2z4c6d8f0g3h5j7l1p2a4e6u8");
    let second = format!(
        "{}.{}",
        "7b2d9f4a0c6e3a8b1d5f9c2e4a6b8d0f", "Q7m2Z9v4K0r6T3x8"
    );
    for text in [
        format!("中文😀 {first} {second}"),
        format!("Authorization: Bearer {first}\nAuthorization: Bearer {second}"),
        format!("{first}\n{second}\n{first}"),
    ] {
        let matches = test_text(text.clone()).await.unwrap();
        let utf16: Vec<_> = text.encode_utf16().collect();
        for (id, secret) in [
            ("kingfisher.openai.1", &first),
            ("kingfisher.zhipu.1", &second),
        ] {
            let selected: Vec<_> = matches.iter().filter(|found| found.rule_id == id).collect();
            assert_eq!(selected.len(), text.matches(secret).count(), "{id}");
            for found in selected {
                assert_eq!(
                    String::from_utf16(&utf16[found.start..found.end]).unwrap(),
                    *secret
                );
            }
        }
        let detected = detector().unwrap().detect(&[&text]).unwrap();
        assert_eq!(
            detected.iter().filter(|item| item.secret == first).count(),
            1
        );
        assert_eq!(
            detected.iter().filter(|item| item.secret == second).count(),
            1
        );
    }
}

#[test]
fn zhipu_rejects_wrong_lengths_low_entropy_and_does_not_join_fields() {
    let good = "7b2d9f4a0c6e3a8b1d5f9c2e4a6b8d0f.Q7m2Z9v4K0r6T3x8";
    assert_eq!(matched("kingfisher.zhipu.1", good), vec![good]);
    for bad in [
        good[..48].to_owned(),
        format!("{good}9"),
        format!("{}.{}", "a".repeat(32), "b".repeat(16)),
    ] {
        assert!(matched("kingfisher.zhipu.1", &bad).is_empty());
    }
    assert!(
        detector()
            .unwrap()
            .detect(&[&good[..32], &good[32..]])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn capture_priority_and_ascii_boundaries_follow_kingfisher() {
    let regex = RegexBuilder::new(r"(?P<host>host) (?P<TOKEN>[A-Z0-9]+)|(?P<fallback>other)")
        .unicode(false)
        .build()
        .unwrap();
    let captures = regex.captures(b"host A7B9").unwrap();
    assert_eq!(
        secret_capture(&regex, &captures).unwrap().as_bytes(),
        b"A7B9"
    );
    let captures = regex.captures(b"other").unwrap();
    assert_eq!(
        secret_capture(&regex, &captures).unwrap().as_bytes(),
        b"other"
    );
    let key = "sk-m7q2b9v4x0k6n3r8s1t5w9y2z4c6d8f0g3h5j7l1p2a4e6u8";
    assert_eq!(
        matched("kingfisher.openai.1", &format!("中文{key}中文")),
        vec![key]
    );
}

#[test]
fn requirements_use_full_match_with_named_groups_and_secret_entropy() {
    let regex = Regex::new(r"(?P<host>[A-Z0-9]+):(?P<TOKEN>[a-z]+)").unwrap();
    let captures = regex.captures(b"AZ12:entropy").unwrap();
    let conditions = Conditions {
        min_entropy: 0.0,
        requirements: Requirements {
            min_digits: 2,
            min_uppercase: 2,
            min_lowercase: 3,
            ..Default::default()
        },
        checksum: None,
    };
    assert!(
        conditions
            .accepts(
                &regex,
                &captures,
                captures.get(0).unwrap(),
                captures.name("TOKEN").unwrap()
            )
            .unwrap()
    );
    assert_eq!(entropy(b"AAAA"), 0.0);
    assert_eq!(entropy(b"ABAB"), 1.0);
}

#[test]
fn all_checksum_transforms_match_external_vectors_and_reject_mutation() {
    // CRC-32 and SHA-256 vectors for "hello", independent of the detector's implementation.
    for (expected, actual) in [
        ("{{ body | crc32 | base62: 6 }}", "0zNvy2"),
        ("{{ body | crc32_le_b64: 6 }}", "hqYQNg"),
        ("{{ body | sha256_b32: 8 }}", "FTZE3OS7"),
        ("{{ BODY | crc32_hex }}", "3610a686"),
    ] {
        let source = ChecksumSource {
            actual: ChecksumActual {
                template: "{{ checksum }}".into(),
                requires_capture: Some("checksum".into()),
            },
            expected: expected.into(),
            skip_if_missing: false,
        };
        let checksum = Checksum::compile(&source).unwrap();
        let regex = Regex::new(r"(?P<body>hello):(?P<checksum>\w+)").unwrap();
        let text = format!("hello:{actual}");
        assert!(
            checksum
                .matches(&regex, &regex.captures(text.as_bytes()).unwrap())
                .unwrap(),
            "{expected}"
        );
        assert!(
            !checksum
                .matches(&regex, &regex.captures(b"hello:00000000").unwrap())
                .unwrap()
        );
    }
    assert_eq!(crc32(b"123456789"), 0xcbf43926);
    assert_eq!(
        radix(123456, b"0123456789abcdefghijklmnopqrstuvwxyz", 7),
        "0002n9c"
    );
}

#[test]
fn unknown_checksum_programs_fail_closed_at_compile_time() {
    let source = ChecksumSource {
        actual: ChecksumActual {
            template: "{{ checksum }}".into(),
            requires_capture: None,
        },
        expected: "{{ body | unsupported }}".into(),
        skip_if_missing: false,
    };
    assert!(Checksum::compile(&source).is_err());
}

#[test]
fn gitlab_checksum_and_optional_checksum_captures_follow_upstream() {
    let source = ChecksumSource {
        actual: ChecksumActual { template: "{{ crc32 }}".into(), requires_capture: Some("crc32".into()) },
        expected: "{{ \"glpat-\" | append: base64_payload | append: \".01.\" | append: base36_payload_length | crc32 | base36: 7 }}".into(),
        skip_if_missing: true,
    };
    let checksum = Checksum::compile(&source).unwrap();
    let regex = Regex::new(
        r"(?P<base64_payload>payload):(?P<base36_payload_length>7)(?::(?P<crc32>[a-z0-9]+))?",
    )
    .unwrap();
    // Computed independently with Python's zlib.crc32 over glpat-payload.01.7.
    assert!(
        checksum
            .matches(&regex, &regex.captures(b"payload:7:041c9xh").unwrap())
            .unwrap()
    );
    assert!(
        !checksum
            .matches(&regex, &regex.captures(b"payload:7:041c9xi").unwrap())
            .unwrap()
    );
    let mut conditions = Conditions {
        min_entropy: 0.0,
        requirements: Requirements {
            checksum: Some(source),
            ..Default::default()
        },
        checksum: Some(checksum),
    };
    let captures = regex.captures(b"payload:7").unwrap();
    let full = captures.get(0).unwrap();
    assert!(conditions.accepts(&regex, &captures, full, full).unwrap());
    conditions
        .requirements
        .checksum
        .as_mut()
        .unwrap()
        .skip_if_missing = false;
    assert!(!conditions.accepts(&regex, &captures, full, full).unwrap());
}

#[test]
fn character_requirements_and_ignore_terms_are_enforced() {
    let regex = Regex::new(r"(.+)").unwrap();
    let conditions = Conditions {
        min_entropy: 0.0,
        requirements: Requirements {
            min_digits: 2,
            min_uppercase: 1,
            min_lowercase: 1,
            min_special_chars: 1,
            special_chars: Some("@".into()),
            ignore_if_contains: vec!["".into(), "  Example  ".into()],
            ..Default::default()
        },
        checksum: None,
    };
    for (text, accepted) in [
        ("Az12@", true),
        ("Az1@", false),
        ("az12@", false),
        ("AZ12@", false),
        ("Az12!", false),
        ("Az12@EXAMPLE", false),
    ] {
        let captures = regex.captures(text.as_bytes()).unwrap();
        let full = captures.get(0).unwrap();
        assert_eq!(
            conditions.accepts(&regex, &captures, full, full).unwrap(),
            accepted
        );
    }
    let example = format!("sk-{}{}", "123456789", "q7m2z9v4k0r6t3x8".repeat(3));
    assert!(matched("kingfisher.openai.1", &example).is_empty());
}

#[test]
fn inline_ignore_comments_do_not_disable_protection() {
    let key = "sk-m7q2b9v4x0k6n3r8s1t5w9y2z4c6d8f0g3h5j7l1p2a4e6u8";
    for comment in ["kingfisher:ignore", "gitleaks:allow", "betterleaks:allow"] {
        assert_eq!(
            matched("kingfisher.openai.1", &format!("{key} # {comment}")),
            vec![key]
        );
    }
}

#[tokio::test]
async fn references_are_atomic_and_do_not_hide_neighboring_credentials() {
    let reference = "~stravia-secret:65ecadffe021442ab611243aca232c08~";
    assert!(test_text(reference.into()).await.unwrap().is_empty());
    let key = "7b2d9f4a0c6e3a8b1d5f9c2e4a6b8d0f.Q7m2Z9v4K0r6T3x8";
    let text = format!("{reference} {key} {reference}");
    let matches = test_text(text.clone()).await.unwrap();
    assert!(
        matches
            .iter()
            .any(|found| found.rule_id == "kingfisher.zhipu.1")
    );
    assert!(
        matches
            .iter()
            .all(|found| &text[found.start..found.end] == key)
    );
}
