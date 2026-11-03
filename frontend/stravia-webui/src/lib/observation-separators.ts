/**
 * 引用正文的拼接分隔符，是管理读取 API 的契约（见 docs/design/interaction-observation.md
 * 「摘要带引用，正文按需取」）：与服务端 `input_text` / `output_text` 的拼接规则一致。
 * 前端只在此处定义，不在组件里各写一份。
 */
/** 输入：多个文本单元以单换行连接。 */
export const INPUT_SEPARATOR = '\n'
/** 输出：块内文本单元以空行连接。 */
export const OUTPUT_SEPARATOR = '\n\n'
