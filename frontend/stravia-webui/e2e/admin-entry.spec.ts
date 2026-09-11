import { chromium, expect, test, type Browser, type Page } from '@playwright/test'
import { spawn, execFileSync, type ChildProcess } from 'node:child_process'
import { createHash, X509Certificate } from 'node:crypto'
import { once } from 'node:events'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { createServer as createHttpServer, request, type Server, type RequestListener } from 'node:http'
import { createServer as createHttpsServer } from 'node:https'
import { tmpdir } from 'node:os'
import { resolve, join } from 'node:path'
import { stripVTControlCharacters } from 'node:util'

const root = resolve(import.meta.dirname, '../../..')
const binary =
  process.env.STRAVIA_BINARY ??
  join(root, 'target/release', process.platform === 'win32' ? 'stravia-server.exe' : 'stravia-server')
const password = 'correct horse battery staple'

async function listen(server: Server): Promise<number> {
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const address = server.address()
  if (!address || typeof address === 'string') throw new Error('Expected TCP listener')
  return address.port
}

async function close(server: Server): Promise<void> {
  const stopped = new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())))
  server.closeAllConnections()
  await stopped
}

async function api(page: Page, path: string, method = 'GET', body?: unknown): Promise<number> {
  return page.evaluate(
    async ({ path, method, body }) => {
      const response = await fetch(path, {
        method,
        headers: { 'content-type': 'application/json', 'x-stravia-csrf': '1', 'x-entry-probe': '1' },
        body: body === undefined ? undefined : JSON.stringify(body),
      })
      await response.arrayBuffer()
      return response.status
    },
    { path, method, body },
  )
}

async function isolatedContext(browser: Browser) {
  const context = await browser.newContext()
  await context.addInitScript(() => localStorage.setItem('stravia-locale', 'en-US'))
  // 更新检查不属于入口契约；只隔离此端点，认证、设置和管理准入均走真实 Server。
  await context.route('**/api/v1/updates/check', (route) =>
    route.fulfill({
      json: {
        data: {
          current_version: '0.0.0',
          check_status: 'up-to-date',
          last_success_at: null,
          last_failure: null,
          available_update: null,
          skipped: false,
          download_supported: false,
        },
      },
    }),
  )
  return context
}

