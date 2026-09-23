import { expect, test } from 'bun:test'

import { resolvePluginText } from '../src/lib/plugin-text'

test('plugin text uses only exact locale and required English fallback', () => {
  const text = { 'zh-CN': '简体', 'en-US': 'English', 'zh-TW': '繁體' }
  expect(resolvePluginText(text, 'zh-CN')).toBe('简体')
  expect(resolvePluginText(text, 'zh-TW')).toBe('繁體')
  expect(resolvePluginText(text, 'zh-HK')).toBe('English')
  expect(resolvePluginText({ 'zh-CN': '简体', 'en-US': 'English' }, 'fr-FR')).toBe('English')
})
