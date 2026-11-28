use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, Operation, PluginError, ProviderSnapshot};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const PROJECT_DOMAIN: &[u8] = b"stravia-opencode-project-v1\0";

pub(crate) fn apply(
    provider: &ProviderSnapshot,
    headers: &mut stravia_vendor_sdk::net::Fields,
    operation: Operation,
) -> Result<(), PluginError> {
    let (session, project, trace_id) = if operation == Operation::Infer {
        let (conversation, conversation_ms) = identity_fact(
            provider,
            "client_conversation_id",
            "client_conversation_created_at_ms",
        )?;
        let (prompt, _prompt_ms) =
            identity_fact(provider, "user_prompt_id", "user_prompt_created_at_ms")?;
        // 会话 ID 与 project ID 跟随虚拟会话（跨重启与工具循环保持一致）；
        // OpenCode 2.4.2 移除 x-opencode-request，改为按用户提示发 W3C traceparent / b3
        //（同一用户提示的重试与工具循环共享 trace_id，每次 HTTP 调用生成新的 span_id）
        // 以及 4 个同值会话头。
        (
            encode_id(
                "ses_",
                conversation_ms,
                1,
                true,
                &random_tail(conversation.as_bytes()),
            ),
            project_id_for_uuid(conversation),
            trace_id_for_uuid(prompt),
        )
    } else {
        let mut ids = NativeIds::now()?;
        let ephemeral = uuid::Uuid::new_v4();
        (
            ids.create("ses_", true),
            project_id_for_uuid(ephemeral),
            trace_id_for_uuid(ephemeral),
        )
    };
    let (span_id, parent_span_id) = fresh_span_pair();
    let b3 = format!("{trace_id}-{span_id}-1-{parent_span_id}");
    let traceparent = format!("00-{trace_id}-{span_id}-01");

    // 顺序与官方 OpenCode 2.4.2（Bun 1.4.4 Fetch）两次独立抓包的线上头序一致。
    crate::set_header(headers, "User-Agent", crate::CLIENT_USER_AGENT.to_owned());
    crate::set_header(headers, "b3", b3);
    crate::set_header(headers, "traceparent", traceparent);
    crate::set_header(headers, "x-opencode-client", "cli".into());
    crate::set_header(headers, "x-opencode-project", project);
    crate::set_header(headers, "x-opencode-session", session.clone());
    crate::set_header(headers, "x-opencode-session-id", session.clone());
    crate::set_header(headers, "x-session-affinity", session.clone());
    crate::set_header(headers, "x-session-id", session);
    Ok(())
}

fn identity_fact(
    provider: &ProviderSnapshot,
    identity_key: &str,
    timestamp_key: &str,
) -> Result<(uuid::Uuid, u64), PluginError> {
    let invalid = |key| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("OpenCode inference requires valid host {key}"),
        )
    };
    let source = provider
        .operation_metadata
        .get(identity_key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid(identity_key))?;
    let identity = uuid::Uuid::parse_str(source).map_err(|_| invalid(identity_key))?;
    let mut canonical = [0; 36];
    if identity.hyphenated().encode_lower(&mut canonical) != source {
        return Err(invalid(identity_key));
    }
    let timestamp_ms = provider
        .operation_metadata
        .get(timestamp_key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid(timestamp_key))?;
    Ok((identity, timestamp_ms))
}

fn random_tail(bytes: &[u8; 16]) -> [u8; 14] {
    // UUID 的版本/变体字节不具备完整的均匀随机性。
    let mut tail = [0; 14];
    tail[..6].copy_from_slice(&bytes[..6]);
    tail[6] = bytes[7];
    tail[7..].copy_from_slice(&bytes[9..]);
    tail
}

fn trace_id_for_uuid(id: uuid::Uuid) -> String {
    // W3C Trace Context 禁止全 0 trace-id；宿主 UUIDv4 非零，但防御 nil 输入。
    let raw = id.as_u128();
    let non_zero = if raw == 0 { 1 } else { raw };
    format!("{non_zero:032x}")
}

