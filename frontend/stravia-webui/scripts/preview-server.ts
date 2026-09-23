import { stat } from 'node:fs/promises'
import { resolve, sep } from 'node:path'

// Playwright 端到端预览服务。`vite preview` 走 Bun 的 node:http 兼容层，在
// Bun 1.4.x 上会因管道化响应回放崩溃（oven-sh/bun#40350）；Bun.serve 使用原生
// HTTP 实现，不经过该路径。端到端用例只依赖静态构建产物，接口请求全部由
// page.route 拦截，不存在的上游前缀直接返回 502。

const webuiRoot = resolve(import.meta.dir, '..')
const dist = resolve(webuiRoot, 'dist')
const indexPath = resolve(dist, 'index.html')

function flagValue(name: string): string | undefined {
  const index = process.argv.indexOf(name)
  return index >= 0 ? process.argv[index + 1] : undefined
}

const port = Number(flagValue('--port') ?? 4173)
const host = flagValue('--host') ?? '127.0.0.1'

if (
  !(await stat(indexPath).then(
    (info) => info.isFile(),
    () => false,
  ))
) {
  throw new Error(`preview-server requires ${indexPath}; run "bun run build" first`)
}

const server = Bun.serve({
  hostname: host,
  port,
  async fetch(request) {
    const pathname = decodeURIComponent(new URL(request.url).pathname)
    if (
      pathname === '/api' ||
      pathname.startsWith('/api/') ||
      pathname === '/v1' ||
      pathname.startsWith('/v1/') ||
      pathname === '/v1beta' ||
      pathname.startsWith('/v1beta/')
    ) {
      return new Response('backend unavailable in WebUI e2e preview', { status: 502 })
    }
    const filePath = resolve(dist, `.${pathname}`)
    if (filePath.startsWith(dist + sep)) {
      const info = await stat(filePath).catch(() => null)
      if (info?.isFile()) return new Response(Bun.file(filePath))
    }
    return new Response(Bun.file(indexPath))
  },
})

console.log(`preview-server listening on http://${server.hostname}:${server.port}`)
