import adapter from '@sveltejs/adapter-static'
import process from 'node:process'

// 桌面测试包含 native smoke bridge，不能覆盖服务端验收使用的生产资源。
const outputDirectory = process.env.STRAVIA_WEBUI_DIST ?? 'dist'

/** @type {import("@sveltejs/kit").Config} */
const config = {
  kit: {
    paths: { relative: false },
    adapter: adapter({ assets: outputDirectory, fallback: 'index.html', pages: outputDirectory }),
    alias: { '@': './src' },
  },
}

export default config
