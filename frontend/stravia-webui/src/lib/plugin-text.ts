import type { LocalizedText } from '#lib/types.js'

/** Plugin text is selected by exact locale, then the contract's required default. */
export function resolvePluginText(text: LocalizedText, locale: string): string {
  return text[locale] ?? text['en-US']
}

function comparable(text: string): string {
  return text.toLocaleLowerCase().replace(/[^\p{L}\p{N}]+/gu, '')
}

/**
 * 插件字段说明只是复述 label（可再加供应商名称）时返回 true：这类说明没有信息量，界面直接省略。
 * 包含约束、数据流向等额外内容的说明不会被判为复述。
 */
export function restatesLabel(description: string, label: string, ...aliases: string[]): boolean {
  let rest = comparable(description)
  if (!rest) return true
  const key = comparable(label)
  if (key && rest.includes(key)) rest = rest.replace(key, '')
  else if (rest !== key) return false
  for (const alias of aliases) {
    const name = comparable(alias)
    if (name && rest.includes(name)) rest = rest.replace(name, '')
  }
  return rest === ''
}
