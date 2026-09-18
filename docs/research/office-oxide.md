# office_oxide — 面向 Media Understanding Office 格式支持的 Crate 评估

| 项目 | 值 |
|---|---|
| 研究日期 | 2026-09（事实截至于此日期） |
| 范围 | `office_oxide` Rust crate：crates.io 页面、docs.rs API 文档、GitHub 源码仓库（yfedoseev/office_oxide）、文档站 office.oxide.fyi |
| 目的 | 判断 office_oxide 能否为 Media Understanding 能力从 Office 文件中提取 LLM 可用内容（文本、表格、图片）。参见 `docs/design/media-understanding.md`、ADR-0009 |

## 结论

**可用，但有保留条件。** `office_oxide` 提供单一纯 Rust API——`Document::open` / `Document::from_reader` → `plain_text()` / `to_markdown()` / `to_ir()`——覆盖 DOCX、XLSX、PPTX **以及**遗留 DOC/XLS/PPT。官方定位即 "RAG-ready — clean Markdown output for LLM ingestion"（https://office.oxide.fyi/），IR 保留标题、表格、演讲者备注、元数据与嵌入图片字节（`ir::Image.data: Option<Vec<u8>>`）。主要风险：0.1.x 版本、2026-04-28 首次发布、实质单作者维护；且 `read_zip_entry` 解压 ZIP entry 时**无解压后大小上限**——处理不可信输入需要我们在 `stravia-media` 侧加输入大小上限和有界/可捕获的解析上下文。不覆盖 ODF 与 PDF。

## 1. 功能与格式支持

纯 Rust 库，用于解析、转换、编辑 Office 文档——"DOCX, XLSX, PPTX, plus the legacy binary formats DOC, XLS, PPT. One crate, one unified `Document` handle, zero native dependencies"（https://office.oxide.fyi/；https://docs.rs/office_oxide/latest/office_oxide/index.html）。

逐格式矩阵（README，https://github.com/yfedoseev/office_oxide）：

| 格式 | 扩展名 | 读 | 写 | 编辑 | Text/Markdown/HTML/IR |
|---|---|---|---|---|---|
| Word OOXML | `.docx` | ✅ | ✅ | ✅ | ✅ 全部 |
| Excel OOXML | `.xlsx` | ✅ | ✅ | ✅ | ✅ 全部 |
| PowerPoint OOXML | `.pptx` | ✅ | ✅ | ✅ | ✅ 全部 |
| Word 遗留 | `.doc` | ✅ | — | — | ✅ 全部；`save_as` → `.docx` |
| Excel 遗留 | `.xls` | ✅ | — | — | ✅ 全部；`save_as` → `.xlsx` |
| PowerPoint 遗留 | `.ppt` | ✅ | — | — | ✅ 全部；`save_as` → `.pptx` |

`DocumentFormat` 枚举恰好只有这六个变体（https://github.com/yfedoseev/office_oxide/blob/b5819b19/src/format.rs）。**不支持：** ODF（`.odt`/`.ods`/`.odp`——corpus 测试中被归入 rejected-inputs）、PDF（作者另有 `pdf_oxide` crate，见 issue #62）、XLSB、RTF，以及**加密/口令保护文件**（无 password API；BENCHMARKS.md 计为预期失败）。

## 2. API 面

顶层自由函数（仅路径）——https://docs.rs/office_oxide/latest/src/office_oxide/lib.rs.html：

```rust
pub fn extract_text(path: impl AsRef<Path>) -> Result<String>
pub fn to_markdown(path: impl AsRef<Path>) -> Result<String>
pub fn to_html(path: impl AsRef<Path>) -> Result<String>
```

`Document` 句柄（https://docs.rs/office_oxide/latest/office_oxide/struct.Document.html；src/lib.rs）：

```rust
impl Document {
    pub fn open(path: impl AsRef<Path>) -> Result<Self>;                    // 扩展名检测 + magic-byte 嗅探
    pub fn from_reader<R: Read + Seek + Send + 'static>(
        reader: R, format: DocumentFormat,
    ) -> Result<Self>;                                                     // bytes → Cursor<Vec<u8>>；显式 format，不嗅探
    #[cfg(feature = "mmap")]
    pub fn open_mmap(path: impl AsRef<Path>) -> Result<Self>;                // 仅 OOXML
    pub fn format(&self) -> DocumentFormat;
    pub fn plain_text(&self) -> String;
    pub fn to_markdown(&self) -> String;
    pub fn to_html(&self) -> String;
    pub fn to_ir(&self) -> DocumentIR;
    pub fn save_as(&self, path: impl AsRef<Path>) -> Result<()>;             // 遗留 → OOXML，经由 IR
    pub fn as_docx(&self) -> Option<&docx::DocxDocument>;                    // + as_xlsx/as_pptx/as_doc/as_xls/as_ppt
}
```

错误类型（https://docs.rs/office_oxide/latest/office_oxide/error/enum.OfficeError.html）：

