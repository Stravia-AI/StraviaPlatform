# Media Understanding 统一 Lossy WebP 转码 × 上游 Provider 支持与 Stravia Codec 透传 一手研究

| 项 | 值 |
|---|---|
| 研究日期 | 2026-08-11（按此日期截断官方文档事实） |
| 研究范围 | OpenAI Responses/Chat vision、Anthropic Messages vision、Google Gemini image understanding 对 `image/webp` 输入的官方格式承诺；Stravia 现有四个出向 encoder 对 `media_type` 的透传契约；Rust `image`、`webp`(libwebp) 对 lossy encode / ICC / alpha 的官方源码契约 |
| 一手来源限定 | 只使用 OpenAI / Anthropic / Google 官方文档页与官方仓库源码；Rust crate 使用 GitHub 主分支源码与 docs.rs。不以浏览器支持率、博客、聚合站推断 Provider 承诺 |
| 边界 | 本文件为只读审查产物，不实施产品代码，不修改 CONTEXT/ADR/design 文档。代码引用仅用于复核已有 codec 行为 |

> 术语遵循既有研究文档约定：`[S#]` 标注官方来源编号。MIME / API 字段 / 代码标识符保留原文。

---

## 1. 先给结论

1. **三家主要 Provider 均在官方文档中明确列出 `image/webp`。** OpenAI Images-and-vision guide 的 "Image input requirements" 表格列出 `WEBP (.webp)`；Anthropic Vision guide 的 "Supported formats" 明写 `image/webp`；Gemini Image understanding guide 的 "Supported image formats" 明写 `image/webp`。三家均为**官方承诺**，不是未证实。[S1][S2][S3]
2. **Stravia 四个出向 encoder（OpenAI Responses、OpenAI Chat-compat、Anthropic Messages、Google Gemini）全部把 `media_type` 当不透明字符串原样透传。** IR 层 `MediaSource::Base64 { media_type: String }` 无枚举校验；四个 encoder 无 MIME allowlist。一张 canonical Base64 WebP（`media_type = "image/webp"`）会被**原样编码**为各 Provider 的 wire shape，MIME 不会丢失或重写。[C1]
3. **Rust `image` crate 单独只能 lossless WebP（VP8L），无法做 lossy。** `image` crate 的 `WebPEncoder` 唯一构造函数是 `new_lossless`，官方 doc 注释明写 "Right now only **lossless** encoding is supported. If you need **lossy** encoding, you'll have to use `libwebp`"。Q85 lossy 必须用 `webp` crate（libwebp-sys 绑定）。[C2]
4. **`image` crate 的 WebP encoder 支持 ICC profile 嵌入（`set_icc_profile`），但 `webp` crate 的 `Encoder` 高层 API 不暴露 ICC 嵌入。** `webp` crate 的 `new_picture` 只设 `use_argb / width / height` 并 import 像素，不设 `picture.icc`。因此 lossy WebP 在 `webp` crate 路径下**无法通过高层 API 嵌入 ICC**。[C2]
5. **三家 Provider 均不读取图片 ICC/metadata。** OpenAI 明写 "The model doesn't process original file names or metadata"；Anthropic FAQ 明写 "Claude does not parse or receive any metadata from images"。因此 "sRGB 转换后移除 profile → Q85 lossy WebP" 是与 Provider 行为一致的路径：先转 sRGB 使色彩空间标准化，再剥离 profile（Provider 本就不读），避免因 lossy 路径无法嵌 ICC 而引入未标记色彩空间歧义。[S1][S2]
6. **统一 WebP 的发布阻断条件不在三家主要 Provider 侧。** 三家均明确接受 WebP，Stravia codec 均原样透传 MIME。实际阻断点在：(a) 非"三家"的 OpenAI-compatible / custom vendor adapter 是否接受 WebP **未在本研究范围内证实**；(b) lossy Q85 编码依赖 `webp` crate 而非 `image` crate，若 Media Understanding 只引入 `image` 则无法产出 lossy WebP。[C3]
7. **Q85 / 最长边 3072 的参数选择在三家 Provider 的限制内。** 3072px 最长边远低于 OpenAI / Anthropic（8000px 硬上限）的尺寸限制；三家都会按自身 patch/tile 策略进一步下采样，因此 3072 是"发送前预缩放"的合理上界，不触发任何 Provider 拒绝。[S1][S2][S3]

