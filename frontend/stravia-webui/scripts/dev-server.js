import { spawn } from 'node:child_process'
import { once } from 'node:events'
import { resolve } from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

const webuiRoot = fileURLToPath(new URL('../', import.meta.url))
const repositoryRoot = resolve(webuiRoot, '../..')
const webui = await createServer({ root: webuiRoot })

try {
  await webui.listen()
  const localUrl = webui.resolvedUrls?.local[0]
  if (!localUrl) {
    throw new Error('The development WebUI must have a local listening address')
  }
  webui.printUrls()

  // Vite may select another port when other workspaces are already running.
  const backend = spawn('cargo', ['run', '-p', 'stravia-server', '--', '--admin-origin', new URL(localUrl).origin, '--trusted-proxy', '127.0.0.1'], {
    cwd: repositoryRoot,
    stdio: 'inherit',
  })
  const [exitCode] = await once(backend, 'exit')
  process.exitCode = exitCode ?? 1
} finally {
  await webui.close()
}
