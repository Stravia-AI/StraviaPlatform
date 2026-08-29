import adapter from '@sveltejs/adapter-static'

/** @type {import("@sveltejs/kit").Config} */
const config = {
  kit: {
    paths: { relative: false },
    adapter: adapter({ assets: 'dist', fallback: 'index.html', pages: 'dist' }),
    alias: { '@': './src' },
  },
}

export default config
