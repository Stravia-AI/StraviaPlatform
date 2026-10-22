/** 控件语义宽度（DESIGN.md Fields 与表单）；Field 的 size 与设置行的 controlSize 共用同一组取值。 */
export const fieldControlWidths = {
  number: '7rem',
  setting: '16rem',
  datetime: '18rem',
  name: '24rem',
  select: '28rem',
  fill: '36rem',
} as const

export type FieldControlSize = keyof typeof fieldControlWidths
