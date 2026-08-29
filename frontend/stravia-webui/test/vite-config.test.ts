import { afterEach, describe, expect, test } from 'bun:test'
import config from '../vite.config'

const originalPort = process.env.STRAVIA_PORT

afterEach(() => {
  if (originalPort === undefined) {
    delete process.env.STRAVIA_PORT
  } else {
    process.env.STRAVIA_PORT = originalPort
  }
})

describe('Vite development proxy', () => {
  test('prefers the current STRAVIA_PORT over the repository .env value', async () => {
    process.env.STRAVIA_PORT = '45678'

    expect(typeof config).toBe('function')
    if (typeof config !== 'function') {
      throw new Error('expected a mode-aware Vite config')
    }

    const resolved = await config({ command: 'serve', mode: 'development', isSsrBuild: false, isPreview: false })
    const proxy = resolved.server?.proxy?.['/api/v1']

    expect(proxy).toBeObject()
    if (!proxy || typeof proxy === 'string') {
      throw new Error('expected an object proxy configuration')
    }
    expect(proxy.target).toBe('http://127.0.0.1:45678')
  })
})
