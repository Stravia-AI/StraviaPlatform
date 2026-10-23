//! 音频理解的输入边界：只校验 MIME 与容器魔数，不解码、不转码。
//! 音频字节原样作为 Artifact 交给音频理解模型；具体 Target 能否接受某种封装由传输层按协议判定。

/// 单个音频源的字节上限。
pub const MAX_AUDIO_SOURCE_BYTES: u64 = 20 * 1024 * 1024;
/// 单次 Turn 的音频字节合计上限，与图片 Turn 的合计上限一致。
pub const MAX_AUDIO_TURN_BYTES: u64 = 20 * 1024 * 1024;

/// 一种受支持的音频容器：同一容器的别名 MIME 与其魔数检测。
struct AudioFormat {
    mimes: &'static [&'static str],
    matches: fn(&[u8]) -> bool,
}

impl AudioFormat {
    fn accepts(&self, base_mime: &str) -> bool {
        self.mimes
            .iter()
            .any(|mime| mime.eq_ignore_ascii_case(base_mime))
    }
}

const FORMATS: &[AudioFormat] = &[
    AudioFormat {
        mimes: &["audio/wav", "audio/x-wav", "audio/wave", "audio/vnd.wave"],
        matches: is_wav,
    },
    AudioFormat {
        mimes: &["audio/mpeg", "audio/mp3"],
        matches: is_mp3,
    },
    AudioFormat {
        mimes: &["audio/flac", "audio/x-flac"],
        matches: is_flac,
    },
    AudioFormat {
        mimes: &["audio/ogg"],
        matches: is_ogg,
    },
    AudioFormat {
        mimes: &["audio/aac"],
        matches: is_aac,
    },
    AudioFormat {
        mimes: &["audio/aiff", "audio/x-aiff"],
        matches: is_aiff,
    },
];

/// Agent Definition 的 Artifact 策略白名单。
pub fn allowed_mime_types() -> Vec<String> {
    FORMATS
        .iter()
        .flat_map(|format| format.mimes.iter().map(|mime| (*mime).to_owned()))
        .collect()
}

fn base_mime(mime_type: &str) -> &str {
    mime_type.split(';').next().unwrap_or_default().trim()
}

pub fn is_supported_audio_mime(mime_type: &str) -> bool {
    let base = base_mime(mime_type);
    FORMATS.iter().any(|format| format.accepts(base))
}

/// 声明的 MIME 必须与实际容器魔数一致，避免把任意字节标成音频送给模型。
pub fn bytes_match_mime(mime_type: &str, bytes: &[u8]) -> bool {
    let base = base_mime(mime_type);
    FORMATS
        .iter()
        .find(|format| format.accepts(base))
        .is_some_and(|format| (format.matches)(bytes))
}

fn is_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE"
}

fn is_mp3(bytes: &[u8]) -> bool {
    bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0 && bytes[1] & 0x06 != 0)
}

fn is_flac(bytes: &[u8]) -> bool {
    bytes.starts_with(b"fLaC")
}

fn is_ogg(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS")
}

/// ADTS 帧同步：12 位同步字且 layer 位为 0（与 MP3 的 layer 位区分）。
fn is_aac(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xF6 == 0xF0
}

fn is_aiff(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"FORM" && matches!(&bytes[8..12], b"AIFF" | b"AIFC")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav() -> Vec<u8> {
        let mut bytes = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
        bytes.extend_from_slice(&[0; 16]);
        bytes
    }

    #[test]
    fn container_magic_must_match_declared_mime() {
        assert!(bytes_match_mime("audio/wav", &wav()));
        assert!(bytes_match_mime("audio/x-wav; charset=binary", &wav()));
        assert!(!bytes_match_mime("audio/mpeg", &wav()));
        assert!(!bytes_match_mime("audio/wav", b"not audio at all"));
        assert!(bytes_match_mime("audio/mpeg", b"ID3\x04\x00\x00"));
        assert!(bytes_match_mime("audio/mpeg", &[0xFF, 0xFB, 0x90, 0x00]));
        assert!(
            !bytes_match_mime("audio/mpeg", &[0xFF, 0xF1, 0x50, 0x80]),
            "ADTS header must not pass as MP3"
        );
        assert!(bytes_match_mime("audio/aac", &[0xFF, 0xF1, 0x50, 0x80]));
        assert!(bytes_match_mime("audio/flac", b"fLaC\x00\x00"));
        assert!(bytes_match_mime("audio/ogg", b"OggS\x00\x02"));
        assert!(bytes_match_mime("audio/aiff", b"FORM\x00\x00\x00\x00AIFF"));
    }

    #[test]
    fn unsupported_audio_containers_are_rejected_by_mime() {
        assert!(is_supported_audio_mime("audio/mpeg"));
        assert!(!is_supported_audio_mime("audio/midi"));
        assert!(!is_supported_audio_mime("video/mp4"));
        assert!(!bytes_match_mime("audio/midi", b"MThd"));
    }
}
