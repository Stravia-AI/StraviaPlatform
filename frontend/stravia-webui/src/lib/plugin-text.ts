import type { LocalizedText } from '$lib/types'

/** Plugin text is selected by exact locale, then the contract's required default. */
export function resolvePluginText(text: LocalizedText, locale: string): string {
  return text[locale] ?? text['en-US']
}
