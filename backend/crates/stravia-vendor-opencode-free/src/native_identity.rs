use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, PluginError, ProviderSnapshot};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub(crate) fn apply(
    provider: &ProviderSnapshot,
    headers: &mut stravia_vendor_sdk::net::Fields,
) -> Result<(), PluginError> {
    let mut ids = NativeIds::now()?;
    let session = common::session_affinity(provider)
        .map(stable_session)
        .unwrap_or_else(|| ids.create("ses_", true));
    // 官方 request header 指向 user message，不是 session。SDK 没有可信的
    // message 级亲和键，故每次操作新建，不能拿 session/root/request_id 代替。
    let message = ids.create("msg_", false);

    crate::set_header(headers, "user-agent", crate::CLIENT_USER_AGENT.to_owned());
    crate::set_header(headers, "x-opencode-client", "cli".into());
    crate::set_header(headers, "x-opencode-project", "global".into());
    crate::set_header(headers, "x-opencode-session", session);
    crate::set_header(headers, "x-opencode-request", message);
    Ok(())
}

fn stable_session(affinity: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"stravia/opencode-free/session\0");
    hash.update(affinity.as_bytes());
    let digest = hash.finalize();
    // 官方默认 ses_ + 12 hex + 14 base62；客户端只校验 startsWith("ses")。
    // 固定亲和键不能同时编码真实创建时间：这里仅保留表示形态，不声称时间排序。
    // 全部 UTF-8 进入散列，不 trim/截断/hex decode，也不读写映射表。
    let mut id = String::with_capacity(30);
    id.push_str("ses_");
    for byte in &digest[..6] {
        write!(&mut id, "{byte:02x}").expect("writing to String");
    }
    for byte in &digest[6..20] {
        id.push(char::from(BASE62[usize::from(*byte) % BASE62.len()]));
    }
    id
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
        let bytes = random.as_bytes();
        // 排除 UUID 固定版本/变体字节后仍有 14 个随机字节。
        let mut tail = [0_u8; 14];
        let mut index = 0;
        for (position, byte) in bytes.iter().enumerate() {
            if position != 6 && position != 8 {
                tail[index] = *byte;
                index += 1;
            }
        }
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
    fn stable_session_vectors_have_native_shape_without_state_or_clock() {
        for (source, expected) in [
            ("chain-a", "ses_a69bb144810eqRpK7VboGRZCr8"),
            ("主体/Target/链路\0\r\n🙂", "ses_2d6e2675e3a3B8feCq4rWYOa0e"),
        ] {
            let id = stable_session(source);
            assert_eq!(id, expected);
            assert_eq!(id.len(), 30);
            assert!(id[4..16].bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(id[16..].bytes().all(|byte| BASE62.contains(&byte)));
        }
    }

    #[test]
    fn stable_session_hashes_complete_unmodified_utf8() {
        let prefix = "共同前缀".repeat(1024);
        for (a, b) in [
            (format!("{prefix}a"), format!("{prefix}b")),
            ("chain".into(), " chain".into()),
            ("chain".into(), "chain\r\n".into()),
            ("ABCDEF".into(), "abcdef".into()),
            ("é".into(), "e\u{301}".into()),
            ("主体/target-a/root".into(), "主体/target-b/root".into()),
            ("主体/target/root-a".into(), "主体/target/root-b".into()),
        ] {
            assert_ne!(stable_session(&a), stable_session(&b));
        }
    }

    #[test]
    fn messages_remain_fresh_even_with_same_session_and_clock() {
        let session = stable_session("chain-a");
        let mut first = NativeIds {
            timestamp_ms: 1,
            counter: 0,
        };
        let mut reloaded = NativeIds {
            timestamp_ms: 1,
            counter: 0,
        };
        let a = first.create("msg_", false);
        let b = reloaded.create("msg_", false);
        assert!(a.starts_with("msg_000000001001"));
        assert!(b.starts_with("msg_000000001001"));
        assert_ne!(a, b);
        assert_ne!(a, session);
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
}
