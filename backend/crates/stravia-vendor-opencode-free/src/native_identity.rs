use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use stravia_vendor_common::common;
use stravia_vendor_sdk::{ErrorKind, Operation, PluginError, ProviderSnapshot};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub(crate) fn apply(
    provider: &ProviderSnapshot,
    headers: &mut stravia_vendor_sdk::net::Fields,
    operation: Operation,
) -> Result<(), PluginError> {
    let (session, message) = if operation == Operation::Infer {
        let (conversation, conversation_ms) = identity_fact(
            provider,
            "client_conversation_id",
            "client_conversation_created_at_ms",
        )?;
        let (prompt, prompt_ms) =
            identity_fact(provider, "user_prompt_id", "user_prompt_created_at_ms")?;
        // 一个新虚拟会话先创建 session、再创建 message；续接只创建新的 message。
        // UUID 提供持久随机部分，宿主创建时刻提供 native 时间位；重试和工具循环不重新分配身份。
        let message_counter = if conversation == prompt && conversation_ms == prompt_ms {
            2
        } else {
            1
        };
        (
            encode_id(
                "ses_",
                conversation_ms,
                1,
                true,
                &random_tail(conversation.as_bytes()),
            ),
            encode_id(
                "msg_",
                prompt_ms,
                message_counter,
                false,
                &random_tail(prompt.as_bytes()),
            ),
        )
    } else {
        let mut ids = NativeIds::now()?;
        (ids.create("ses_", true), ids.create("msg_", false))
    };

    crate::set_header(headers, "User-Agent", crate::CLIENT_USER_AGENT.to_owned());
    crate::set_header(headers, "x-opencode-client", "cli".into());
    crate::set_header(headers, "x-opencode-project", "global".into());
    crate::set_header(headers, "x-opencode-request", message);
    crate::set_header(headers, "x-opencode-session", session);
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
    fn messages_remain_fresh_even_with_same_session_and_clock() {
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
