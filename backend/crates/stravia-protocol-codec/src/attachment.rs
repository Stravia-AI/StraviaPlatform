//! 附件交付能力的单一事实源。
//!
//! 每个协议对同一媒体的承载方式不同：部分协议只能内联字节，部分只能引用
//! URL，部分两者皆可；部分协议还只接受特定 MIME。Core 在每次 Target 调用前
//! 只按这张表选择签名下载地址或 base64，跨协议请求裁剪也按同一张表判断媒体
//! 种类能否被目标承载，避免 Core 与 codec 维护两份互相矛盾的矩阵。
//!
//! 表只描述 wire 承载能力，不代替编码器的来源级细节（例如某些协议的原生
//! file id 载体）与角色限制；那些规则仍留在各自的 encoder。

use stravia_runtime_contract::protocol::ids::Protocol;
use stravia_runtime_contract::protocol::ir::ContentBlock;

/// 结构化附件的媒体种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Audio,
    File,
    Video,
    Document,
}

impl AttachmentKind {
    /// 用于错误信息与附件引用的英文名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::File => "file",
            Self::Video => "video",
            Self::Document => "document",
        }
    }

    /// 内容块对应的附件种类；非媒体块返回 `None`。
    pub fn of_block(block: &ContentBlock) -> Option<Self> {
        match block {
            ContentBlock::Image { .. } => Some(Self::Image),
            ContentBlock::Audio { .. } => Some(Self::Audio),
            ContentBlock::File { .. } => Some(Self::File),
            ContentBlock::Video { .. } => Some(Self::Video),
            ContentBlock::Document { .. } => Some(Self::Document),
            _ => None,
        }
    }
}

/// 协议对一类附件的交付方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentDelivery {
    /// 可用 URL（外部地址或签名下载地址）交付。
    pub url: bool,
    /// 可内联字节（base64）交付。
    pub inline: bool,
}

impl AttachmentDelivery {
    const NONE: Self = Self {
        url: false,
        inline: false,
    };
    const INLINE: Self = Self {
        url: false,
        inline: true,
    };
    const BOTH: Self = Self {
        url: true,
        inline: true,
    };

    /// 协议能否以任一方式承载该种类。
    pub const fn carries(self) -> bool {
        self.url || self.inline
    }
}

/// 协议对某类附件的交付方式。
///
/// 取值依据各 encoder 的实际 wire 承载：
/// - Anthropic Messages：图片、PDF 文件与文档可用 URL 或内联；
/// - Google Gemini：五类附件均可用 `fileData` 或 `inlineData`；
/// - OpenAI Compatible：只有图片两者皆可；文件与文档（encoder 都编码为 `file`
///   部件）、音频仅内联，视频无载体。`file` 部件的 `file_url` 不属于 OpenAI Chat
///   Completions 的官方 schema，因此不把签名下载地址当作文件载体；
/// - watsonx Text Chat：复用 chat-completions encoder，但上游文档只定义
///   `image_url` 与 `input_audio` 部件，没有 `file` 部件：图片两者皆可，音频仅
///   内联，其余无载体；
/// - Open Responses：图片、文件、视频与文档两者皆可，音频无载体；
/// - Bedrock Converse：图片、文件与文档仅内联（`bytes` 来源）；
/// - Cohere Chat：仅图片；
/// - Gateway Language Model：图片、文件与视频两者皆可。
pub fn attachment_delivery(protocol: Protocol, kind: AttachmentKind) -> AttachmentDelivery {
    use AttachmentKind::{Audio, Document, File, Image, Video};
    use Protocol::{
        AnthropicMessages, BedrockConverse, CohereChat, GatewayLanguageModel, GoogleGemini,
        OpenAICompatible, OpenResponses, WatsonxTextChat,
    };
    match (protocol, kind) {
        (AnthropicMessages, Image | File | Document) => AttachmentDelivery::BOTH,
        (AnthropicMessages, Audio | Video) => AttachmentDelivery::NONE,
        (GoogleGemini, _) => AttachmentDelivery::BOTH,
        (OpenAICompatible, Image) => AttachmentDelivery::BOTH,
        (OpenAICompatible, Audio | File | Document) => AttachmentDelivery::INLINE,
        (OpenAICompatible, Video) => AttachmentDelivery::NONE,
        (WatsonxTextChat, Image) => AttachmentDelivery::BOTH,
        (WatsonxTextChat, Audio) => AttachmentDelivery::INLINE,
        (WatsonxTextChat, File | Video | Document) => AttachmentDelivery::NONE,
        (OpenResponses, Image | File | Video | Document) => AttachmentDelivery::BOTH,
        (OpenResponses, Audio) => AttachmentDelivery::NONE,
        (BedrockConverse, Image | File | Document) => AttachmentDelivery::INLINE,
        (BedrockConverse, Audio | Video) => AttachmentDelivery::NONE,
        (CohereChat, Image) => AttachmentDelivery::BOTH,
        (CohereChat, Audio | File | Video | Document) => AttachmentDelivery::NONE,
        (GatewayLanguageModel, Image | File | Video) => AttachmentDelivery::BOTH,
        (GatewayLanguageModel, Audio | Document) => AttachmentDelivery::NONE,
    }
}

