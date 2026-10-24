/**
 * 控件语义宽度（DESIGN.md Fields 与表单）：Field 的 size 与设置行的 controlSize 共用。
 * 名称、地址、密钥与多行文本等自由文本不设宽度，占满设置列。
 */
export const fieldControlWidths = { number: '7rem', setting: '16rem', datetime: '18rem' } as const

export type FieldControlSize = keyof typeof fieldControlWidths
