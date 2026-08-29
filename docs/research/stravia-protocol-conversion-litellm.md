# Stravia 协议转换 × LiteLLM 一手源码研究

| 项 | 值 |
|---|---|
| 研究日期 | 2026-08-09 |
| 仓库 | BerriAI/litellm |
| Commit SHA | `6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573` |
| 分支 | main（2026-08-08 14:24:04 -0700） |
| 本地核验 commit | `ecba48dd7c47f1c183786a337d61004e41d6bcdb`（2026-08-08 21:09，较 `6a919ae` 更新；`git diff --stat 6a919ae` 在本研究的 6 个 transformation 文件上零改动，故下文 blob URL 同时适用于两个 SHA） |
| blob URL 前缀 | `https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/` |
| 研究范围 | OpenAI Chat（`gpt_transformation.py` / `gpt_5_transformation.py` / `o_series_transformation.py`）、OpenAI Responses（`responses/transformation.py`）、Anthropic Messages（`anthropic/chat/transformation.py`）、Gemini GenerateContent（`vertex_ai/gemini/`）；仅请求构建与非流响应解析，不含流式 |

> 术语遵循 Stravia 词汇表与 ADR 0001/0003/0004：module、interface、implementation、depth、seam、adapter、leverage、locality。canonical invariants 指 model、ID、tool correlation、reasoning、usage、stop reason。本文件为只读审查产物，不提出新的 public interface。

---

## 1. LiteLLM 的 transformation seam 结构

LiteLLM 用「OpenAI ChatCompletion 作为 canonical IR + 每 provider 一个 `XxxConfig(BaseConfig)` adapter」实现协议转换。核心要点：**每 provider 只实现一次 `BaseConfig`，所有入站协议先归一到 OpenAI shape，再由 provider config 出向编码到上游 wire**。这与 Stravia「AiRequest IR + 每 protocol 一个 codec + Vendor hook」是镜像设计，但 LiteLLM 的归一中心是 OpenAI 而非自有 IR。

### 1.1 Chat 路径的抽象 interface

`BaseConfig`（`litellm/llms/base_llm/chat/transformation.py`）定义了两个必须实现的转换方法，以及一组「能力声明 + hook」可选方法：

源码：`litellm/llms/base_llm/chat/transformation.py`
- URL: https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/litellm/llms/base_llm/chat/transformation.py

关键抽象方法（行号对应 blob）：
- `transform_request(model, messages, optional_params, litellm_params, headers) -> dict`（L346，抽象，必须实现）
- `async_transform_request(...)`（L360，默认委托同步版；仅 OpenAI 覆盖以做 url→base64 异步转换）
- `transform_response(model, raw_response, model_response, logging_obj, request_data, messages, optional_params, litellm_params, encoding, api_key, json_mode) -> ModelResponse`（L378，抽象）
- `get_supported_openai_params(model) -> list`（L313，抽象）—— **支持参数白名单**
- `map_openai_params(non_default_params, optional_params, model, drop_params) -> dict`（L328，抽象）—— **OpenAI 参数→provider 参数映射**
- `validate_environment(headers, model, messages, optional_params, litellm_params, api_key, api_base) -> dict`（L339，抽象）—— **认证/header 注入**
- `get_complete_url(api_base, api_key, model, optional_params, litellm_params, stream) -> str`（L290，默认 `api_base`）

可选能力声明 hook（非抽象，有默认实现）：
- `should_fake_stream(model, stream, custom_llm_provider) -> bool`（L84，默认 False）
- `is_thinking_enabled(non_default_params) -> bool`（L60，识别 `thinking.type==enabled` 或 `reasoning_effort`）
- `update_optional_params_with_thinking_tokens(...)`（L70，thinking 开启时若未设 max_tokens 则补 budget+DEFAULT_MAX_TOKENS）
- `translate_developer_role_to_system_role(messages)`（L101，非 OpenAI provider 把 developer role 降级为 system）
- `_add_response_format_to_tools(...)`（L135，把 `response_format` 翻译成单 tool + tool_choice，给不支持 response_format 的 provider）
- `transform_parsed_response_dict(parsed_response) -> dict`（L397，修复 OpenAI SDK 直通路径的畸形响应）
- `apply_assembled_streaming_response_metadata(response, chunks)`（L443，流式合并元数据）
- `supports_stream_param_in_request_body`（L430，Bedrock invoke 为 False）
- `should_retry_llm_api_inside_llm_translation_on_http_error(...)`（L109，Azure AI 覆盖）

> **观察**：`BaseConfig` 是一个**中等深度**的 module——它既是每 provider 实现的 interface，又混入了大量「通用 helper」（thinking tokens、response_format→tool、developer role）。这些 helper 对所有 provider 可复用，但被放在 base class 里，导致 provider config 既要实现抽象方法又要继承一堆可选行为。Stravia 的 `Vendor` trait（`provider/vendor.rs`）把 hook（`pre_encode`/`post_encode`/`auth_headers`）和编排（`build_request`/`parse_response`）拆开，编排走共享 free function `pipeline::build_request`——这是更深的 seam：interface 只暴露 hook，通用行为集中在独立 module。

### 1.2 Responses 路径的独立 interface

**关键发现**：LiteLLM 为 Responses 维护了一套**独立**的 `BaseResponsesAPIConfig`（`litellm/llms/base_llm/responses/transformation.py`），与 Chat 的 `BaseConfig` **不复用**。它的抽象方法是 `transform_responses_api_request` / `transform_response_api_response` / `transform_streaming_response`，以及一整套 CRUD（delete/get/list/cancel/compact）的 request+response 对。

源码：`litellm/llms/base_llm/responses/transformation.py`
- URL: https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/litellm/llms/base_llm/responses/transformation.py

OpenAI 自己的 `OpenAIResponsesAPIConfig`（`litellm/llms/openai/responses/transformation.py:30`）的 `transform_response_api_response` 基本是直通——`"No transform applied since outputs are in OpenAI spec already"`（L264）。它主要做：剥离 Anthropic-only 的 `cache_control`（L137-163）、把 reasoning item 的 `status=None` 过滤掉（L226-262，issue #13484）、把 `max_output_tokens` 下限抬到 16（L65-75）。

> **对 Stravia 的含义**：Stravia 已有独立的 `protocol/codec/openai/responses/` 目录（encoder/decoder/parser/formatter/stream），与 Chat 的 `openai/compatible/` 分离——这与 LiteLLM「Responses 独立 interface」一致，是**正确的 seam 划分**。LiteLLM 的教训是：两套 interface 之间会有重复逻辑（如 `remove_cache_control_flag` 在 Chat 叫 `_from_messages_and_tools`、在 Responses 叫 `_from_input_and_tools`，签名不同但 body 都是 `filter_value_from_dict`）。Stravia 的 `cache.rs` 若被两个 codec 共享 leverage，应作为独立 module 而非各自复制。

### 1.3 Gemini 的双重结构

Gemini 是最复杂的：类层级是 `VertexGeminiConfig(VertexAIBaseConfig, BaseConfig)`（`vertex_and_google_ai_studio_gemini.py:157`），但 `transform_request` 在该类里 **`raise NotImplementedError`**（L2520），真正的请求构建在**模块级 free function** `_transform_request_body`（`vertex_ai/gemini/transformation.py:1147`），由同步/异步 handler `sync_transform_request_body` / `async_transform_request_body`（L1260/L1311）调用。原因是 Gemini 需要 sync+async 两个 handler 分别处理 auth token 获取（Google AI Studio 用 API key，Vertex 用 OAuth/Bearer）。

> 这是一个 **locality 信号**：当同一个 codec 的「请求构建」逻辑必须脱离 class 变成 free function 时，说明 class-as-interface 已经装不下「需要在不同 I/O 上下文复用」的逻辑。Stravia 的 codec 已经是 free function 风格（`encoder.encode_request`、`decoder.parse_response`），这比 LiteLLM 的 class-bound `transform_request` 更适合多 transport 复用。

---

## 2. supported params / drop params 机制

### 2.1 两层过滤

LiteLLM 的参数处理分两层：

**第一层：全局 gate（`get_optional_params`，`litellm/utils.py:3811`）**
- URL: https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/litellm/utils.py#L3811
- `_check_valid_arg(supported_params)`（L3897）：遍历 `non_default_params`，不在白名单的归入 `unsupported_params`；若 `litellm.drop_params` 或 per-call `drop_params=True` 则静默 pop，否则抛 `UnsupportedParamsError(500)`。
- 特殊豁免：`user`/`stream`/`stream_options` 永远跳过检查（L3907）；`n==1` 跳过（langchain 默认值，L3911）；`max_retries` 跳过（L3914）。

