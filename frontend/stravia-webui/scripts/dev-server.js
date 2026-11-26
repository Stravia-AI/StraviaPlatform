import { spawn, spawnSync } from 'node:child_process'
import { once } from 'node:events'
import { resolve } from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { createServer, loadEnv } from 'vite'
import { DEFAULT_BACKEND_PORT, parseBackendPort, resolveAvailableBackendPort } from './dev-server-port.ts'

const webuiRoot = fileURLToPath(new URL('../', import.meta.url))
const repositoryRoot = resolve(webuiRoot, '../..')
const preparation = spawnSync('task', ['prepare:crates'], { cwd: repositoryRoot, stdio: 'inherit' })
if (preparation.error) throw preparation.error
if (preparation.status !== 0) throw new Error('Failed to prepare patched Cargo crates')

const env = loadEnv('development', repositoryRoot, 'STRAVIA_')
const backendHost = env.STRAVIA_HOST || '127.0.0.1'
const requestedPort = parseBackendPort(env.STRAVIA_PORT ?? String(DEFAULT_BACKEND_PORT))
const backendPort = await resolveAvailableBackendPort(backendHost, requestedPort)
if (backendPort !== requestedPort) {
  console.warn(`Backend port ${requestedPort} on ${backendHost} is already in use; using ${backendPort} instead.`)
}
process.env.STRAVIA_PORT = String(backendPort)

const webui = await createServer({ root: webuiRoot, server: { host: true } })

try {
  await webui.listen()
  const localUrl = webui.resolvedUrls?.local[0]
  if (!localUrl) {
    throw new Error('The development WebUI must have a local listening address')
  }
  // Keep an explicit IPv4 loopback entry even when Vite advertises localhost.
  // Wildcard binding also avoids Bun's localhost address-family port fallback.
  const loopbackUrl = new URL(localUrl)
  loopbackUrl.hostname = '127.0.0.1'
  webui.resolvedUrls.local = [...new Set([loopbackUrl.href, ...webui.resolvedUrls.local])]
  const origins = new Set(
    [...webui.resolvedUrls.local, ...webui.resolvedUrls.network].map((url) => new URL(url).origin),
  )
  webui.printUrls()

  // Vite may select another port when other workspaces are already running.
  const backend = spawn(
    'cargo',
    [
      'run',
      '-p',
      'stravia-server',
      '--',
      ...[...origins].flatMap((origin) => ['--admin-origin', origin]),
      '--trusted-proxy',
      '127.0.0.1',
    ],
    { cwd: repositoryRoot, env: { ...process.env, STRAVIA_PORT: String(backendPort) }, stdio: 'inherit' },
  )
  const [exitCode] = await once(backend, 'exit')
  process.exitCode = exitCode ?? 1
} finally {
  await webui.close()
}
