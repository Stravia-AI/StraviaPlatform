import js from '@eslint/js'
import pluginQuery from '@tanstack/eslint-plugin-query'
import svelteConfig from './svelte.config.js'
import svelte from 'eslint-plugin-svelte'
import globals from 'globals'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  globalIgnores(['.svelte-kit', 'build', 'dist', 'src/lib/paraglide']),
  {
    files: ['**/*.{js,ts}'],
    extends: [js.configs.recommended],
    languageOptions: { ecmaVersion: 2020, globals: globals.browser },
  },
  tseslint.configs.recommended,
  svelte.configs.recommended,
  ...pluginQuery.configs['flat/recommended'],
  {
    files: ['**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
    languageOptions: {
      globals: { ...globals.browser, ...globals.node },
      parserOptions: { projectService: true, extraFileExtensions: ['.svelte'], parser: tseslint.parser, svelteConfig },
    },
    rules: { '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }] },
  },
  svelte.configs.prettier,
])