/// 内联交付是否被协议接受。
///
/// 只有协议实际拒绝的 MIME 才返回 `false`：例如 OpenAI 形状的 `input_audio`
/// 只区分 WAV 与 MP3，Anthropic 与 Bedrock 的图片只接受 JPEG/PNG/GIF/WebP，
/// Anthropic 文档与 Bedrock 文档只接受 PDF。调用方应先确认
/// [`attachment_delivery`] 允许内联。
pub fn inline_media_type_supported(
    protocol: Protocol,
    kind: AttachmentKind,
    media_type: &str,
) -> bool {
    use AttachmentKind::{Audio, Document, File, Image};
    use Protocol::{AnthropicMessages, BedrockConverse, OpenAICompatible, WatsonxTextChat};
    match (protocol, kind) {
        (AnthropicMessages, Image) => matches!(
            media_type,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        ),
        (AnthropicMessages, File) => matches!(media_type, "application/pdf" | "text/plain"),
        (AnthropicMessages, Document) => media_type == "application/pdf",
        (BedrockConverse, Image) => matches!(
            media_type,
            "image/jpeg" | "image/png" | "image/gif" | "image/webp"
        ),
        (BedrockConverse, Document) => media_type == "application/pdf",
        (BedrockConverse, File) => matches!(
            media_type,
            "application/pdf"
                | "text/csv"
                | "application/csv"
                | "application/msword"
                | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                | "application/vnd.ms-excel"
                | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                | "text/html"
                | "text/plain"
                | "text/markdown"
                | "text/x-markdown"
        ),
        (OpenAICompatible | WatsonxTextChat, Audio) => matches!(
            media_type,
            "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/mp3" | "audio/mpeg"
        ),
        _ => true,
    }
}

/// URL 交付是否被协议接受。
///
/// Anthropic 的 `document` 部件只接受 PDF URL；其余协议的 URL 部件不区分 MIME。
/// 调用方应先确认 [`attachment_delivery`] 允许 URL。
pub fn url_media_type_supported(
    protocol: Protocol,
    kind: AttachmentKind,
    media_type: &str,
) -> bool {
    match (protocol, kind) {
        (Protocol::AnthropicMessages, AttachmentKind::File | AttachmentKind::Document) => {
            media_type == "application/pdf"
        }
        _ => true,
    }
}