test('HTTP management and HTTPS proxy sessions retain browser cookie boundaries', async () => {
  test.setTimeout(120_000)
  const directory = await mkdtemp(join(tmpdir(), 'stravia-entry-browser-'))
  const key = join(directory, 'key.pem')
  const certificate = join(directory, 'cert.pem')
  let backendPort = 0
  const proxyCookies: string[] = []
  const proxy =
    (scheme: 'http' | 'https'): RequestListener =>
    (incoming, outgoing) => {
      // 只捕获显式探针，不依赖页面后台刷新会话的请求次数。
      if (scheme === 'http' && incoming.url === '/api/v1/auth/refresh' && incoming.headers['x-entry-probe'] === '1') {
        proxyCookies.push(incoming.headers.cookie ?? '')
      }
      const headers = { ...incoming.headers }
      delete headers.forwarded
      delete headers['x-forwarded-for']
      headers.host = `127.0.0.1:${backendPort}`
      headers['x-forwarded-host'] = incoming.headers.host
      headers['x-forwarded-proto'] = scheme
      const upstream = request(
        { host: '127.0.0.1', port: backendPort, path: incoming.url, method: incoming.method, headers },
        (response) => {
          outgoing.writeHead(response.statusCode ?? 502, response.headers)
          response.pipe(outgoing)
        },
      )
      upstream.on('error', () => {
        if (outgoing.headersSent) {
          outgoing.destroy()
        } else {
          outgoing.writeHead(502)
          outgoing.end()
        }
      })
      incoming.pipe(upstream)
    }
  const http = createHttpServer(proxy('http'))
  // 后端的自动目录／更新检查也只能访问隔离代理，不能联系生产服务。
  http.on('connect', (_request, socket) => socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n'))
  let https: Server | undefined
  let backend: ChildProcess | undefined
  let browser: Browser | undefined
  try {
    execFileSync(
      'openssl',
      [
        'req',
        '-x509',
        '-newkey',
        'rsa:2048',
        '-nodes',
        '-days',
        '1',
        '-keyout',
        key,
        '-out',
        certificate,
        '-subj',
        '/CN=entry.test',
        '-addext',
        'subjectAltName=DNS:entry.test,DNS:denied.test',
      ],
      { stdio: 'ignore' },
    )
    const pem = await readFile(certificate)
    const spki = createHash('sha256')
      .update(new X509Certificate(pem).publicKey.export({ type: 'spki', format: 'der' }))
      .digest('base64')
    https = createHttpsServer({ key: await readFile(key), cert: pem }, proxy('https'))
    const httpPort = await listen(http)
    const httpsPort = await listen(https)
    const plainOrigin = `http://entry.test:${httpPort}`
    const secureOrigin = `https://entry.test:${httpsPort}`
    // 仅此临时证书公钥受信，不修改系统信任库或全局关闭证书校验。
    browser = await chromium.launch({
      args: [
        '--host-resolver-rules=MAP entry.test 127.0.0.1, MAP denied.test 127.0.0.1',
        '--no-proxy-server',
        `--ignore-certificate-errors-spki-list=${spki}`,
      ],
    })
    backend = spawn(
      binary,
      [
        '--host',
        '127.0.0.1',
        '--port',
        '0',
        '--log-level',
        'info',
        '--data-dir',
        directory,
        '--trusted-proxy',
        '127.0.0.1',
        '--admin-origin',
        plainOrigin,
        '--admin-origin',
        secureOrigin,
      ],
      {
        cwd: directory,
        stdio: ['ignore', 'pipe', 'pipe'],
        env: {
          ...process.env,
          NO_COLOR: '1',
          HTTP_PROXY: `http://127.0.0.1:${httpPort}`,
          HTTPS_PROXY: `http://127.0.0.1:${httpPort}`,
          http_proxy: `http://127.0.0.1:${httpPort}`,
          https_proxy: `http://127.0.0.1:${httpPort}`,
          ALL_PROXY: `http://127.0.0.1:${httpPort}`,
          all_proxy: `http://127.0.0.1:${httpPort}`,
          NO_PROXY: '127.0.0.1,localhost,::1',
          no_proxy: '127.0.0.1,localhost,::1',
        },
      },
    )
    const serverProcess = backend
    const setupToken = await new Promise<string>((resolve, reject) => {
      let token = ''
      let text = ''
      const deadline = setTimeout(() => reject(new Error('Server did not become ready')), 45_000)
      const onData = (chunk: Buffer) => {
        text += chunk.toString()
        token = /Stravia setup token:\s*(\S+)/.exec(text)?.[1] ?? token
        const address = /Stravia Server listening[^\n]*127\.0\.0\.1:(\d+)/.exec(stripVTControlCharacters(text))
        if (address && token) {
          backendPort = Number(address[1])
          clearTimeout(deadline)
          serverProcess.stdout?.off('data', onData)
          serverProcess.stderr?.off('data', onData)
          resolve(token)
        }
      }
      serverProcess.stdout?.on('data', onData)
      serverProcess.stderr?.on('data', onData)
      serverProcess.once('error', (error) => {
        clearTimeout(deadline)
        reject(error)
      })
      serverProcess.once('exit', (code) => {
        clearTimeout(deadline)
        reject(new Error(`Server exited before readiness (${code}); build the production server first`))
      })
    })
    const context = await isolatedContext(browser)
    const page = await context.newPage()
    await page.goto(`${secureOrigin}/setup`)
    await expect(page.locator('#setup-token')).toBeVisible()
    expect(await page.evaluate(() => window.isSecureContext)).toBe(true)
    expect(await api(page, '/api/v1/setup/claim', 'POST', { token: setupToken })).toBe(204)
    expect(
      await api(page, '/api/v1/setup/complete', 'POST', {
        database: { backend: 'sqlite', path: join(directory, 'gateway.db') },
        username: 'admin',
        password,
        client_base_url: plainOrigin,
      }),
    ).toBe(200)
    await page.goto(`${plainOrigin}/login`)
    expect(await page.evaluate(() => window.isSecureContext)).toBe(false)
    await page.locator('#admin-username').fill('admin')
    await page.locator('input[autocomplete="current-password"]').fill(password)
    await page.locator('button[type="submit"]').click()
    await expect(page).toHaveURL(`${plainOrigin}/`)
    expect(await api(page, '/api/v1/status')).toBe(200)
    expect(await api(page, '/api/v1/settings/log_retention_days', 'PUT', { value: '9' })).toBe(200)
    await page.reload()
    await expect(page).toHaveURL(`${plainOrigin}/`)
    expect(await api(page, '/api/v1/status')).toBe(200)
    expect(await api(page, '/api/v1/auth/refresh', 'POST', {})).toBe(200)
    expect(await api(page, '/api/v1/auth/logout', 'POST')).toBe(204)
    expect(await api(page, '/api/v1/status')).toBe(401)
    await context.close()

    const secureContext = await isolatedContext(browser)
    const securePage = await secureContext.newPage()
    await securePage.goto(`${secureOrigin}/login`)
    await securePage.locator('#admin-username').fill('admin')
    await securePage.locator('input[autocomplete="current-password"]').fill(password)
    await securePage.locator('button[type="submit"]').click()
    await expect(securePage).toHaveURL(`${secureOrigin}/`)
    expect(await api(securePage, '/api/v1/status')).toBe(200)
    await securePage.reload()
    expect(await api(securePage, '/api/v1/auth/refresh', 'POST', {})).toBe(200)
    const accessCookie = (await secureContext.cookies()).find((cookie) => cookie.name === 'stravia_access')
    expect(accessCookie?.secure).toBe(true)
    expect(accessCookie?.httpOnly).toBe(true)
    // 同主机、不同协议：HTTPS Cookie 必须不能随 HTTP 管理请求发送。
    const plainPage = await secureContext.newPage()
    await plainPage.goto(`${plainOrigin}/login`)
    proxyCookies.length = 0
    expect(await api(plainPage, '/api/v1/status')).toBe(401)
    expect(await api(plainPage, '/api/v1/auth/refresh', 'POST', {})).toBe(401)
    expect(proxyCookies).toEqual([''])
    expect(await api(securePage, '/api/v1/auth/refresh', 'POST', {})).toBe(200)
    expect(await api(securePage, '/api/v1/status')).toBe(200)
    expect(await api(securePage, '/api/v1/auth/logout', 'POST')).toBe(204)
    expect(await api(securePage, '/api/v1/status')).toBe(401)
    const denied = await securePage.goto(`https://denied.test:${httpsPort}/media-understanding`)
    expect(denied?.status()).toBe(403)
    await expect(securePage.locator('#admin-username')).toHaveCount(0)
    await secureContext.close()
  } finally {
    await browser?.close()
    if (backend && backend.exitCode === null) {
      backend.kill()
      await once(backend, 'exit')
    }
    if (http.listening) await close(http)
    if (https?.listening) await close(https)
    await rm(directory, { recursive: true, force: true })
  }
})