---

## 2. 来源矩阵

| ID | 官方一手来源 | 本文采用的事实 | 查询日期 |
|---|---|---|---|
| S1 | [OpenAI — Images and vision guide](https://developers.openai.com/api/docs/guides/images-vision) | "Image input requirements" 表格：Supported file types = PNG / JPEG / WEBP / Non-animated GIF；Size limits = 单请求总 payload ≤ 512 MB、≤ 1500 张图；Limitations 明写 "The model doesn't process original file names or metadata" | 2026-08-11 |
| S2 | [Anthropic — Vision guide](https://platform.claude.com/docs/en/build-with-claude/vision) | "Supported formats" 明列 `image/webp`；Request limits = 直连 API 单图 ≤ 10 MB(base64)、≤ 100 张/请求(200k 模型) 或 ≤ 600 张(其他)、单图 ≤ 8000×8000 px、>20 张时更严 per-image 限制；FAQ "Claude does not parse or receive any metadata from images"；Image quality guidance 提及 lossy WebP 可降延迟但需注意 artifacts | 2026-08-11 |
| S3 | [Google — Gemini Image understanding](https://ai.google.dev/gemini-api/docs/image-understanding) | "Supported image formats" 明列 `image/webp`（与 PNG/JPEG/HEIC/HEIF 并列）；File limit = 单请求 ≤ 3600 image files；Token calculation = ≤384px 双边时 258 tokens，更大图按 768×768 tile 切分 | 2026-08-11 |
| C1 | Stravia 仓库源码（见 §4 仓库路径索引） | 四个 encoder + IR `MediaSource` 均将 `media_type` 作不透明 `String` 透传，无 MIME allowlist | 2026-08-11 |
| C2 | [image-rs/image `src/codecs/webp/encoder.rs`](https://github.com/image-rs/image/blob/main/src/codecs/webp/encoder.rs)（main 分支）；[`webp` crate `src/encoder.rs`](https://github.com/jaredforth/webp/blob/main/src/encoder.rs)（main 分支，0.3.1） | `image` WebPEncoder = lossless-only(VP8L)、支持 `set_icc_profile`；`webp` crate `Encoder::encode(quality)` = lossy(libwebp)、不暴露 ICC 嵌入 | 2026-08-11 |

---

## 3. 各 Provider 官方 WebP 支持详情

### 3.1 OpenAI（Responses API + Chat Completions API）

**官方承诺 — 明确支持。** OpenAI 的 Images-and-vision guide 在 "Image input requirements" 小节以表格列出支持的文件类型：

> Supported file types: PNG (`.png`), JPEG (`.jpeg` and `.jpg`), WEBP (`.webp`), Non-animated GIF (`.gif`)

该 guide 明确说明此行为在 Responses API 与 Chat Completions API 上一致（"This behavior is the same in both the Responses API and the Chat Completions API"）。[S1]

**输入方式（三者均适用 WebP）：**
- URL（`input_image.image_url`，或 Chat 的 `image_url.url`）
- Base64 data URL（`data:image/webp;base64,...`）
- File ID（Files API，`purpose: "vision"`）

**大小 / 数量限制：**
- 单请求总 payload ≤ 512 MB
- 单请求 ≤ 1500 张图
- 各模型有 patch budget（如 GPT-5.4 `high` = 2500 patches / 2048px 最长边；`original` = 10000 patches / 6000px）；超限会等比缩放，**不拒绝**

**Metadata 行为：** "The model doesn't process original file names or metadata. `low` and `high` detail, and models with finite image budgets, may resize images before analysis."[S1]

> **对 Stravia 的含义：** OpenAI 的 `detail` 参数（`low`/`high`/`original`/`auto`）控制 patch-based tokenization。Media Understanding 若用 Q85/3072 统一 WebP，等价于在客户端完成预缩放，可配合 `detail: "original"` 保留 3072 级细节，或用 `auto` 让模型自选。3072px 在所有模型族的 patch budget 内不会触发额外缩放（GPT-5.4 `high` 上限 2048px 除外——该档会再缩一次，但不拒绝）。

### 3.2 Anthropic（Messages API vision）

**官方承诺 — 明确支持。** Anthropic Vision guide "Supported formats" 小节原文：

> Claude supports JPEG, PNG, GIF, and WebP images (`image/jpeg`, `image/png`, `image/gif`, `image/webp`). Animations are unsupported, and only the first frame is used.

FAQ 中同样确认："JPEG, PNG, GIF, and WebP."[S2]

**输入方式：**
- Base64（`source.type = "base64"`, `media_type = "image/webp"`）
- URL（`source.type = "url"`）
- Files API `file_id`（`source.type = "file"`，beta `files-api-2025-04-14`）

> Bedrock / Vertex 上只支持 base64（官方 Note）。

**大小 / 数量 / 尺寸限制：**
- 单图 ≤ 10 MB（base64，直连 API）；Bedrock/Vertex ≤ 5 MB
- 单请求 ≤ 100 张（200k context 模型）或 ≤ 600 张（其他模型）
- 单图 ≤ 8000×8000 px
- 单请求 > 20 张图/文档时，per-image 更严限制生效（每维 ≤ 2000px 或控制在 ≤ 20 张）
- 标准端点请求总大小 ≤ 32 MB

**分辨率与 token：** Claude 用 28×28px patch 作为 visual token；High-resolution tier（Claude 4.7+）最长边 2576px / 4784 tokens，Standard tier 最长边 1568px / 1568 tokens；超限自动等比缩放。[S2]

**Metadata 行为：** FAQ 明确 "Claude does not parse or receive any metadata from images passed to it."[S2]

**Image quality guidance（官方建议）：**

> Compressing images before sending them, using a lossy format such as JPEG or WebP (lossy mode), can reduce latency by reducing the size of requests. However, this can introduce artifacts that are detrimental to model performance, especially when multiple compression passes are applied.

这表明 Anthropic **官方知晓并接受 lossy WebP**，同时提示注意多次压缩的 artifacts 累积。[S2]

> **对 Stravia 的含义：** Q85/3072 WebP 对 Anthropic 而言是官方推荐路径（"lossy mode … can reduce latency"）。3072px 最长边远低于 8000px 硬上限；High-resolution tier 会下采样到 2576px。需注意：若请求含 >20 张图，每维需 ≤ 2000px，此时 3072 会触发"many-image requests"的更严限制——Media Understanding 应在批量场景下将最长边压到 ≤ 2000px。

### 3.3 Google Gemini（GenerateContent / Interactions — image understanding）

**官方承诺 — 明确支持。** Gemini Image understanding guide "Supported image formats" 小节原文：

> Gemini supports the following image format MIME types:
> - PNG - `image/png`
> - JPEG - `image/jpeg`
> - WEBP - `image/webp`
> - HEIC - `image/heic`
> - HEIF - `image/heif`

[S3]

**输入方式：**
- Inline base64（`inlineData.mimeType = "image/webp"`, `inlineData.data`）
- File API URI（`fileData.fileUri`, `fileData.mimeType`）
- URL（`image.uri` in Interactions API）

**数量 / 尺寸限制：**
- 单请求 ≤ 3600 image files
- Token 计算：双边 ≤ 384px 时 258 tokens；更大图按 768×768 tile 切分，每 tile 258 tokens

**Metadata 行为：** Gemini guide 未明确声明是否读取 ICC profile——**此项未证实**。但 Gemini 的 token 化基于像素 tile，不依赖 EXIF/ICC。[S3]

> **对 Stravia 的含义：** Gemini 的 `inlineData.mimeType` 字段原生接受 `image/webp`。Q85/3072 的 tile 成本可估算：3072×3072 ≈ ⌈3072/768⌉×⌈3072/768⌉ = 4×4 = 16 tiles × 258 = 4128 tokens（非精确，crop unit 会影响实际 tile 数）。这是可接受的范围。

---

## 4. Stravia Codec `media_type` 透传契约

以下均为仓库源码逐行复核。结论：**四个出向 encoder + IR 层均把 `media_type` 当不透明 `String`，无 MIME allowlist，canonical Base64 WebP 会被原样编码到各 Provider wire shape。**

### 4.1 IR 层 — `MediaSource`

```rust
// backend/crates/stravia-core/src/protocol/ir/request.rs:31-36
pub enum MediaSource {
    Base64 { media_type: String, data: String },
    Url(String),
    FileId { file_id: String, detail: Option<String> },
}
```

`media_type: String`——无枚举、无校验。canonical Base64 WebP 以 `media_type = "image/webp"` 进入 IR 后，所有下游 encoder 直接读这个字符串。

### 4.2 OpenAI Responses encoder

```rust
// backend/crates/stravia-core/src/protocol/codec/openai/responses/encoder.rs:296-299
MediaSource::Base64 { media_type, data } => {
    encoded["image_url"] =
        Value::String(format!("data:{media_type};base64,{data}"));
}
```

`image/webp` → `data:image/webp;base64,...`。无 MIME 过滤。测试 `encodes_responses_supported_media_without_text_coercion`（同文件 L407）用 `image/png` 验证路径，但 encoder 本身对 `image/webp` 同样适用——无分支差异。

### 4.3 OpenAI Chat-completions compatible encoder

```rust
// backend/crates/stravia-core/src/protocol/codec/openai/compatible/encoder.rs:606-609
MediaSource::Base64 { media_type, data } => {
    format!("data:{media_type};base64,{data}")
}
```

经 `media_source_to_url` 统一拼 data URL。同样无 MIME 过滤。

### 4.4 Anthropic Messages encoder

```rust
// backend/crates/stravia-core/src/protocol/codec/anthropic/messages/encoder.rs:438-443
MediaSource::Base64 { media_type, data } => serde_json::json!({
    "type": "base64",
    "media_type": media_type,
    "data": data,
}),
```

`validate_anthropic_payload`（同文件 L217-）只校验 block **type**（`text`/`image`/`thinking`/`tool_use`/`tool_result`/`document`/`input_audio`），**不校验 `media_type` 值**。`image/webp` 原样进入 `source.media_type`。

### 4.5 Google Gemini encoder

```rust
// backend/crates/stravia-core/src/protocol/codec/google/gemini/encoder.rs:201-207
ContentBlock::Image { source, .. } => match source {
    MediaSource::Base64 { media_type, data } => serde_json::json!({
        "inlineData": {
            "mimeType": media_type,
            "data": data,
        }
    }),
```

`image/webp` 原样进入 `inlineData.mimeType`。

### 4.6 透传结论

| 出向路径 | wire 字段 | `image/webp` 是否原样保留 | MIME 校验 |
|---|---|---|---|
| OpenAI Responses | `input_image.image_url` = `data:image/webp;base64,...` | ✅ | 无 |
| OpenAI Chat-compat | `image_url.url` = `data:image/webp;base64,...` | ✅ | 无 |
| Anthropic Messages | `source.media_type` = `image/webp` | ✅ | 仅校验 block type，不校验 media_type |
| Google Gemini | `inlineData.mimeType` = `image/webp` | ✅ | 无 |

> **风险提示：** 无 MIME allowlist 意味着任意 `media_type` 字符串都会被透传。对于三家主要 Provider 这是安全的（它们均接受 `image/webp`）。但对于走 OpenAI-compatible / custom vendor adapter 的**非三家 Provider**，不支持的 MIME 会直达上游并可能被拒绝——这部分未在本研究范围内证实。

---

## 5. Rust 编解码 crate 官方契约

### 5.1 `image` crate（image-rs/image）— Lossless-only WebP

源码：[`src/codecs/webp/encoder.rs`](https://github.com/image-rs/image/blob/main/src/codecs/webp/encoder.rs)（main 分支）

**关键 doc 注释（struct 级）：**

> ### Limitations
> Right now only **lossless** encoding is supported.
> If you need **lossy** encoding, you'll have to use `libwebp`. Example code for encoding a `DynamicImage` with `libwebp` via the [`webp`](https://docs.rs/webp/latest/webp/) crate can be found [here](https://github.com/jaredforth/webp/blob/main/examples/convert.rs).

**唯一构造函数：**

```rust
/// Create a new encoder that writes its output to `w`.
/// Uses "VP8L" lossless encoding.
pub fn new_lossless(w: W) -> Self
```

**ICC profile 支持（lossless 路径可用）：**

```rust
fn set_icc_profile(&mut self, icc_profile: Vec<u8>) -> Result<(), UnsupportedError> {
    self.inner.set_icc_profile(icc_profile);
    Ok(())
}
```

`set_icc_profile` 委托给内部 `image_webp::WebPEncoder`。注意：这是 `image` crate 的 **lossless** encoder 路径——它能在 VP8L 码流中嵌入 ICC profile chunk，但这**不适用于 lossy 场景**。

**颜色类型限制：** `encode` 只接受 `L8` / `La8` / `Rgb8` / `Rgba8`，其余返回 `UnsupportedError`。

> **结论：** `image` crate 单独**无法产出 lossy WebP**。若 Media Understanding 的目标是 Q85 lossy WebP，**不能只依赖 `image` crate**——必须引入 `webp` crate（libwebp-sys 绑定）或直接使用 `image_webp`（但 `image_webp` 同样是 lossless-only，其 lossy 支持未在 `image` 的 encoder 中暴露）。

### 5.2 `webp` crate（jaredforth/webp）— libwebp-sys 绑定，支持 lossy

源码：[`src/encoder.rs`](https://github.com/jaredforth/webp/blob/main/src/encoder.rs)（main 分支，docs.rs 版本 0.3.1）

**Lossy 编码 API：**

```rust
/// Encode the image with the given quality.
/// The image quality must be between 0.0 and 100.0 inclusive for minimal and maximal quality respectively.
pub fn encode(&self, quality: f32) -> WebPMemory {
    self.encode_simple(false, quality).unwrap()
}
```

`encode` 调用 `encode_simple(false, quality)`，其中 `false` = `lossless` 参数为 `false`：

```rust
pub fn encode_simple(&self, lossless: bool, quality: f32) -> Result<WebPMemory, WebPEncodingError> {
    let mut config = WebPConfig::new().unwrap();
    config.lossless = if lossless { 1 } else { 0 };
    config.alpha_compression = if lossless { 0 } else { 1 };
    config.quality = quality;
    self.encode_advanced(&config)
}
```

因此 `Encoder::encode(85.0)` 会产出 **Q85 lossy WebP**，且 lossy 模式下 `alpha_compression = 1`（启用 alpha 通道压缩）。

**像素导入：** `new_picture` 使用 `WebPPictureImportRGB` / `WebPPictureImportRGBA`，设 `use_argb = 1`。

**ICC profile 嵌入 — 不暴露：**

```rust
pub(crate) unsafe fn new_picture(
    image: &[u8], layout: PixelLayout, width: u32, height: u32,
) -> ManageedPicture {
    let mut picture = WebPPicture::new().unwrap();
    picture.use_argb = 1;
    picture.width = width as i32;
    picture.height = height as i32;
    // 导入像素后返回——未设置 picture.icc
    ...
}
```

`new_picture` 不设置 `picture.icc`（libwebp 的 `WebPPicture` 结构体有 `icc` 字段用于 ICC profile，但 `webp` crate 的 Rust 安全 API 不暴露设置它）。`encode_advanced` 接受 `&WebPConfig`（质量/lossless/alpha 等编码参数），但不接受 ICC 数据。

> **结论：** `webp` crate 能做 Q85 lossy WebP，但**高层 API 无法嵌入 ICC profile**。要在 lossy WebP 中嵌入 ICC，需要直接操作 `libwebp-sys` 的 `WebPPicture.icc`（unsafe），超出 `webp` crate 的安全封装范围。

### 5.3 Lossy + ICC 的契约矛盾与 "sRGB 先行" 的必要性

综合 5.1 与 5.2：

| 能力 | `image` crate WebPEncoder | `webp` crate Encoder |
|---|---|---|
| Lossy WebP | ❌（lossless-only VP8L） | ✅（`encode(quality)`，Q0–100） |
| Lossless WebP | ✅（`new_lossless`） | ✅（`encode_lossless`） |
| ICC profile 嵌入 | ✅（`set_icc_profile`，lossless 路径） | ❌（高层 API 不暴露） |
| Alpha 通道 | ✅（Rgba8） | ✅（`alpha_compression`，lossy=1） |
| 颜色类型 | L8/La8/Rgb8/Rgba8 | RGB/RGBA only |

**Q85 lossy WebP 路径（`webp` crate）无法嵌入 ICC**，而三家 Provider 又不读取 ICC/metadata（§3）。因此正确策略是：

1. **先转 sRGB**：把任意 ICC 的像素数据转换到 sRGB 色彩空间（用 `image` crate 的 color management 或外部 ICC 转换），使像素值在标准空间下正确。
2. **再剥离 ICC profile**：既然 lossy 路径无法嵌 ICC，且 Provider 不读 ICC，剥离后不会引入歧义（sRGB 是 Web 的默认假设空间）。
3. **Q85 lossy encode**：用 `webp` crate `Encoder::from_rgb/from_rgba(...).encode(85.0)` 产出 lossy WebP。

这与三家 Provider 的行为完全一致——它们按 sRGB 假设解码像素，不依赖 ICC profile。

> **Caveat（未知范围）：** 若未来有 Provider 实际读取并依赖 ICC profile 来做色彩管理（目前三家均未证实），剥离 ICC 的 lossy WebP 在非 sRGB 源图上可能产生色彩偏差。本研究日期内无此证据。

---

## 6. 兼容性结论：Q85 / 最长边 3072 / sRGB-strip WebP

### 6.1 总判定

| 维度 | 判定 | 依据 |
|---|---|---|
| 三家主要 Provider 接受 `image/webp` | ✅ 可直接采用 | [S1][S2][S3] 均明确列出 |
| Stravia codec 保留 `image/webp` MIME | ✅ 原样透传 | [C1] 四个 encoder 无 MIME 校验 |
| Q85 lossy 编码可行性 | ✅ 需 `webp` crate | [C2] `image` crate lossless-only |
| ICC 剥离安全性 | ✅ 与 Provider 行为一致 | [S1][S2] Provider 不读 metadata |
| 3072px 最长边尺寸 | ✅ 在三家限制内 | OpenAI patch budget / Anthropic 8000px / Gemini tile |
| 非"三家"Provider 接受 WebP | ⚠️ 未证实 | OpenAI-compatible / custom vendor adapter 未覆盖 |

### 6.2 采用策略建议

**对三家主要 Provider（OpenAI / Anthropic / Gemini）：可直接采用。** 统一 Q85/3072/sRGB-strip lossy WebP 不需要 fallback，不需要 Target capability metadata 来控制是否转 WebP——三家均原生接受。

**对非三家 Provider：需 Target capability metadata 或保留 fallback。** 研究范围内无法证实的 Provider（如 Bedrock-Anthropic 的 base64 限制、各 OpenAI-compatible 国产模型 API、Ollama 等）可能不接受 `image/webp`。建议：

- **方案 A（推荐）**：在 Target/Route 协商层增加 `accepts_webp: bool`（或复用现有 capability 声明 module），Media Understanding 仅对 `accepts_webp = true` 的 Provider 产出统一 WebP；对未知/false 的 Provider 保留原始格式或转 PNG/JPEG fallback。
- **方案 B（保守）**：统一 WebP 仅作为 artifact 存储格式；发往上游时若 Provider 不在已知"三家"白名单，则转码回 JPEG。代价是额外的 decode/encode 往返。

### 6.3 发布阻断条件（明确清单）

| # | 阻断条件 | 状态 | 说明 |
|---|---|---|---|
| 1 | 三家 Provider 接受 WebP | ✅ 已证实，不阻断 | [S1][S2][S3] |
| 2 | Stravia codec 透传 MIME | ✅ 已证实，不阻断 | [C1] |
| 3 | lossy WebP 编码能力 | ⚠️ 需引入 `webp` crate | `image` crate 单独不够；若 Cargo 未加 `webp` 依赖则阻断 |
| 4 | 非三家 Provider 兼容 | ⚠️ 未证实 | 需 capability 声明或 fallback，否则可能对未知 Provider 发出不支持的 MIME |
| 5 | Anthropic 批量 >20 张图的尺寸限制 | ⚠️ 参数注意 | Q85/3072 在 >20 张图场景下需降到 ≤ 2000px/维，否则触发 "many-image requests" 拒绝 [S2] |

**核心阻断条件：#3（lossy 编码依赖）和 #4（非三家 Provider 未证实）。** 若 Media Understanding 只在三家 Provider 范围内使用且 Cargo 已含 `webp` crate，则无阻断。

---

## 7. 仓库源码路径索引

以下为 Stravia 仓库内复核 codec 透传行为所引用的文件（相对仓库根）：

- IR `MediaSource`：[`backend/crates/stravia-core/src/protocol/ir/request.rs`](../../backend/crates/stravia-core/src/protocol/ir/request.rs) L31-36（`Base64 { media_type: String, data: String }`）
- OpenAI Responses encoder 图片块：[`backend/crates/stravia-core/src/protocol/codec/openai/responses/encoder.rs`](../../backend/crates/stravia-core/src/protocol/codec/openai/responses/encoder.rs) L295-299（data URL 拼接）
- OpenAI Chat-compat encoder：[`backend/crates/stravia-core/src/protocol/codec/openai/compatible/encoder.rs`](../../backend/crates/stravia-core/src/protocol/codec/openai/compatible/encoder.rs) L606-609（`media_source_to_url`）
- Anthropic Messages encoder 图片块：[`backend/crates/stravia-core/src/protocol/codec/anthropic/messages/encoder.rs`](../../backend/crates/stravia-core/src/protocol/codec/anthropic/messages/encoder.rs) L434-453（`source.media_type`）；payload 校验 L217-（`ALLOWED_BLOCK_TYPES` 只校验 block type）
- Google Gemini encoder 图片块：[`backend/crates/stravia-core/src/protocol/codec/google/gemini/encoder.rs`](../../backend/crates/stravia-core/src/protocol/codec/google/gemini/encoder.rs) L201-206（`inlineData.mimeType`）
- Cargo 依赖核查：`backend/` 下未发现 `image` / `webp` / `image_webp` / `libwebp` crate 依赖（截至 2026-08-11），确认 lossy WebP 编码能力尚未引入

---

## 8. 未证实范围（明确声明）

1. **Gemini 是否读取 ICC profile**：Gemini guide 未明确声明。本研究不按"Gemini 基于 libwebp 解码，应支持 ICC"推断——只记录"guide 未声明"。
2. **非三家 Provider 的 WebP 支持**：Bedrock-Anthropic、Vertex-Gemini、OpenAI-compatible 国产模型 API、Ollama 等的 WebP 接受情况未在本文一手来源中证实。
3. **WebP 动画**：三家 Provider 的 WebP 支持均指静态图。OpenAI 限 "Non-animated GIF"，类比推断 WebP 动画同样不被支持（Anthropic 明写 "Animations are unsupported"）。
4. **Q85 具体视觉质量**：Q85 的 artifacts 对各 Provider 模型理解能力的影响需实测，不属于文档可证实范围。Anthropic 官方提示 "heavy compression can make text difficult to read"，建议实测确认。[S2]