/// 媒体块能否按 [`attachment_delivery`] 的种类能力被目标承载。
///
/// 只判断种类层面；来源变体、MIME 与角色限制由调用方保留。
pub fn attachment_block_supported(protocol: Protocol, block: &ContentBlock) -> bool {
    AttachmentKind::of_block(block)
        .is_some_and(|kind| attachment_delivery(protocol, kind).carries())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_runtime_contract::protocol::ir::MediaSource;

    #[test]
    fn openai_shaped_inline_audio_accepts_only_wav_and_mp3() {
        for protocol in [Protocol::OpenAICompatible, Protocol::WatsonxTextChat] {
            assert!(attachment_delivery(protocol, AttachmentKind::Audio).inline);
            assert!(!attachment_delivery(protocol, AttachmentKind::Audio).url);
            for media_type in [
                "audio/wav",
                "audio/x-wav",
                "audio/wave",
                "audio/mp3",
                "audio/mpeg",
            ] {
                assert!(
                    inline_media_type_supported(protocol, AttachmentKind::Audio, media_type),
                    "{protocol} must accept {media_type}"
                );
            }
            for media_type in ["audio/flac", "audio/ogg", "video/mp4", ""] {
                assert!(
                    !inline_media_type_supported(protocol, AttachmentKind::Audio, media_type),
                    "{protocol} must reject {media_type}"
                );
            }
        }
        // Gemini 的 inlineData 不区分音频格式。
        assert!(inline_media_type_supported(
            Protocol::GoogleGemini,
            AttachmentKind::Audio,
            "audio/flac"
        ));
    }

    #[test]
    fn bedrock_and_anthropic_restrict_inline_media_type() {
        let image = ContentBlock::Image {
            source: MediaSource::Base64 {
                media_type: "image/svg+xml".into(),
                data: String::new(),
            },
            detail: None,
            cache_control: None,
        };
        assert!(!inline_media_type_supported(
            Protocol::BedrockConverse,
            AttachmentKind::Image,
            "image/svg+xml"
        ));
        assert!(!inline_media_type_supported(
            Protocol::AnthropicMessages,
            AttachmentKind::Image,
            "image/svg+xml"
        ));
        assert!(inline_media_type_supported(
            Protocol::OpenAICompatible,
            AttachmentKind::Image,
            "image/svg+xml"
        ));
        assert!(attachment_block_supported(
            Protocol::BedrockConverse,
            &image
        ));
        assert!(!attachment_delivery(Protocol::BedrockConverse, AttachmentKind::Image).url);
    }

    #[test]
    fn document_delivery_follows_encoder_support() {
        assert_eq!(
            attachment_delivery(Protocol::GoogleGemini, AttachmentKind::Document),
            AttachmentDelivery::BOTH
        );
        // chat-completions 的 `file` 部件只有 file_data / file_id 属于官方 schema。
        assert_eq!(
            attachment_delivery(Protocol::OpenAICompatible, AttachmentKind::Document),
            AttachmentDelivery::INLINE
        );
        assert_eq!(
            attachment_delivery(Protocol::BedrockConverse, AttachmentKind::Document),
            AttachmentDelivery::INLINE
        );
        // watsonx 复用 chat-completions encoder，但上游没有 `file` 部件。
        for kind in [AttachmentKind::File, AttachmentKind::Document] {
            assert!(!attachment_delivery(Protocol::WatsonxTextChat, kind).carries());
        }
        assert!(!url_media_type_supported(
            Protocol::AnthropicMessages,
            AttachmentKind::File,
            "text/plain"
        ));
        assert!(url_media_type_supported(
            Protocol::AnthropicMessages,
            AttachmentKind::File,
            "application/pdf"
        ));
        assert!(url_media_type_supported(
            Protocol::GoogleGemini,
            AttachmentKind::Document,
            "text/plain"
        ));
        assert_eq!(
            attachment_delivery(Protocol::GatewayLanguageModel, AttachmentKind::Document),
            AttachmentDelivery::NONE
        );
        assert_eq!(
            attachment_delivery(Protocol::CohereChat, AttachmentKind::Document),
            AttachmentDelivery::NONE
        );
        assert!(inline_media_type_supported(
            Protocol::AnthropicMessages,
            AttachmentKind::Document,
            "application/pdf"
        ));
        assert!(!inline_media_type_supported(
            Protocol::AnthropicMessages,
            AttachmentKind::Document,
            "text/plain"
        ));
    }

    #[test]
    fn attachment_blocks_map_to_delivery_kinds() {
        let audio = ContentBlock::Audio {
            source: MediaSource::Url("https://example.test/a.mp3".into()),
        };
        assert_eq!(
            AttachmentKind::of_block(&audio),
            Some(AttachmentKind::Audio)
        );
        assert!(!attachment_block_supported(Protocol::CohereChat, &audio));
        assert!(attachment_block_supported(
            Protocol::WatsonxTextChat,
            &audio
        ));
        assert_eq!(
            AttachmentKind::of_block(&ContentBlock::Text {
                text: "text".to_owned().into(),
                cache_control: None
            }),
            None
        );
    }
}
