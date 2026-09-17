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
  tseslint.configs.recommendedTypeChecked,
  {
    // Plain JS files are outside every tsconfig include; type-aware rules cannot run on them.
    files: ['**/*.js', '**/*.cjs', '**/*.mjs'],
    ignores: ['**/*.svelte.js'],
    extends: [tseslint.configs.disableTypeChecked],
  },
  {
    files: ['**/*.ts', '**/*.mts', '**/*.cts'],
    languageOptions: {
      parserOptions: {
        // e2e/, scripts/, and the root runner configs sit outside the SvelteKit tsconfig.
        project: ['./tsconfig.json', './tsconfig.eslint.json'],
        tsconfigRootDir: import.meta.dirname,
      },
    },
  },
  {
    files: ['**/*.ts', '**/*.mts', '**/*.cts', '**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
    rules: {
      '@typescript-eslint/restrict-template-expressions': [
        'error',
        { allowNumber: true, allowBoolean: true, allowNullish: true },
      ],
    },
  },
  svelte.configs.recommended,
  ...pluginQuery.configs['flat/recommended'],
  {
    files: ['**/*.svelte', '**/*.svelte.ts', '**/*.svelte.js'],
    languageOptions: {
      globals: { ...globals.browser, ...globals.node },
      parserOptions: {
        project: ['./tsconfig.json'],
        tsconfigRootDir: import.meta.dirname,
        extraFileExtensions: ['.svelte'],
        parser: tseslint.parser,
        svelteConfig,
      },
    },
    rules: { '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }] },
  },
  {
    // E2E specs and build scripts handle untyped JSON payloads and WebdriverIO
    // globals; the no-unsafe-* family adds noise without catching real bugs there.
    files: ['e2e/**/*.ts', 'scripts/**/*.ts'],
    rules: {
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
    },
  },
  {
    // WebdriverIO assertion chains type as non-Thenable even though the runner awaits them.
    files: ['e2e/desktop.smoke.ts'],
    rules: { '@typescript-eslint/await-thenable': 'off' },
  },
  svelte.configs.prettier,
])
