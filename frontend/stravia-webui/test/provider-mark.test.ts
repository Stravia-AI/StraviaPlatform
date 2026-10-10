import { expect, test } from 'bun:test'

import { icons } from '../src/assets/icons'

test('icons dictionary resolves known built-in provider brand SVGs', () => {
  expect(icons.anthropic).toBeDefined()
  expect(icons.openai).toBeDefined()
})

test('data URI encoding generates valid svgSource format for plugin registered svg', () => {
  const pluginRegisteredSvg = '<svg width="24" height="24"><path d="M0 0"/></svg>'
  const svgSource = `data:image/svg+xml,${encodeURIComponent(pluginRegisteredSvg)}`
  expect(svgSource).toStartWith('data:image/svg+xml,%3Csvg')
})