fn fresh_span_pair() -> (String, String) {
    // UUIDv4 第 6 字节含版本位 0x40、第 8 字节含变体位 0x80，前后各 8 字节必然非零。
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let span = u64::from_be_bytes(bytes[..8].try_into().expect("8 bytes"));
    let parent = u64::from_be_bytes(bytes[8..].try_into().expect("8 bytes"));
    (format!("{span:016x}"), format!("{parent:016x}"))
}

fn project_id_for_uuid(id: uuid::Uuid) -> String {
    let mut input = [0u8; PROJECT_DOMAIN.len() + 16];
    input[..PROJECT_DOMAIN.len()].copy_from_slice(PROJECT_DOMAIN);
    input[PROJECT_DOMAIN.len()..].copy_from_slice(id.as_bytes());
    let digest = sha1_digest(&input);
    let mut hex = String::with_capacity(40);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to String");
    }
    hex
}

// 最小 RFC 3174 SHA-1，仅用于把会话 UUID 映射为官方 40 位 hex `x-opencode-project` 形态，
// 避免在 Wasm 插件引入额外加密 crate 依赖。
fn sha1_digest(message: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0];
    let bit_len = (message.len() as u64) * 8;
    let padded_len = (message.len() + 9).div_ceil(64) * 64;
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(message);
    padded.push(0x80);
    padded.resize(padded_len - 8, 0);
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in padded.as_chunks::<64>().0 {
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdcu32),
                _ => (b ^ c ^ d, 0xca62c1d6u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

struct NativeIds {
    timestamp_ms: u64,
    counter: u16,
}

impl NativeIds {
    fn now() -> Result<Self, PluginError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|err| {
                common::plugin_error(
                    ErrorKind::Trapped,
                    format!("read OpenCode native identity UTC clock: {err}"),
                )
            })?;
        let timestamp_ms = u64::try_from(elapsed.as_millis()).map_err(|err| {
            common::plugin_error(
                ErrorKind::Trapped,
                format!("encode OpenCode native identity UTC milliseconds: {err}"),
            )
        })?;
        Ok(Self {
            timestamp_ms,
            counter: 0,
        })
    }

    fn create(&mut self, prefix: &str, descending: bool) -> String {
        // 官方计数从 1 开始；随机尾部负责跨操作去重。
        self.counter += 1;
        let random = uuid::Uuid::new_v4();
        let tail = random_tail(random.as_bytes());
        encode_id(prefix, self.timestamp_ms, self.counter, descending, &tail)
    }
}