**第二层：provider 自映射（`map_openai_params`）**
- 每个 provider 把 OpenAI 参数名翻译成 provider 参数名（如 `max_tokens`→Anthropic `max_tokens`、Gemini `max_output_tokens`；`stop`→Anthropic `stop_sequences`、Gemini `stop_sequences`）。

### 2.2 drop_params 的三态语义

`drop_params` 有全局 `litellm.drop_params` 和 per-call `drop_params` 两个来源，且**语义在 map_openai_params 内部被重新检查**——不是只在第一层 gate 检查。例如 GPT-5 的 `map_openai_params`（`gpt_5_transformation.py:213`）在 xhigh/temperature/sampling 分支里再次 `if litellm.drop_params or drop_params: ... else: raise`。

源码：`litellm/llms/openai/chat/gpt_5_transformation.py`
- URL: https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/litellm/llms/openai/chat/gpt_5_transformation.py

> **对 Stravia 的含义**：Stravia 的 `provider/negotiator.rs`（planner）做的是 Target/Route 协商，没有等价的「参数白名单 + drop」层。LiteLLM 这套机制的价值在于：**让一个 OpenAI-shape 的客户端请求可以安全落到任意 provider**，靠 drop + remap 吸收能力差。Stravia 若要保持「任意 ingress×任意 egress」的矩阵，需要一个类似的、**与 codec 解耦**的「supported params 声明 + drop 决策」module——但 Stravia 当前由各 codec encoder 内部各自处理 unsupported 字段，locality 散落。这是一个 deepening 候选：把「参数能力声明」从 encoder 里抽出来成为独立 module，让 encoder 只管「给定已过滤的参数，如何编码」。

---

## 3. 协议对照矩阵

### 3.1 supported_openai_params 对照

| OpenAI 参数 | OpenAI Chat (gpt) | GPT-5 | O-series | Anthropic | Gemini |
|---|---|---|---|---|---|
| temperature | ✅ | ✅(仅=1 或 effort=none) | ✅(仅=1) | ✅ | ✅(gemini-3 弃用警告) |
| top_p | ✅ | ⚠️(仅 effort=none) | ❌drop | ✅ | ✅(gemini-3 弃用) |
| max_tokens | ✅ | →max_completion_tokens | →max_completion_tokens | ✅(必填,有默认) | →max_output_tokens |
| tools | ✅ | ✅ | ⚠️(capability) | ✅ | ✅ |
| tool_choice | ✅ | ⚠️(supports_tool_choice) | ⚠️ | ✅(none/auto/any/tool) | ✅(none/required/auto/ANY+name) |
| response_format | ✅(非gpt-4) | ❌drop | ⚠️(capability) | →tool 或 output_format | →response_schema |
| reasoning_effort | — | ✅ | ✅(only) | →thinking(budget) | →thinkingConfig |
| thinking | — | — | — | ✅(原生) | ✅(→thinkingConfig) |
| web_search_options | ✅ | ❌drop | — | →hosted web_search tool | →googleSearch |
| parallel_tool_calls | ✅ | — | ⚠️ | →disable_parallel_tool_use | ⚠️(False+多tool则drop) |
| stop | ✅ | ❌drop | — | →stop_sequences | →stop_sequences |
| frequency_penalty | ✅ | ❌drop | ❌drop | ❌ | ⚠️(非preview) |
| user | ✅ | ✅ | — | →metadata.user_id(非email) | — |

行号锚点（blob SHA `6a919ae`）：
- OpenAI gpt: `litellm/llms/openai/chat/gpt_transformation.py:129` (`get_supported_openai_params`)
- GPT-5: `litellm/llms/openai/chat/gpt_5_transformation.py:171` (`get_supported_openai_params`)；`map_openai_params` L213
- O-series: `litellm/llms/openai/chat/o_series_transformation.py:54`（docstring 明列 drop 矩阵）；`map_openai_params` L88
- Anthropic: `litellm/llms/anthropic/chat/transformation.py:433` (`get_supported_openai_params`)；`map_openai_params` L1392
- Gemini: `litellm/llms/vertex_ai/gemini/vertex_and_google_ai_studio_gemini.py:294` (`get_supported_openai_params`)；`map_openai_params` L1065

### 3.2 转换语义对照

