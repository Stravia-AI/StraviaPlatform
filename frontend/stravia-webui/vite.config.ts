import adapter from '@sveltejs/adapter-static'
import { dirname, sep } from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'
import tailwindcss from '@tailwindcss/vite'
import { sveltekit } from '@sveltejs/kit/vite'
import { defineConfig, loadEnv, searchForWorkspaceRoot, type Plugin } from 'vite'

const repositoryRoot = fileURLToPath(new URL('../..', import.meta.url))
const webuiRootFile = fileURLToPath(new URL('./src/lib/extensions/index.ts', import.meta.url))

// 桌面测试包含 native smoke bridge，不能覆盖服务端验收使用的生产资源。
const outputDirectory = process.env.STRAVIA_WEBUI_DIST ?? 'dist'

const customExtensionsPath = process.env.STRAVIA_WEBUI_EXTENSIONS
const extensionsEntry = customExtensionsPath
  ? fileURLToPath(new URL(customExtensionsPath, new URL('./', import.meta.url)))
  : fileURLToPath(new URL('./src/lib/extensions/ce-entry.ts', import.meta.url))
const extensionsDir = dirname(extensionsEntry)

/**
 * Allows build-time extension modules outside `frontend/stravia-webui` to resolve
 * shared workspace dependencies (`svelte`, `@lucide/svelte`, `@tanstack/svelte-query`, etc.)
 * from CE's `node_modules` without duplicating packages in the commercial repository.
 */
function extensionPackageFallbackPlugin(extDir: string, rootFile: string): Plugin {
  const prefix = extDir.endsWith(sep) ? extDir : `${extDir}${sep}`
  return {
    name: 'stravia-extension-package-fallback',
    async resolveId(source, importer, options) {
      if (
        !importer ||
        !importer.startsWith(prefix) ||
        source.startsWith('.') ||
        source.startsWith('/') ||
        source.startsWith('\0') ||
        source.startsWith('#') ||
        source.startsWith('$') ||
        source.startsWith('virtual:')
      ) {
        return null
      }
      return this.resolve(source, rootFile, { ...options, skipSelf: true })
    },
  }
}

export default defineConfig(({ mode }) => {
  const { STRAVIA_PORT = '23471' } = loadEnv(mode, repositoryRoot, 'STRAVIA_')
  const backendPort = Number(STRAVIA_PORT)
  if (!Number.isInteger(backendPort) || backendPort < 1 || backendPort > 65535) {
    throw new Error(`STRAVIA_PORT must be an integer between 1 and 65535, received "${STRAVIA_PORT}"`)
  }

  return {
    envDir: repositoryRoot,
    resolve: {
      alias: {
        '#lib': fileURLToPath(new URL('./src/lib', import.meta.url)),
        '#app-css': fileURLToPath(new URL('./src/app.css', import.meta.url)),
        '#webui-extensions': extensionsEntry,
      },
    },
    plugins: [
      ...(customExtensionsPath ? [extensionPackageFallbackPlugin(extensionsDir, webuiRootFile)] : []),
      tailwindcss(),
      sveltekit({
        paths: { relative: false },
        adapter: adapter({ assets: outputDirectory, fallback: 'index.html', pages: outputDirectory }),
        // 产品更新沿用后端更新服务，不额外轮询静态部署版本。
        version: { pollInterval: 0 },
      }),
    ],
    build: {
      // Lazy-loaded mermaid bundles upstream elkjs (~1.46 MB) and parser (~629 kB); app chunks remain < 500 kB.
      chunkSizeWarningLimit: 1600,
      rolldownOptions: { checks: { pluginTimings: false } },
    },
    optimizeDeps: {
      include: ['style-to-object'],
      // Styled Svelte libraries must stay eligible for prebundling or cold starts can expose component source as virtual CSS.
      exclude: ['@lucide/svelte', '@tanstack/svelte-query', 'mode-watcher'],
    },
    server: {
      // Bun 在 localhost 的 IPv6 地址被占用后可能改绑 IPv4，导致两个实例仍使用同一端口。
      host: '127.0.0.1',
      port: 5173,
      ...(customExtensionsPath
        ? { fs: { allow: [searchForWorkspaceRoot(process.cwd()), repositoryRoot, extensionsDir] } }
        : {}),
      proxy: {
        '/api/v1': {
          target: `http://127.0.0.1:${backendPort}`,
          changeOrigin: true,
          configure(proxy) {
            proxy.on('proxyReq', (proxyReq, req) => {
              // Only this local HTTP proxy may declare the browser-facing entry.
              proxyReq.removeHeader('forwarded')
              proxyReq.setHeader('x-forwarded-proto', 'http')
              proxyReq.removeHeader('x-forwarded-host')
              if (req.headers.host) proxyReq.setHeader('x-forwarded-host', req.headers.host)
            })
          },
        },
        '/v1': { target: `http://127.0.0.1:${backendPort}`, changeOrigin: true },
        '/v1beta': { target: `http://127.0.0.1:${backendPort}`, changeOrigin: true },
      },
    },
  }
})