fn encode_id(
    prefix: &str,
    timestamp_ms: u64,
    counter: u16,
    descending: bool,
    random: &[u8; 14],
) -> String {
    let current = u128::from(timestamp_ms) * 4096 + u128::from(counter);
    let value = if descending { !current } else { current };
    let mut id = String::with_capacity(prefix.len() + 26);
    id.push_str(prefix);
    for shift in [40, 32, 24, 16, 8, 0] {
        write!(&mut id, "{:02x}", (value >> shift) & 0xff).expect("writing to String");
    }
    for byte in random {
        id.push(char::from(BASE62[usize::from(*byte) % BASE62.len()]));
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn official_timestamp_vectors_preserve_direction_counter_and_low_48_bits() {
        let random = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        for (timestamp, counter, ascending, descending) in [
            (0, 1, "000000000001", "fffffffffffe"),
            (1, 1, "000000001001", "ffffffffeffe"),
            (1, 2, "000000001002", "ffffffffeffd"),
            (0x123456789ab, 1, "3456789ab001", "cba987654ffe"),
        ] {
            assert_eq!(
                encode_id("msg_", timestamp, counter, false, &random),
                format!("msg_{ascending}0123456789ABCD")
            );
            assert_eq!(
                encode_id("ses_", timestamp, counter, true, &random),
                format!("ses_{descending}0123456789ABCD")
            );
        }
    }

    #[test]
    fn sha1_matches_rfc3174_test_vectors() {
        let hex = |bytes: [u8; 20]| {
            let mut s = String::with_capacity(40);
            for b in bytes {
                write!(&mut s, "{b:02x}").unwrap();
            }
            s
        };
        assert_eq!(
            hex(sha1_digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(sha1_digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn absent_affinity_uses_new_descending_sessions() {
        let mut first = NativeIds {
            timestamp_ms: 1,
            counter: 0,
        };
        let mut reloaded = NativeIds {
            timestamp_ms: 1,
            counter: 0,
        };
        let a = first.create("ses_", true);
        let b = reloaded.create("ses_", true);
        assert!(a.starts_with("ses_ffffffffeffe"));
        assert!(b.starts_with("ses_ffffffffeffe"));
        assert_ne!(a, b);
    }

    #[test]
    fn infer_headers_match_v2_order_trace_consistency_and_session_fanout() {
        let provider = ProviderSnapshot {
            provider_id: "opencode-free".into(),
            channel: "default".into(),
            base_url: crate::DEFAULT_BASE_URL.into(),
            protocol: "openai-compatible".into(),
            options: BTreeMap::new(),
            credentials: BTreeMap::new(),
            model: Some("big-pickle".into()),
            model_metadata: None,
            client_headers: Vec::new(),
            operation_metadata: BTreeMap::from([
                (
                    "client_conversation_id".into(),
                    serde_json::json!("00000000-0000-4000-8000-000000000001"),
                ),
                (
                    "client_conversation_created_at_ms".into(),
                    serde_json::json!(1700000000000_u64),
                ),
                (
                    "user_prompt_id".into(),
                    serde_json::json!("00000000-0000-4000-8000-000000000002"),
                ),
                (
                    "user_prompt_created_at_ms".into(),
                    serde_json::json!(1700000001000_u64),
                ),
            ]),
        };
        let mut fields = stravia_vendor_sdk::net::Fields::new();
        apply(&provider, &mut fields, Operation::Infer).unwrap();
        let pairs: Vec<(String, String)> = fields
            .into_vec()
            .into_iter()
            .map(|f| (f.name, String::from_utf8(f.bytes).unwrap()))
            .collect();
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "User-Agent",
                "b3",
                "traceparent",
                "x-opencode-client",
                "x-opencode-project",
                "x-opencode-session",
                "x-opencode-session-id",
                "x-session-affinity",
                "x-session-id",
            ]
        );
        let map: BTreeMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map["User-Agent"], crate::CLIENT_USER_AGENT);
        assert_eq!(map["x-opencode-client"], "cli");
        assert!(!map.contains_key("x-opencode-request"));
        assert_eq!(map["x-opencode-project"].len(), 40);
        assert!(
            map["x-opencode-project"]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        let session = &map["x-opencode-session"];
        assert!(session.starts_with("ses_"));
        assert_eq!(session.len(), 30);
        assert_eq!(&map["x-opencode-session-id"], session);
        assert_eq!(&map["x-session-affinity"], session);
        assert_eq!(&map["x-session-id"], session);

        let b3_parts: Vec<&str> = map["b3"].split('-').collect();
        let tp_parts: Vec<&str> = map["traceparent"].split('-').collect();
        assert_eq!(b3_parts.len(), 4);
        assert_eq!(tp_parts.len(), 4);
        assert_eq!(tp_parts[0], "00");
        assert_eq!(tp_parts[3], "01");
        assert_eq!(b3_parts[2], "1");
        assert_eq!(b3_parts[0], "00000000000040008000000000000002");
        assert_eq!(tp_parts[1], b3_parts[0]);
        assert_eq!(tp_parts[2], b3_parts[1]);
        assert_eq!(b3_parts[1].len(), 16);
        assert_eq!(b3_parts[3].len(), 16);
    }
}
