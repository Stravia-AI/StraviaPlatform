import { expect, test } from 'bun:test'

import { resolvePluginText, restatesLabel } from '../src/lib/plugin-text'

test('plugin text uses only exact locale and required English fallback', () => {
  const text = { 'zh-CN': '简体', 'en-US': 'English', 'zh-TW': '繁體' }
  expect(resolvePluginText(text, 'zh-CN')).toBe('简体')
  expect(resolvePluginText(text, 'zh-TW')).toBe('繁體')
  expect(resolvePluginText(text, 'zh-HK')).toBe('English')
  expect(resolvePluginText({ 'zh-CN': '简体', 'en-US': 'English' }, 'fr-FR')).toBe('English')
})

test('plugin descriptions that only restate the label are recognised', () => {
  expect(restatesLabel('OpenAI API key.', 'API key', 'OpenAI')).toBe(true)
  expect(restatesLabel('OpenAI API 密钥。', 'API 密钥', 'OpenAI')).toBe(true)
  expect(restatesLabel('API key', 'API key')).toBe(true)
  expect(restatesLabel('OpenAI API key.', 'API key')).toBe(false)
  expect(restatesLabel('Credential sent to the configured endpoint.', 'API key', 'OpenAI')).toBe(false)
  expect(restatesLabel('API key used for billing in this workspace.', 'API key', 'OpenAI')).toBe(false)
})