| 维度 | OpenAI Chat | Anthropic | Gemini |
|---|---|---|---|
| **system message** | top-level `messages[role=system]` | 抽出为 top-level `system: []`，支持多块 + cache_control | 抽出为 `systemInstruction.parts[]`；不支持则 inline；仅 system 时注入空 user 消息(#13769) |
| **developer role** | 原生支持 | →system role（base 默认） | →system role（base 默认） |
| **tool_use→tool_call** | 原生 `tool_calls` | `content[].type=tool_use` → `tool_calls`（`extract_response_content` L2012） | `parts[].functionCall` → `tool_calls` |
| **tool_result→消息** | `role=tool` | `content[].type=tool_result`（必须在 tool_use 之后） | 紧跟 function call 的 `functionResponse` part |
| **stop reason** | `finish_reason`(stop/length/tool_calls/content_filter) | `stop_reason`(end_turn/tool_use/max_tokens/stop_sequence) → map_finish_reason | `finishReason`(STOP/MAX_TOKENS/SAFETY/RECITATION/...) → map |
| **usage** | `usage`(prompt/completion/total) | `usage`(input/output + cache_read/creation + server_tool_use) → `calculate_usage` L2120 | `usageMetadata`(promptTokenCount/...) + thoughtsTokenDetails |
| **reasoning** | `reasoning_content` / `reasoning` field | `thinking` blocks + `reasoning_content` 聚合 | `thoughtsTokenDetails` + thoughtSignature |

行号锚点：
- Anthropic system: `translate_system_message` L1607；`calculate_usage` L2120；`extract_response_content` L2012
- Gemini system: `_transform_system_message` (`transformation.py:1385`)；usage: `_calculate_usage` (`vertex_and_google_ai_studio_gemini.py:2363` 内调用)；finishReason: `get_flagged_finish_reasons` L2333
- OpenAI gpt response: `transform_response` L582；`_transform_choices` L507

---

## 4. 有证据的结论（10 条）

### C1. Anthropic 工具名卫生化：per-request 双向 map，单一 chokepoint

**证据**：`litellm/llms/anthropic/chat/transformation.py:130-242`（`_basic_sanitize_anthropic_tool_name` + `_build_anthropic_tool_name_maps`）；调用点 `transform_request` L1828-1845。

Anthropic 强制工具名 `^[a-zA-Z0-9_-]{1,128}$`。LiteLLM 在 `transform_request`（而非 `map_openai_params`）做卫生化，原因是这是 Anthropic/Bedrock-Anthropic/Vertex-Anthropic/Azure-Anthropic **共享的唯一边界**（都 `super().transform_request`）。关键设计：
- 建 **forward（original→sanitized）+ reverse（sanitized→original）双向 map**，reverse 只含被改写的项——所以本来就叫 `foo_bar` 的工具**不会被误回译**成 `foo/bar`。
- 冲突时加数字后缀 `_2/_3`，保持 128 上限。
- forward map 写入 messages，reverse map 存入 `litellm_params["_anthropic_tool_name_map"]`（**内部通道**，绝不进 `optional_params`——后者会被序列化进 JSON body 触发 400）。
- 响应侧 `transform_response`（L2461）读回 reverse map 还原工具名。

**测试覆盖**：`tests/test_litellm/llms/anthropic/chat/test_anthropic_chat_transformation.py:4490-4623`（7 个单测：基础替换、无冲突、与已有合法名冲突、两个 rewrite 撞同一目标、三方冲突、逆序冲突、重复 original）。

> **对 Stravia 的适用点**：Stravia 的 `codec/tool_correlation.rs` 已有 `normalize_request_tool_results`。若 Stravia 的 egress 是 Anthropic 协议，**工具名卫生化是 canonical invariant（tool correlation）的一部分**，应作为 tool_correlation module 的职责，而非散在各 codec。LiteLLM 的「reverse map 只含被改写项」是关键不变量，避免合法名被误译——Stravia 若做多协议中转（OpenAI ingress→Anthropic egress），工具名在往返中必须稳定。

### C2. Gemini thoughtSignature：嵌入 tool_call_id 的隐藏 seam

**证据**：`litellm/litellm_core_utils/prompt_templates/factory.py:66`（`THOUGHT_SIGNATURE_SEPARATOR = "__thought__"`）；`_encode_tool_call_id_with_signature` L1184；`_get_thought_signature_from_tool` L1203；`vertex_ai/gemini/transformation.py:637`（`_collect_tool_call_thought_signatures`）。

Gemini 的 thoughtSignature 是「必须回传给模型才能续接推理」的不透明 token。问题：OpenAI 客户端不知道这个字段。LiteLLM 的解法是把 signature **编码进 `tool_call_id`**（`call_<uuid>__thought__<base64>`），让现有 OpenAI 客户端无感透传，响应侧再解码还原。

核心不变量（`_collect_tool_call_thought_signatures` docstring，transformation.py:637-665）：每个 signature 只附在**一个** part 上；若同时附在 function-call part 和 text part，会在 gemini-3+ **双重计费**上一轮的 reasoning tokens。检测故意**不传 model 参数**调 `_get_thought_signature_from_tool`——因为该 helper 在 gemini-3 下会为未签名 tool call **合成假 signature**，传 model 会误抑制真实的 text-part signature。

**测试覆盖**：`tests/test_litellm/llms/vertex_ai/gemini/test_thought_signature_in_tool_call_id.py`（编码/解码往返、无 signature、响应内嵌、向后兼容、provider_fields 优先、convert 嵌入、e2e flow）。

> **对 Stravia 的适用点**：Stravia 的 canonical invariants 包含 reasoning 和 tool correlation。若 Stravia 要支持 Gemini egress，thoughtSignature 的「嵌入 ID」是**已知取舍**——它污染了 canonical tool_call_id（不再是纯 UUID）。替代方案是 Stravia 在 `ir/stream.rs` 或 `ir/cache.rs` 里用独立字段承载 signature（保持 ID 纯净），代价是 OpenAI-shape 客户端透传会丢失。**这是 Stravia 需要明确决策的 deepening 方向**：canonical IR 是否为 reasoning signature 预留独立槽位。

### C3. Anthropic thinking 与 reasoning_effort 的双向映射带模型能力探测

**证据**：`litellm/llms/anthropic/chat/transformation.py:1173`（`_map_reasoning_effort`）；`_cap_thinking_budget_to_max_tokens` L1232；`map_openai_params` 的 thinking/reasoning_effort 分支 L1505-1567。

LiteLLM 把 OpenAI 的 `reasoning_effort`（string：minimal/low/medium/high/xhigh/max/none）映射成 Anthropic 的 `thinking`（`{type:enabled, budget_tokens:N}`）。映射**不是纯查表**：
- 对 adaptive-thinking 模型（Opus 4.5+），映射成 `{type:adaptive}` 而非 budget；
- 非 adaptive 模型收到 `{type:adaptive}` 时，**降级**成 legacy `{type:enabled, budget_tokens:medium默认}` 再按 max_tokens 截断（L1505-1530）——Claude Code 会无条件发 adaptive；
- `_cap_thinking_budget_to_max_tokens`：budget 必须 < max_tokens，否则截到 `max_tokens-1`，太小则返回 None（drop thinking）；
- adaptive 模型额外写 `output_config.effort`（L1559）。

`is_thinking_enabled`（base L60）同时识别 `thinking.type==enabled` 和 `reasoning_effort is not None`，统一触发 `update_optional_params_with_thinking_tokens`（base L70：thinking 开启且未设 max_tokens → 补 budget+DEFAULT_MAX_TOKENS）。

**测试覆盖**：`tests/llm_translation/reasoning_effort_grid/`（整目录：`grid_spec.py` 定义 `CellExpectation(status, thinking_type, output_config_effort, thinking_budget_tokens, max_tokens)` 跨 ANTHROPIC_DIRECT/AZURE_AI/VERTEX_AI/BEDROCK 模型×11 种 effort 的期望矩阵；`test_reasoning_effort_grid.py` 执行）。Anthropic 专项：`tests/llm_translation/test_anthropic_completion.py:1046/1159/1196`（thinking 输出 / assistant 消息内 thinking / redacted_thinking）。Gemini 专项：`tests/llm_translation/test_gemini.py:1535/1656/1703`（anthropic thinking param→gemini-3 defaults / gemini-2 thinkingBudget / via map_openai_params）。

> **对 Stravia 的适用点**：reasoning 是 canonical invariant。LiteLLM 的「grid 测试」是一个**可借鉴的测试结构**——把「模型族 × reasoning 配置 → 期望 wire shape」做成声明式矩阵，避免散落的断言。Stravia 的 `tests/protocol_conversion.rs`（80KB）目前是行为级，若加 reasoning 跨协议矩阵，应 leverage 这个 grid 模式。但 Stravia **不适用** LiteLLM 的「在 map_openai_params 里硬编码模型名前缀探测」（`_is_opus_4_6_model` L324 等字符串匹配）——Stravia 已有 Provider Model snapshot（持久化模型能力），应 leverage 那个 module 而非字符串嗅探。

### C4. Gemini 仅 system 消息时注入空 user 消息

**证据**：`litellm/llms/vertex_ai/gemini/transformation.py:1376-1428`（`_default_user_message_when_system_message_passed` + `_transform_system_message`）；issue #13769。

Gemini 要求 `contents` 非空且角色交替。若请求**只有** system message（抽出后 messages 为空），Gemini 400。LiteLLM 注入一个空 user message 兜底。

**测试**：见 `test_gemini.py` 的 system-only 用例（grep 命中 `test_gemini_with_system_message_only` 类）。

> **对 Stravia 的适用点**：这是「协议语义补全」而非 canonical invariant 变形。Stravia 的 Gemini encoder 应在 codec 层处理（global codec 而非 vendor hook，因为这是 Gemini 协议本身的要求）。Stravia 的 `provider/vendor.rs` 注释里「Is it defined in the protocol spec? YES → global codec」正合此例。

### C5. Responses API 独立于 Chat 的 transform interface，且 OpenAI 自身基本直通

**证据**：`litellm/llms/base_llm/responses/transformation.py`（`BaseResponsesAPIConfig`，独立 ABC）；`openai/responses/transformation.py:30`（`OpenAIResponsesAPIConfig`）；`transform_response_api_response` 注释 `"No transform applied since outputs are in OpenAI spec already"`（L264）。

Responses 的 `get_supported_openai_params`（L77）直接用 `get_type_hints(ResponsesAPIRequestParams).keys()` 从类型生成白名单——**类型即契约**。OpenAI 自身只做：剥离 `cache_control`（Anthropic-only）、过滤 reasoning `status=None`（#13484）、`max_output_tokens` 下限 16。`transform_streaming_response`（L323）按 `type` 字段路由到对应 pydantic event model，validation 失败 fallback 到 `model_construct`。

> **对 Stravia 的适用点**：确认 Stravia 的 `protocol/codec/openai/responses/` 与 `protocol/codec/openai/compatible/` 分离是正确的 seam。LiteLLM 的「类型即白名单」对 Stravia 不直接适用（Rust 无运行时类型反射），但思路可 leverage：Stravia 可用一个 `ResponsesParam` enum 让编译器保证只接受合法变体。注意 ADR 0004：Stravia 把 Responses `web_search` 适配成隐藏 Platform Tool 轮次而非透传——这与 LiteLLM 的 Responses 直通策略**相反**，是 Stravia 有意的语义重塑。

### C6. Anthropic usage 的 cache token 归并与 iterations 聚合

**证据**：`litellm/llms/anthropic/chat/transformation.py:2106`（`is_anthropic_usage_object` 判别式）、`2120`（`calculate_usage`）。

Anthropic 把 prompt cache token 放在**顶层** `cache_read_input_tokens` / `cache_creation_input_tokens`（非嵌套）。`is_anthropic_usage_object` 用「有 input_tokens 且有任一 cache key」作为判别式——因为 Responses API usage 也有顶层 `input_tokens`，cache key 是唯一区分。`calculate_usage` 还处理：
- `iterations[]`（多次内部推理）：sum 各轮 input/output/cache tokens；
- `server_tool_use.web_search_requests` / `tool_search_requests`；
- `reasoning_tokens` = `min(token_counter(reasoning_content), completion_tokens)`——**估计值**，可能低估；
- `service_tier`、`inference_geo`、`speed` 透传。

**测试覆盖**：`tests/test_litellm/llms/anthropic/chat/test_anthropic_chat_transformation.py:84-205`（`test_calculate_usage` / clamps / mocked / nulls / server_tool_null）。

> **对 Stravia 的适用点**：usage 是 canonical invariant。LiteLLM 的「cache token 在 prompt_tokens 里**重复计入**」（`prompt_tokens += cache_creation + cache_read`，L2200）是一个**已知的统计口径选择**——总 token 会大于 input+output。Stravia 的 `ir/usage.rs` 若要对齐 OpenAI 的 `prompt_tokens_details.cached_tokens`（不计入 prompt_tokens），需明确这个口径差。这是兼容性风险点。

### C7. Gemini parallel_tool_calls=False + 多工具时静默 drop（非报错）

**证据**：`litellm/llms/vertex_ai/gemini/vertex_and_google_ai_studio_gemini.py:1213-1220`（`map_openai_params` 的 parallel_tool_calls 分支）。

```python
# Gemini does not support parallel_tool_calls=False with multiple
# tools. Drop the param instead of failing — Responses API clients
# often send parallel_tool_calls=false by default.
if not (value is False and num_tools > 1):
    optional_params["parallel_tool_calls"] = value
```

Gemini 不支持「禁用并行 + 多工具」，LiteLLM 选择**静默 drop** 而非报错，因为 Responses 客户端默认发 `parallel_tool_calls=false`。

> **对 Stravia 的适用点**：这是「语义损失优先于硬失败」的取舍。Stravia 的 ADR 0001 强调 canonical invariants（tool correlation）必须保持，但 parallel_tool_calls 的**控制语义**是否属于 canonical？若属于，Stravia 不应静默 drop；若只是 provider-specific 优化提示，drop 可接受。这是 Stravia 需要归类的**架构摩擦**，非缺陷。

### C8. Gemini finishReason 安全类短路，不进正常解析

**证据**：`vertex_and_google_ai_studio_gemini.py:2392-2408`（`_transform_google_generate_content_to_openai_model_response`）；`get_flagged_finish_reasons` L2333。

`_GEMINI_FINISH_REASON_KEYS`（L2339）含 STOP/MAX_TOKENS/SAFETY/RECITATION/BLOCKLIST/PROHIBITED_CONTENT/SPII/IMAGE_SAFETY/IMAGE_PROHIBITED_CONTENT。遇到 `promptFeedback.blockReason` 或 candidate 的安全类 finishReason，短路到 `_handle_blocked_response` / `_handle_content_policy_violation`，**不进正常 choice 解析**。

> **对 Stravia 的适用点**：stop reason 是 canonical invariant。Stravia 的 Gemini decoder 应把 Gemini 的安全类 finishReason 映射成一个**可被客户端观察**的 stop reason（而非吞掉）。LiteLLM 映射成 content_filter 风格——Stravia 需确认其 `ir/response.rs` 的 stop reason 枚举有对应变体。

### C9. O-series / GPT-5 的 system→user 降级与 temperature=1 硬约束

**证据**：`litellm/llms/openai/chat/o_series_transformation.py:54`（docstring drop 矩阵）、L88（`map_openai_params` temperature 约束）、L131（`_transform_messages` system→user）；`gpt_5_transformation.py:213`（`map_openai_params`）。

O-series 不支持 system message 时降级为 user；temperature 只接受 1（否则 drop 或报错）。GPT-5 继承并细化：`reasoning_effort=none` 时解锁灵活 temperature；xhigh 是 opt-in 能力（需模型 map 显式 True），minimal/low 是 opt-out（需显式 False 才禁）。`max_tokens` 在 GPT-5 全部映射成 `max_completion_tokens`（#13381）。

**测试覆盖**：`tests/llm_translation/test_openai_o1.py`；`test_anthropic_completion.py` 的 reasoning grid 覆盖 OpenAI 模型族。

> **对 Stravia 的适用点**：system→user 降级会**改变对话结构**（system 语义进入 user turn），影响 hook 可见上下文（ADR 0001 的 Context Rewrite）。Stravia 若支持 O-series egress，需决定降级发生在 codec 层（wire 兼容）还是 IR 层（hook 可见）。推荐 codec 层——保持 IR 的 system 语义纯净，降级是 wire mechanic。

### C10. drop_params 决策散落在 map_openai_params 内部，非集中 gate

**证据**：`gpt_5_transformation.py:213-300`（多个 `if litellm.drop_params or drop_params` 分支）；`o_series_transformation.py:104`；`anthropic/chat/transformation.py:384`（`_maybe_drop_speed_param`）。

虽然 `get_optional_params`（`utils.py:3897`）有全局 `_check_valid_arg`，但**模型级能力约束**（xhigh 支持、temperature 取值、speed 支持）在 `map_openai_params` 内部重新决策 drop/raise。这意味着 drop 逻辑**没有单一 chokepoint**——每个 provider config 各自重复 `if drop_params: pop else: raise` 模式。

> **对 Stravia 的不适用点**：Stravia **不应**复制这种散落模式。Stravia 的 deepening 方向是：一个独立的「参数能力 + drop 决策」module，输入 (protocol, model snapshot, params) 输出 (filtered params, dropped set)，让 codec encoder 只接收已过滤参数。这把 drop 决策的 locality 收拢到一个 module，leveraging Provider Model snapshot（持久化能力）而非字符串嗅探。

---

## 5. 各协议 transformation 关键源码 URL 索引

blob 前缀：`https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/`

### Base interface
- Chat BaseConfig: `litellm/llms/base_llm/chat/transformation.py`（L60 is_thinking_enabled, L84 should_fake_stream, L290 get_complete_url, L313 get_supported_openai_params, L328 map_openai_params, L346 transform_request, L378 transform_response）
- Responses BaseResponsesAPIConfig: `litellm/llms/base_llm/responses/transformation.py`
- Gemini google_genai base: `litellm/llms/base_llm/google_genai/transformation.py`

### OpenAI Chat
- `litellm/llms/openai/chat/gpt_transformation.py`（L129 supported params, L409 transform_request, L507 _transform_choices, L582 transform_response）
- `litellm/llms/openai/chat/gpt_5_transformation.py`（L171 supported params, L213 map_openai_params）
- `litellm/llms/openai/chat/o_series_transformation.py`（L54 drop matrix docstring, L88 map_openai_params, L131 system→user）

### OpenAI Responses
- `litellm/llms/openai/responses/transformation.py`（L30 class, L77 supported params, L96 map_openai_params, L137 transform_responses_api_request, L226 _handle_reasoning_item, L264 transform_response_api_response, L323 transform_streaming_response）

### Anthropic Messages
- `litellm/llms/anthropic/chat/transformation.py`（L130 tool name sanitize, L433 supported params, L613 _map_tool_choice, L656 _map_tool_helper, L876 _map_tools, L1173 _map_reasoning_effort, L1232 _cap_thinking_budget, L1392 map_openai_params, L1607 translate_system_message, L1779 transform_request, L2012 extract_response_content, L2106 is_anthropic_usage_object, L2120 calculate_usage, L2437 transform_response）

### Gemini GenerateContent
- `litellm/llms/vertex_ai/gemini/vertex_and_google_ai_studio_gemini.py`（L157 VertexGeminiConfig, L294 supported params, L1065 map_openai_params, L2333 flagged finish reasons, L2363 transform_response, L2520 transform_request[NotImplementedError]）
- `litellm/llms/vertex_ai/gemini/transformation.py`（L637 _collect_tool_call_thought_signatures, L689 _gemini_convert_messages_with_history, L1147 _transform_request_body, L1260 sync_transform_request_body, L1385 _transform_system_message）
- `litellm/llms/gemini/google_genai/transformation.py`（L38 GoogleGenAIConfig, L334 transform_generate_content_request）
- `litellm/llms/vertex_ai/google_genai/transformation.py`（VertexAIGoogleGenAIConfig 子类）

### Cross-cutting
- thought signature seam: `litellm/litellm_core_utils/prompt_templates/factory.py`（L66 separator, L1184 encode, L1203 decode）
- drop_params gate: `litellm/utils.py`（L3811 get_optional_params, L3897 _check_valid_arg, L2866 _should_drop_param）

---

## 6. 关键测试 URL 索引

blob 前缀同上。

- Anthropic tool name sanitize（7 测）: `tests/test_litellm/llms/anthropic/chat/test_anthropic_chat_transformation.py#L4490`（test_basic_sanitize... / test_build_..._no_collisions / ..._collision_with_existing_valid / ..._two_rewrites_to_same_target / ..._three_way_collision / ..._reverse_order_collision / ..._duplicate_originals）
- Anthropic usage（5 测）: 同文件 `#L84`（test_calculate_usage / clamps / mocked / nulls / server_tool_null）
- Anthropic thinking: `tests/llm_translation/test_anthropic_completion.py#L1046`（thinking_output / streaming / in_assistant_message / redacted）
- Anthropic tool_choice: 同上 `#L607`（test_map_tool_choice + 6 子变体）
- Gemini thought signature: `tests/test_litellm/llms/vertex_ai/gemini/test_thought_signature_in_tool_call_id.py`（encode/decode 往返、无 sig、响应内嵌、向后兼容、provider_fields 优先、convert 嵌入、e2e）
- Gemini function/usage: `tests/llm_translation/test_gemini.py`（L757 empty function args, L972 tool_use, L1465 unicode args, L616/1123 thinking）
- Gemini web search: `tests/llm_translation/test_gemini.py#L1874`（openai web_search→google_search）
- Reasoning grid（跨 provider 矩阵）: `tests/llm_translation/reasoning_effort_grid/grid_spec.py` + `test_reasoning_effort_grid.py`（ANTHROPIC_DIRECT/AZURE_AI/VERTEX_AI/BEDROCK × 11 efforts）
- OpenAI o1: `tests/llm_translation/test_openai_o1.py`
- OpenAI chat: `tests/llm_translation/test_openai.py`

---

## 7. 对 Stravia 的适用与不适用点

### 7.1 可借鉴（leveraging，非照搬）

| # | 机制 | Stravia deepening 方向 |
|---|---|---|
| L1 | Anthropic 工具名 per-request 双向 map（reverse 只含改写项） | 把工具名卫生化收进 `codec/tool_correlation.rs` 作为 tool correlation invariant 的一部分；reverse-only-rewritten 是关键不变量 |
| L2 | thoughtSignature 嵌入 tool_call_id 的取舍 | 评估 Stravia canonical IR 是否为 reasoning signature 预留独立槽位（`ir/stream.rs`/`ir/cache.rs`），避免污染 ID；若选嵌入，需在 codec 层且记录为已知 wire 取舍 |
| L3 | reasoning_effort grid 测试结构 | 在 `tests/protocol_conversion.rs` 增加声明式「模型族×reasoning配置→期望wire」矩阵，leveraging Provider Model snapshot 而非字符串嗅探 |
| L4 | Responses 独立 codec 目录 | 确认 Stravia 已有分离（`protocol/codec/openai/responses/` vs `compatible/`）正确；共享逻辑（cache_control 剥离）应 leverage 独立 `cache.rs` module |
| L5 | cache token 统计口径（Anthropic 重复计入 prompt_tokens） | 在 `ir/usage.rs` 明确口径：对齐 OpenAI 的 `cached_tokens`（不计入 prompt）还是 Anthropic（计入）；文档化为兼容性风险 |
| L6 | Gemini 安全类 finishReason 短路 | 确保 `ir/response.rs` stop reason 枚举有安全类变体，decoder 不吞掉 |

### 7.2 不适用 / Stravia 已有更好设计

| # | LiteLLM 做法 | Stravia 现状 | 不适用原因 |
|---|---|---|---|
| N1 | `BaseConfig` 混合抽象方法 + 通用 helper（thinking/response_format/developer role） | `Vendor` trait 只暴露 hook，通用行为在 `pipeline` free function | Stravia 的 hook/编排分离是更深 seam |
| N2 | `transform_request` class-bound，Gemini 不得不脱类成 free function | codec 已是 free function（`encoder.encode_request`） | Stravia 天然支持多 transport 复用 |
| N3 | 模型能力靠字符串前缀嗅探（`_is_opus_4_6_model` 等） | Provider Model snapshot 持久化能力 | Stravia 应 leverage snapshot module |
| N4 | drop_params 散落在每个 `map_openai_params` | Stravia 可建独立「参数能力+drop」module | LiteLLM 的散落是技术债，非目标 |
| N5 | Responses `web_search` 直通透传 | ADR 0004 适配成隐藏 Platform Tool 轮次 | Stravia 有意的语义重塑，保留 Web Provider 选择 |
| N6 | canonical IR = OpenAI shape | Stravia canonical IR = 自有 AiRequest | Stravia 不以 OpenAI 为归一中心，避免 OpenAI 演进绑架 IR |

### 7.3 推荐强度

- **强（应评估落地）**：L1（工具名卫生化进 tool_correlation）、L5（usage 口径文档化）、C7 归类（parallel_tool_calls 是否 canonical）。
- **中（测试结构借鉴）**：L3（reasoning grid）、L6（finish reason 枚举）。
- **弱（仅记录取舍）**：L2（thoughtSignature 槽位，取决于是否支持 Gemini egress）、L4（已落地）。

---

## 8. 方法论说明

- 所有源码引用基于本地 `git clone` 后 `git fetch --depth 1 origin 6a919aec...`，`git diff --stat 6a919ae` 在 6 个核心 transformation 文件上零改动，确认本地内容与 blob 一致。
- 仅读取源码、类型定义、测试；未运行测试（按指令跳过验证）。
- 未使用博客或二手文章；所有 URL 指向 `github.com/BerriAI/litellm` blob at `6a919ae`。
- 术语遵循 Stravia 词汇表；不使用 component/service/API/boundary 替代 module/interface/implementation/seam/adapter。

---

## 9. LiteLLM 流式/SSE 对照

> 本节补全头部「研究范围」声明的缺口（「不含流式」），研究 LiteLLM v1.97.0 的流式/SSE 转换、chunk 装配、tool call index/ID、reasoning、usage、finish reason、错误转换及相关官方测试。blob URL 与本文件头部 pinned SHA `6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573` 一致；本地核验 commit `ecba48dd` 与该 SHA 在 7 个流式关键文件（`streaming_chunk_builder_utils.py` / `streaming_handler.py` / `responses/streaming_iterator.py` / `responses/sse_output_recovery.py` / `anthropic/chat/handler.py` / `vertex_and_google_ai_studio_gemini.py` / `core_helpers.py`）逐文件行数完全一致。术语沿用 §0：canonical invariants = model、ID、tool correlation、reasoning、usage、stop reason。

### 9.1 流式装配的三层 seam

LiteLLM 的流式不是单点实现，而是三个 module 串联：

1. **provider SSE 迭代器**（`ModelResponseIterator`，每 provider 一份）：把上游 SSE/JSON 流逐帧解析成 `ModelResponseStream`（OpenAI chunk shape）。这是「上游 wire → OpenAI chunk」的 adapter。
2. **live streaming handler**（`CustomStreamWrapper`，`streaming_handler.py`）：逐 chunk 透传给客户端，做 chunk 级路由（`_dispatch_provider_chunk`）、终止帧合成（`finish_reason_handler`）、部分 usage 恢复。
3. **chunk builder**（`ChunkProcessor`，`streaming_chunk_builder_utils.py`）：流结束后把整段 chunk 序列「拍平」回单个 `ModelResponse`，用于缓存日志与 `include_usage`。**canonical invariants 在这里被最后裁决**——这是流式的 deep module。

> **对 Stravia 的含义**：Stravia 的 `ir/stream.rs` 若对应 live handler，需显式区分「逐 chunk 透传语义」与「装配后聚合语义」——两者对同一 invariant（尤其 tool_call id、finish reason）可能有不同真实性（见 §9.4 反例）。LiteLLM 把这两个 seam 混在 `CustomStreamWrapper` 里，locality 受损。

### 9.2 跨协议防御（防止语义损失，建议 leverage）

| # | 防御 | 位置 | invariant | 说明 |
|---|---|---|---|---|
| S1 | **Anthropic `message_start` cursor=1 重置** | `streaming_chunk_builder_utils.py:759`（`_reset_anthropic_cursor_completion_tokens`） | usage | last-wins 累加器会把 Anthropic 占位的 `output_tokens=1` 当真值钉住，绕过文本回退。实现用 `completion_usage_updates>=2 或 >1` 判定「非 cursor」，仅 provider-gated 在 `anthropic` 时重置为 0，迫使 `calculate_usage`（L796）的 `token_counter(text=...)` 回退。**唯一被严格单测的跨协议防御** |
| S2 | **cache TTL 跨事件保留** | 同文件 L66-87 + 注释 L628-634 | usage | Anthropic 5m/1h cache-creation 拆分只在 `message_start` 出现，`message_delta` 只带扁平 count；last-wins 会丢 1h 拆分 → 1h 缓存写按 5m 计费。实现把 message_start 拆分单独保存并 attach 回 |
| S3 | **按 tool_call.index 装配**（挽救 per-chunk UUID） | 同文件 L301-496（`_iter_tool_call_fragments` + `_join_fragments_by_index_and_field` + `get_combined_tool_content`） | tool correlation | 以 `(index, field)` 为分组键拼接 arguments，`id/name/type` 按 index last-wins。即使 provider 跨帧发不同 id，装配结果按 index 自洽 |
| S4 | **thinking_blocks 按 signature flush 重建** | 同文件 L520-575；Anthropic 侧 `anthropic/chat/handler.py:660-700` | reasoning | 累积 thinking 文本，遇 `signature` 即 flush 成 `ChatCompletionThinkingBlock`，遇 `redacted_thinking` 单独成块；Anthropic 侧在 signature 帧把「此前所有 thinking 文本」拼回该 block |
| S5 | **Gemini tool_calls/finishReason 分离帧记忆** | `vertex_and_google_ai_studio_gemini.py:3122-3179`（`_apply_stream_candidates`） | stop reason / tool correlation | `has_seen_tool_calls` 跨 chunk 记忆；Gemini 把 tool_calls 与 finishReason 放不同帧，最终帧 STOP 但无 tool_calls 会被错映射成 `stop`，此处改写为 `tool_calls`；并为无 content 的最终帧/metadata-only 帧回填空 delta choice |
| S6 | **部分 JSON / TCP 分片累积** | Anthropic `handler.py:1037-1100`（`_handle_accumulated_json_chunk`/`_parse_sse_data`）+ StopIteration 残留重试 L1096；Gemini 同文件 L3330-3370（`raw_decode` 逐值剥离） | 末帧/缺省 | Anthropic 累积到合法 JSON 再 parse；Gemini 用 `json.JSONDecoder().raw_decode` 逐值剥离（注释说明每片 parse 整 buffer 是 O(n²) 且占 GIL），仅当末字节属 `}]` 才尝试，`is_final` 时强排空 |
| S7 | **中断流部分 usage 恢复 + 可回退错误** | `streaming_handler.py:2102-2132`（`_record_partial_usage_for_failure`）+ L2128（`_handle_stream_fallback_error`） | usage / stop reason | 流中断时仍用 chunk builder 估出 partial usage+cost 挂 logging_obj；错误经 `exception_type` 映射后 4xx（除 429）直接抛，其余包成 `MidStreamFallbackError`（带 `generated_content`、`is_pre_first_chunk`）让 Router fallback |
| S8 | **Responses 末帧事件捕获 + SSE 重建** | `responses/streaming_iterator.py:211-363`（`_process_chunk`）；`responses/sse_output_recovery.py`（整文件）；`_build_synthetic_response_events` L1166-1318 | usage / reasoning | 捕获 `RESPONSE_COMPLETED/INCOMPLETE/FAILED` 分流日志；从裸 SSE 按 `output_index`（缺失落空槽）重建 output items；把非流式 response 合成完整事件序列（含 reasoning summary delta/done）。这是 Responses 事件协议装配的 deep module |

### 9.3 OpenAI-centric 兼容（仅 wire 兼容，损失 canonical 信息）

| # | 实现 | 位置 | 损失点 |
|---|---|---|---|
| W1 | **`map_finish_reason` 折叠进 OpenAI 5 值** | `core_helpers.py:78-127`（`_FINISH_REASON_MAP`）+ L140 | Anthropic `compaction→length`、`refusal→content_filter`；Gemini `MALFORMED_FUNCTION_CALL→stop`、`RECITATION/SAFETY/LANGUAGE/...→content_filter`、`TOO_MANY_TOOL_CALLS→stop`；OpenRouter `error→stop`。未映射值默认 `stop` 并告警。被折叠的原值**未保留**到 provider_specific_fields（deletion test：移除该映射则依赖 stop/length/tool_calls 的逻辑全错，但依赖原值的调用方已无法恢复） |
| W2 | **缺省 finish_reason 塌缩 `\"stop\"`** | `streaming_chunk_builder_utils.py:326-337`（`build_base_response`）+ `streaming_handler.py:1654-1668`（`finish_reason_handler`） | 流被取消/无末帧 → 终端 chunk 仍是干净 `\"stop\"`。调用方无法区分正常结束与中途断流 |
| W3 | **Anthropic error chunk 硬编码 500** | `anthropic/chat/handler.py:944-948` | 注释「Anthropic API does not return a status code in the chunk error」→ 全部映射成 500，真实 status 丢失 |
| W4 | **`<think>` 标签注入丢结构** | `streaming_handler.py:991-1026`（`_optional_combine_thinking_block_in_choices`，`merge_reasoning_content_in_choices=True`） | 把 `reasoning_content` 包进 `<think>…</think>` 注入 `content` 再 `del reasoning_content`，注释明说为 OpenWebUI。非语义保留，是 wire 形状适配且丢结构化字段 |
| W5 | **reasoning_tokens 用 `min(est, completion_tokens)` 估计** | `anthropic/chat/transformation.py:2220-2228` | 非真值；reasoning_content 被 redaction/截断时偏低。尽力而为非 invariant |

### 9.4 已证实缺陷

**D1. 多候选 `n>1` 装配塌缩为单一 choice（强）**
- 位置：`streaming_chunk_builder_utils.py:271-281`（`build_base_response` 硬编码 `choices:[{index:0,...}]`）；`get_combined_content` L503-518 对「所有 chunk 的所有 choice」的 `delta.content` 做同一字符串拼接。
- 触发：`n>1` 流式响应进入 `stream_chunk_builder`（默认日志/缓存路径）→ 多候选被合并成一条 message，per-choice finish_reason 丢失，cost 仅按单候选。
- 测试：未发现针对 `n>1` 装配的单测（cursor 测试等均假设单 choice）。
- LiteLLM 对照：live streaming 路径逐 chunk 透传多 choice，但装配这条 seam 是 shallow 的——它假设 OpenAI 单 choice 形状。

### 9.5 协议对照矩阵（流式 invariants × 实现）

| invariant | 防御点（§9.2） | 损失点（§9.3/§9.4） | 类别 |
|---|---|---|---|
| **model** | `_get_model_from_chunks` 偏好异于首帧的 model（Azure Model Router，L283-296） | — | 防御 |
| **ID** | `_get_chunk_id` 取首个非空 id（L262-269） | — | 防御 |
| **tool correlation** | 按 index 装配（S3） | Gemini per-chunk 新 UUID（见下）使 live 层 id 不稳定 | 防御+风险 |
| **reasoning** | thinking_blocks signature flush（S4） | `<think>` 注入丢结构（W4）；reasoning_tokens 估计（W5） | 防御+摩擦 |
| **usage** | cursor 重置（S1）/ cache TTL（S2）/ 部分 usage 恢复（S7） | — | 防御 |
| **stop reason** | Gemini seen_tool_calls 改写（S5） | 缺省塌缩 stop（W2）/ map 折叠损失（W1）/ Anthropic error→500（W3） | 防御+风险 |

**tool_call id 的 live/assembly seam 不一致（§9.4 补充）**：Gemini 对每个 functionCall chunk 合成新 `call_<uuid>.hex[:28]`（`vertex_and_google_ai_studio_gemini.py:1579`），仅 Gemini 3.5+ 优先用原生 `id`（L1586-1588），thought signature 还被编码进 id（L1591-1593，`_encode_tool_call_id_with_signature`）。结果：**逐 chunk 透传（live）层 id 不稳定，仅在 chunk builder 装配后靠 index 自愈**。依赖 live id 关联 tool_call 的 naive 客户端会把同一调用拆成多个。这与 C2（thoughtSignature 嵌入 ID）同源——LiteLLM 选择在 wire 层兼容 OpenAI 客户端，代价是污染 canonical tool_call_id。

### 9.6 对 Stravia 的适用 / 不适用

**适用（leveraging，建议评估落地）**

| # | LiteLLM 机制 | Stravia deepening 方向 | 强度 |
|---|---|---|---|
| LS1 | cursor 重置（S1）—— provider SSE 形状 → canonical invariant 修正，provider-gated + count-based 启发式 | 作为正面对照样板：Stravia 的 Anthropic/Gemini stream adapter 各设一个「usage 形状修正」seam，不让 last-wins 累加器静默错 | 强 |
| LS2 | 按 index 装配 tool_call（S3）+ 装配后 id 自愈 | 把「correlation key = index」提升为装配 interface 显式契约；对「无稳定原生 id」生成 per-call 稳定 id（hash of name+序号），让逐 chunk 层也保 id invariant，而非只在装配后自愈 | 强 |
| LS3 | 装配 module 升级为多 choice 分桶（修 D1） | 让「候选数 = 入站 choice 数」「终止状态显式」成为装配点可证 invariant，而非把 `n>1` 塌缩、把未知 finish 塌缩成 stop | 强 |
| LS4 | partial usage 恢复 + MidStreamFallbackError（S7） | 直接 leverage 该模式：中断流仍计部分 usage + 可路由 fallback | 强 |
| LS5 | `raw_decode`-peel 分片累积（S6）/ SSE output items 重建（S8） | Stravia 的 SSE 层/Responses 仿真直接 leverage 此 reconstruction 模式（经多 provider 验证的健壮性 seam） | 中 |
| LS6 | thinking_blocks 状态机（S4） | 抽成独立 module（当前散落在 chunk builder + Anthropic iterator 两处，locality 受损） | 中 |

**不适用 / Stravia 已有更好设计或需相反决策**

| # | LiteLLM 做法 | Stravia 取舍 | 原因 |
|---|---|---|---|
| LN1 | `map_finish_reason` 把 provider 原因折叠进 OpenAI 5 值（W1），原值不保留 | Stravia 应保留原值到 provider_specific_fields，OpenAI 5 值仅作 adapter 输出 | stop reason 真实性是 canonical invariant，LiteLLM 牺牲 it 换 wire 兼容 |
| LN2 | `<think>` 标签注入 content 并 del reasoning（W4） | Stravia 若做同类只作可选 adapter，不进 canonical path | 丢结构化字段，非语义保留 |
| LN3 | 缺省 finish_reason 塌缩 `\"stop\"`（W2） | Stravia 应区分 stop / truncated / unknown | LiteLLM 把未知塌缩成 stop 掩盖截断 |
| LN4 | Anthropic error chunk 硬编码 500（W3） | Stravia 应解析 error 事件 type 反推 status | LiteLLM 损失真实 status |

### 9.7 流式关键源码 / 测试 URL 索引

blob 前缀：`https://github.com/BerriAI/litellm/blob/6a919aec6a2a0c54cc6a2e6f67ff7b236a3a2573/`

**核心装配 / handler**
- `litellm/litellm_core_utils/streaming_chunk_builder_utils.py`（L62 cache TTL, L88 sort, L262 chunk id, L271 build_base_response, L301 tool fragments, L520 thinking flush, L586 usage per chunk, L759 cursor reset, L796 token_counter fallback）
- `litellm/litellm_core_utils/streaming_handler.py`（L507 openai chunk handler, L1027 dispatch, L1654 finish_reason_handler, L2102 partial usage, L2128 fallback error）
- `litellm/litellm_core_utils/core_helpers.py`（L78 `_FINISH_REASON_MAP`, L140 `map_finish_reason`）
- `litellm/litellm_core_utils/exception_mapping_utils.py`（L2157 `exception_type`）

**provider SSE 迭代器 / chunk_parser**
- `litellm/llms/anthropic/chat/handler.py`（L517 ModelResponseIterator, L596 usage, L604 content_block_delta, L753 chunk_parser, L944 error→500, L1037 partial JSON, L1096 StopIteration 残留）
- `litellm/llms/vertex_ai/gemini/vertex_and_google_ai_studio_gemini.py`（L1579 per-chunk UUID, L1993 create_streaming_choice, L3098 streaming error, L3122 seen_tool_calls, L3201 usage metadata, L3231 chunk_parser, L3330 raw_decode peel）

**Responses 流式**
- `litellm/responses/streaming_iterator.py`（L151 BaseResponsesAPIStreamingIterator, L211 _process_chunk, L1166 _build_synthetic_response_events）
- `litellm/responses/sse_output_recovery.py`（L60 record_output_item_chunk, L88 record_output_text_chunk）

**测试**
- cursor 重置（唯一被严格单测的跨协议防御，9 例）: `tests/test_litellm/litellm_core_utils/test_streaming_chunk_builder_cursor.py`（`TestAnthropicCursorBug` / `TestProviderGuard`：cursor-only reset / cursor+delta / token_counter 回退 / cache 保留 / OpenAI 不受影响 / 合法单 token / provider 守卫 / unknown provider / cache-only chunk）
- 该测试目录下未见针对 `n>1` 装配、tool id 跨帧不一致、cache TTL 计费的单测——属测试覆盖盲区。

### 9.8 证据强度与盲区说明

- **已证实缺陷（D1）**：源码逻辑可直接推出错误后果且无单测覆盖 → 高置信。
- **跨协议防御（S1-S8）**：源码 + 注释 + S1 有完整单测 → 高置信。
- **OpenAI-centric 兼容（W1-W5）**：行为真实存在，是否构成「损失」取决于调用方是否依赖被压缩信息 → 中置信。
- **测试盲区**：`n>1` 装配、tool id 跨帧不一致、cache TTL 计费均无单测；cursor 重置是唯一被严格单测的跨协议防御。
- 所有行号已在 commit `6a919aec` 的 blob 上逐一核验（与本地 `ecba48dd` 在 7 个流式关键文件逐文件行数一致）。


---

## 10. Rust 社区实现盘点

调查日期同本文：2026-08-09。结论：**没有一个 Rust 项目同时达到 LiteLLM 的成熟度与 Stravia 的四协议双向 ingress/egress 覆盖**。可用参考应拆成“成熟网关 implementation”“Rust transformation seam”“纯映射测试面”三类，而不是寻找单一替代品。

| 项目 | 成熟度 | 协议转换范围 | 对 Stravia 的价值 |
|---|---|---|---|
| [agentgateway](https://github.com/agentgateway/agentgateway) | 成熟：Linux Foundation、v1.4.x、4.2k+ stars、活跃维护 | Chat Completions / Responses / Messages 可按 provider matrix native 或 translation；无 Gemini client ingress | 生产级 gateway policy、translation matrix 与测试组织；translation implementation 与通用代理耦合较深 |
| [api7/aisix](https://github.com/api7/aisix) | 部分成熟：v0.8.1、APISIX 团队、165 个 E2E 文件 / 426 cases；项目仅约四个月 | OpenAI Chat/Responses 与 Anthropic Messages 双向；Vertex/Gemini 仅 upstream | 最接近 Stravia 的 Rust `Bridge` + `ChatFormat` IR + provider crate 架构；适合审查 seam、stream、error translation |
| [anyllm_translate](https://docs.rs/crate/anyllm_translate/latest) | 不成熟：v0.16.0、单维护者、采用度低 | 以 Anthropic Messages 为中心连接 OpenAI Chat/Responses、Gemini 的 IO-free 双向 mapping；没有任意协议对直连 | 唯一可直接作为 crate 引入的候选，但会引入 Anthropic-shaped 二级 IR；不能替代 Stravia canonical IR |
| [majiayu000/litellm-rs](https://github.com/majiayu000/litellm-rs) | 部分成熟：v0.5、活跃，但核心贡献高度集中于单人 | OpenAI-compatible ingress → 多 provider native upstream；无 Anthropic/Gemini client ingress | 可参考 provider catalog/routing；不是四协议 transformation 基线 |
| [LiteLLM-Labs/litellm-rust](https://github.com/LiteLLM-Labs/litellm-rust) | PoC，不可生产使用 | `/messages`、`/responses` 与少数 provider transformation | 只参考“provider transformation 不做 IO”的 seam；README 已明确真正迁移回 LiteLLM 主仓库 |

一手依据：

- [agentgateway 1.4.x provider translation matrix](https://agentgateway.dev/docs/standalone/latest/)
- [AISIX README 与协议范围](https://github.com/api7/aisix)；[v0.8.1 release](https://github.com/api7/aisix/releases/tag/v0.8.1)；[`Bridge` trait](https://github.com/api7/aisix/blob/main/crates/aisix-gateway/src/bridge.rs)
- [anyllm_translate 0.16.0 文档与版本记录](https://docs.rs/crate/anyllm_translate/latest)
- [litellm-rs 0.5.0 crate](https://docs.rs/crate/litellm-rs/latest)；其 GitHub contributor 数据显示主要 implementation 来自单一维护者
- [LiteLLM 官方 Rust 迁移 issue #31263](https://github.com/BerriAI/litellm/issues/31263)：截至调查日仍是分阶段迁移计划，完整 Rust server 尚未完成

**推荐参考组合**：用 AISIX 检查 production Rust seam 与生命周期，用 anyllm_translate 对照四协议纯 mapping，用 agentgateway 的 provider matrix 组织 conversion contract；继续以 LiteLLM Python implementation 作为字段覆盖与长期兼容行为的主要一手来源。

---

## 11. 第三方 Rust transformation 能否承载 Stravia Hook

### 11.1 必须保持的 seam

Stravia 要保持的执行顺序是：

```text
client wire
  → ProtocolPair::decode_request
  → AiRequest
  → HookRuntime::on_request（可原子改写）
  → Vendor canonical mutation
  → ProtocolPair::encode_request
  → Stravia-owned HTTP/auth
  → ProtocolPair provider decode → AiResponse / AiStreamDelta
  → HookRuntime（response / stream）
  → ProtocolPair client encode
  → client wire
```

因此 transformation library 不能“拥有 Hook”。正确关系只能是：**library 藏在 codec implementation 后，Hook 继续只观察 Stravia canonical IR**。候选若拥有 HTTP、auth、routing、retry 或 stream delivery，或要求用 vendor-specific 类型替代 `AiRequest` / `AiResponse` / `AiStreamDelta`，就与 Inference Run、Vendor adapter 和 HookRuntime seam 冲突。

### 11.2 可嵌入性对比

| 候选 | 可直接 Cargo 依赖 | transformation 形状 | stream 形状 | Hook 适配 | 判定 |
|---|---|---|---|---|---|
| agentgateway `agent-llm` | **否**。workspace `version = "0.0.0"`、`publish = false`；只能 pin git SHA，并连带内部 `agent-core` / `agent-http`、CEL fork、async-openai fork 等 | wire type 到 wire type 的 pairwise matrix；没有 neutral canonical IR | 直接包装 `axum_core::body::Body` | 必须重写 Hook 类型，或新增 `AiRequest ↔ agent_llm::*` 重复 mapping | **Not suitable** |
| AISIX `aisix-gateway` / provider crates | **否**。未发布 crates.io；workspace library version 仍为 0.3.0，而产品 tag 已到 v0.8.1；wire module 明确不承诺 public-SDK 稳定性 | `ChatFormat` 是 OpenAI Chat superset；tools 多为 `Value` passthrough，reasoning 只有字符串槽位 | `Bridge::chat_stream` 返回 `BoxStream<ChatChunk>`，同时 Bridge 拥有 reqwest、auth、deadline | `Guardrail` 只能 Allow/Block/Bypass，不能结构化改写；采用 Bridge 会取代 Stravia Vendor seam | **Not suitable；只适合 fork/extract** |
| `anyllm_translate = "0.16"` | **是**。MIT；`default-features = false` 时只有 serde/thiserror/tracing/uuid，无 IO | Anthropic Messages 是 hub；其它协议只与 Anthropic 成对映射，不是 neutral IR | 四个 stateful translator，单消息/单 choice | 能放在 codec 内，但 Stravia 仍须自写 `AiRequest ↔ MessageCreateRequest`；由此新增二级 IR 和一次有损转换 | **Partial** |

### 11.3 决定性的语义缺口

**agentgateway**

- 非流式 conversion 有纯函数，但类型依赖 `agent-core::Strng`、schema 宏与 async-openai fork，不能独立抽取。
- stream conversion 持有 Axum HTTP body lifecycle；canonical delta 出现前已进入第三方 delivery 模型，无法直接插入 Stravia `StreamTransformer`。
- conversion 是协议对矩阵，不提供供 Hook 读写的统一 request/response representation。

**AISIX**

- `Bridge` 明确负责 transformation、HTTP、authorization、deadline 与 stream；retry/fallback 又在未导出的 `aisix-proxy::retrying_dispatch`。只取 Bridge 会夺走 Vendor seam，取完整 proxy 会夺走 Inference Run。
- `ChatFormat` 无法类型化表达 Platform Tool、Tool Continuation、Response Chain 或 reasoning signature；跨 provider 非文本 block 还存在 documented silent drop。
- 唯一 extension seam `Guardrail::check_input(&ChatFormat)` / `check_output(&ChatResponse)` 接收不可变引用，只能给 verdict；不是 Hook 的读写 interface。
- 可复用的 `wire.rs` 和 `responses_bridge.rs` 是内部纯函数，但没有独立 crate，源码明确声明不是稳定 public interface。

**anyllm_translate**

- 唯一满足“已发布 + default 无 IO”的候选，但其拓扑是：

```text
OpenAI Chat ↔ Anthropic Messages ↔ OpenAI Responses
                         ↕
                       Gemini
```

- 不存在 `openai_to_gemini`、`gemini_to_openai`、`chat_to_responses` 等任意协议对 direct mapping。接入 Stravia egress 时必须先做 `AiRequest → Anthropic MessageCreateRequest → target wire`。
- 已核验的硬损失：`n > 1` 被剥到单 choice；Anthropic stream `SignatureDelta` 被丢弃；Responses thinking 被 strip；`cache_creation_input_tokens` 恒为 `None`；部分 Gemini thinking/document/URL image 被丢弃。
- `LossyBehavior::Error` 没有统管上述损失；多处仍是固定 warning 或 silent drop，不能实现 Stravia 所需的统一 fail-closed representability gate。

### 11.4 推荐

1. **不让任何候选承载 Hook lifecycle。** `HookRuntime`、`InferenceRun`、`AiRequest` / `AiResponse` / `AiStreamDelta` 保持 Stravia-owned。
2. **不把 agentgateway 或 AISIX 作为 live dependency。** 两者适合作为 conversion contract 与实现参考；它们没有稳定、轻量、无 IO 的 published transformation crate。
3. **若必须引入现成 crate，只考虑 `anyllm_translate` 的窄范围纯函数，且关闭 `middleware` feature。** 适合借用 tool-name correlation、schema sanitize、error mapping；不适合作为四协议主转换路径。
4. **若目标仍是“删除大部分自有 codec”**，现有三个候选都不能同时满足。可行路线只能是 fork/extract 一个新的纯 transformation module，并让它直接以 Stravia canonical IR 为 interface；这意味着 Stravia 仍要拥有该 fork 的语义与升级责任。

一手依据：

- [agentgateway workspace publication settings](https://github.com/agentgateway/agentgateway/blob/34610af2edfc0d775820da76ef91839311a2bb93/Cargo.toml#L41-L46)、[`agent-llm` dependencies](https://github.com/agentgateway/agentgateway/blob/34610af2edfc0d775820da76ef91839311a2bb93/crates/llm/Cargo.toml)、[Axum Body stream transformer](https://github.com/agentgateway/agentgateway/blob/34610af2edfc0d775820da76ef91839311a2bb93/crates/llm/src/parse/transform.rs)
- [AISIX `Bridge`](https://github.com/api7/aisix/blob/35a71b53cd81dd00b222d783bc96784d6be3da0d/crates/aisix-gateway/src/bridge.rs#L609-L660)、[`ChatFormat`](https://github.com/api7/aisix/blob/35a71b53cd81dd00b222d783bc96784d6be3da0d/crates/aisix-gateway/src/chat.rs#L246-L289)、[`Guardrail`](https://github.com/api7/aisix/blob/35a71b53cd81dd00b222d783bc96784d6be3da0d/crates/aisix-guardrails/src/lib.rs#L449-L462)
- [`anyllm_translate` manifest](https://github.com/whit3rabbit/anyllm-proxy/blob/75a5a3a230a3f26196cadecfa1a5378e804f2493/crates/translator/Cargo.toml)、[mapping modules](https://github.com/whit3rabbit/anyllm-proxy/blob/75a5a3a230a3f26196cadecfa1a5378e804f2493/crates/translator/src/mapping/mod.rs)、[`LossyBehavior`](https://github.com/whit3rabbit/anyllm-proxy/blob/75a5a3a230a3f26196cadecfa1a5378e804f2493/crates/translator/src/config.rs#L4-L14)

### 11.5 在允许复制源码时的选择

若允许把上游源码复制并改造成 Stravia-owned implementation，**首选 donor 是 `anyllm_translate` 0.16.0（release commit `75a5a3a`）**：

- transformation 默认无 IO，源码与测试已独立成 crate，不必先从 gateway/routing/auth 中剥离；
- 同时有 Anthropic、OpenAI Chat、OpenAI Responses、Gemini wire types 和四套 stream state machine；
- 494 个纯 mapping tests 可转成 Stravia canonical contract tests；
- MIT license 比复制 agentgateway/AISIX 的跨 crate Apache-2.0 implementation 更易维护归属。

但复制必须是**选择性移植并直接改写为 Stravia IR hub**，不能原样保留 `MessageCreateRequest` hub，也不能保留 `middleware`、model map、HTTP 或 proxy modules。目标 topology 是：

```text
OpenAI Chat ─┐
Responses ───┼─ decode/encode ↔ AiRequest/AiResponse/AiStreamDelta ↔ HookRuntime
Anthropic ───┤
Gemini ──────┘
```

不是：

```text
AiRequest ↔ Anthropic MessageCreateRequest ↔ target wire
```

agentgateway 排第二，仅作为 conversion matrix 和 fixture donor；AISIX 排第三，仅作为 Bridge/error/usage lifecycle donor。两者的可移植代码散落在 IO-owning gateway crates 中，抽取成本和不必要依赖均高于 `anyllm_translate`。

后续 grilling 决策已记录为 [ADR-0006：Own Protocol Conversion behind a pair-bound canonical interface](../adr/0006-own-protocol-conversion-behind-canonical-stages.md)。