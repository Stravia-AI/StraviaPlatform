import { fileURLToPath } from 'node:url'
import tailwindcss from '@tailwindcss/vite'
import { sveltekit } from '@sveltejs/kit/vite'
import { defineConfig, loadEnv } from 'vite'

const repositoryRoot = fileURLToPath(new URL('../..', import.meta.url))

export default defineConfig(({ mode }) => {
  const { STRAVIA_PORT = '23471' } = loadEnv(mode, repositoryRoot, 'STRAVIA_')
  const backendPort = Number(STRAVIA_PORT)
  if (!Number.isInteger(backendPort) || backendPort < 1 || backendPort > 65535) {
    throw new Error(`STRAVIA_PORT must be an integer between 1 and 65535, received "${STRAVIA_PORT}"`)
  }

  return {
    envDir: repositoryRoot,
    plugins: [tailwindcss(), sveltekit()],
    optimizeDeps: {
      include: ['style-to-object'],
      // Styled Svelte libraries must stay eligible for prebundling or cold starts can expose component source as virtual CSS.
      exclude: ['@lucide/svelte', '@tanstack/svelte-query', 'mode-watcher'],
    },
    server: { port: 5173, proxy: { '/api/v1': { target: `http://127.0.0.1:${backendPort}`, changeOrigin: true } } },
  }
})