```rust
pub enum OfficeError {
    Core(Error), Docx(DocxError), Xlsx(XlsxError), Pptx(PptxError),
    Doc(DocError), Xls(XlsError), Ppt(PptError), UnsupportedFormat(String),
}
pub type Result<T> = std::result::Result<T, OfficeError>;
```

注意：`open()`/`from_reader()` 返回 `Result`，而 `plain_text()`/`to_markdown()`/`to_html()`/`to_ir()` 解析后为 infallible。`sniff_format` 按 magic bytes 纠正错误扩展名（`PK\x03\x04` → OOXML，`D0 CF 11 E0` → CFB）——src/lib.rs L436-457。

公开模块：`cfb`（OLE2 读取）、`core`（OPC/XML）、`create`、`doc`、`docx`、`edit`、`error`、`ffi`、`format`、`ir`、`ppt`、`pptx`、`xls`、`xlsx`（https://docs.rs/office_oxide/latest/office_oxide/index.html）。

## 3. 各格式可提取内容

所有格式共通：plain text、Markdown、HTML 片段，以及 serde 可序列化的 `DocumentIR`（`pub metadata: Metadata, pub sections: Vec<Section>`；Section = DOCX section / XLSX worksheet / PPTX slide——https://docs.rs/office_oxide/latest/office_oxide/ir/struct.DocumentIR.html、.../struct.Section.html）。

IR 块级 `Element` 枚举：`Heading(1-6)`、`Paragraph`、`Table`（`TableRow`/`TableCell`）、`List`、`Image`、`TextBox`、`CodeBlock`、`Footnote`/`Endnote`、`Shape`、breaks（https://docs.rs/office_oxide/latest/office_oxide/ir/enum.Element.html）。行内：`TextSpan { text, bold, italic, strikethrough, hyperlink: Option<String>, … }`（https://docs.rs/office_oxide/latest/office_oxide/ir/struct.TextSpan.html）。元数据：`format, title, author, subject, keywords, created, modified, description`（https://docs.rs/office_oxide/latest/office_oxide/ir/struct.Metadata.html）。

- **DOCX：** 完整块结构，含 headers/footers 并入 IR sections；嵌入图片有两处出口——`DocxDocument.images: HashMap<String, (Vec<u8>, Option<String>)>`（字节 + 扩展名，按 rId 索引）与 `ir::Image { alt_text, data: Option<Vec<u8>>, format: Option<ImageFormat>, display_*_emu, pixel_* }`（https://docs.rs/office_oxide/latest/office_oxide/docx/struct.DocxDocument.html、.../ir/struct.Image.html）。`ImageFormat`：Png/Jpeg/Gif/Tiff/Bmp/Emf/Wmf。
- **XLSX：** worksheet → section；`to_markdown()` 在每 sheet 的 `## <sheet name>` 下输出 GFM 管道表格；`to_csv()`/`sheet_to_csv()`；格式化单元格值（1900 日期系统）；图表文本以 `## Chart N` 块提取（src/xlsx/text.rs）。单列散文式 sheet 渲染为段落而非退化表格。
- **PPTX：** 每 slide 一个 section；shape 按上到下、左到右空间排序；title placeholder → `##` 标题；演讲者备注以 `[Notes]` 追加；`slide_to_markdown(i)`/`slide_plain_text(i)` 逐页可用（src/pptx/text.rs）；`Slide.notes: Option<String>`。
- **遗留 DOC/XLS/PPT：** 同一 `plain_text`/`to_markdown`/`to_ir` 面；`.ppt` 图片经 `PptDocument::images() -> &[PptImage]`（src/ppt/document.rs）。`save_as` 单向迁移到 OOXML。
- **无法提取：** ODF/PDF 内容、加密文件、图形化图表（仅文本）、渐变/图片 slide 背景（丢弃为 `None`，src/pptx/slide.rs）；footnote 的*引用标记*在渲染文本中丢弃，但正文保留在 IR（src/ir_render.rs）。维护者说明 IR→文档往返仅对 XLSX 可靠——与单向提取无关（issue #62）。

## 4. 成熟度

- 最新 **0.1.11**，发布于 2026-09-11；首个版本 0.1.0 发布于 2026-04-28；约 4.5 个月 12 个版本——节奏很活跃。**无 yanked 版本**（crates.io 版本历史）。
- 总下载约 577k，近期约 501k；**12 个 dependent**（crates.io）。数字可能被项目自身的多绑定 CI 抬高；真实 dependent 有 `kreuzberg`（见 PR #63 讨论）。
- 仓库：https://github.com/yfedoseev/office_oxide ——创建于 2026-03-01，约 86–125 star、14–18 fork、5 个 open issue，实质单作者（Yury Fedoseev）。文档站 https://office.oxide.fyi/。在 6,062 个真实文件上做 corpus 测试（BENCHMARKS.md：98.4% 通过，失败均为非法输入）。存在 cargo-fuzz target。dep 树中的 RustSec advisory 在 v0.1.3/v0.1.4 中被及时清理。
- `rust-version = "1.88"`、`edition = "2024"`（workspace Cargo.toml）——与本仓库 pinned Rust 1.98.1 兼容。

## 5. 许可证

**MIT OR Apache-2.0**（crates.io）。注意："OfficeOxide"/"office_oxide" 名称与 logo 有商标（TRADEMARKS.md）；作为内部依赖使用无碍。

## 6. 依赖重量

强制依赖：`quick-xml 0.41`、`zip 8.6`（default-features 关闭，仅 `deflate`）、`thiserror 2`、`serde`、`serde_json`、`log`、`encoding_rs`、`atoi_simd`、`fast-float2`、`libc`（crates.io/Cargo.toml）。`libc` 仅用于 Unix 的 `getrlimit(RLIMIT_STACK)`——不构建任何 C 库；crate 自称 "zero native dependencies"。可选 feature：`mmap`（memmap2）、`parallel`（rayon）、`python`（pyo3）、`wasm`（wasm-bindgen）。默认构建不引入任何可选项 → 轻量、纯 Rust、编译快的依赖树。

## 7. 健壮性与安全姿态

- **Result 返回的解析 API**（`open`/`from_reader` → `Result<Document>`）；解析后的提取方法为 infallible。
- **栈安全：** `with_parse_stack` 在 Unix `RLIMIT_STACK < 12 MB` 时用 16 MB 栈的独立线程跑解析，经 `join` 把 panic 转成 `Err("parsing panicked")`（src/lib.rs L89-156）。在 Windows 或大栈环境下**内联运行——panic 会向上传播**；处理不可信文件时应包 `catch_unwind` 或专用 worker。
- **Fuzzing：** `fuzz_parse.rs` 向全部六个解析器喂任意字节——"none may panic, overflow, or hang; malformed input must surface as `Err`"（fuzz/fuzz_targets/fuzz_parse.rs）。
- **⚠️ 无解压上限。** `read_zip_entry` 先 `Vec::with_capacity(file.size())` 再无界 `file.read_to_end(&mut buf)`（src/core/opc.rs）。声明超大 size 的 ZIP entry 会触发巨额前置分配（alloc 失败直接 abort 进程，不是 `Err`），伪造 header 则无界膨胀 → 典型 zip-bomb OOM。**Stravia 必须强制最大输入大小并在内存有界的上下文中执行提取。**（对照：作者关联 crate `oxidocs-common` 有 128 MB/512 MB/10k-entry 上限，office_oxide 没有。）
- XML bomb：billion-laughs/CVE fixture 在 corpus 中被拒绝；quick-xml 不展开自定义实体。quick-xml 0.41 升级修复了不可信 XML DoS（RUSTSEC-2026-0194/0195，v0.1.3 发布）。
- Zip-slip 不适用（entry 按名读入内存，不写盘）。加密文件返回错误而非挂起。

## 8. 输出保真度

Markdown 输出保留真实结构：ATX 标题、GFM 管道表格、`**bold**`/`*italic*`/`~~strike~~`、`[text](url)` 超链接、列表、`---` slide/section 分隔、`## <sheet>`/`## Slide N`/`## Chart N` 上下文标题、`[Notes]` 演讲者备注（src/ir_render.rs、src/pptx/text.rs、src/xlsx/text.rs）。这正是 model turn 需要的形态——表格/slide/sheet 边界得以保留，优于裸文本。

**无流式。** 解析是 eager 的：整个 ZIP/CFB 与嵌入媒体全部进入内存；`from_reader` 需要 `Read + Seek`；输出是单个 `String`/`DocumentIR`。`mmap` feature 仅对 OOXML 避免堆拷贝。提取后 Markdown 的 token 预算截断由我们负责。

## 9. 备选（仅覆盖 office_oxide 的缺口）

- **PDF：** `pdf_oxide`（同作者）或 `lopdf`。
- **ODF（`.ods` 等）：** `calamine`（只读；也覆盖 `.xlsb`）或 `dotext`。
- **仅 DOCX 的成熟选项：** `docx-rs`——不覆盖 xlsx/pptx，维护较弱。
- **加密 Office 文件：** 无良好纯 Rust 方案；msoffcrypto 是 Python。可作为已知限制接受。
- **全管道框架：** `kreuzberg`——本身依赖 office_oxide；此处过重。

## 10. 风险信号

1. **0.1.x、约 5 个月历史、单维护者**——API churn 风险；`Element`/`InlineContent` 标了 `#[non_exhaustive]`，match 需要 wildcard。
2. **无 zip-bomb 解压上限**（§7）——对不可信输入是唯一的实质加固缺口。
3. 下载量（<5 个月约 577k）相对约 100 个 GitHub star 疑似被 CI 抬高——真实但单薄的采用面（kreuzberg 是真实 dependent）。
4. 名称有商标（内部使用无碍）。
5. 抵消项：无 yanked 版本、有维护的 CHANGELOG、docs.rs 完整、fuzz target + 6k 文件 corpus、维护者响应及时（安全发布、详细 issue triage）。
