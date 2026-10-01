import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import globals from 'globals';
import tseslint from 'typescript-eslint';

/**
 * Lint gates for the Electron shell.
 *
 * The shell's promise is "electron is a boundary": every module except
 * `src/main.ts` and `src/preload.ts` is plain Node logic that `vitest` runs
 * without an Electron runtime. Three mechanisms keep that honest:
 *
 * 1. this zone — a *value* import of `electron` outside the two entry files is
 *    red while you type (`allowTypeImports` keeps `import type` legal, which is
 *    how `src/menu.ts` speaks Electron's menu vocabulary);
 * 2. the single node tsconfig (`lib: ES2023`, `types: node`, no DOM);
 * 3. `tests/layers.test.ts` — parses the real import graph of every source file
 *    (static imports, `export … from`, dynamic `import()`, `require()`, import
 *    types). It is the authoritative gate and runs in `pnpm test`.
 */

const ELECTRON = 'electron';
const ELECTRON_BOUNDARY_MESSAGE =
  'Only src/main.ts and src/preload.ts may import electron for value: every other module is plain Node logic covered by unit tests. Use `import type` for Electron types.';

/**
 * `no-restricted-imports` rule. The options object is always present on purpose:
 * a bare `['error']` keeps the paths of the previously matching config block,
 * which is the opposite of "this zone replaces that one".
 */
const restricted = (paths) => ['error', { paths }];

const ELECTRON_PATH = { name: ELECTRON, message: ELECTRON_BOUNDARY_MESSAGE };

export default tseslint.config(
  { ignores: ['node_modules/**', 'dist/**', 'release/**', 'coverage/**', 'eslint.config.mjs'] },

  js.configs.recommended,

  {
    files: ['**/*.ts', '**/*.mts', '**/*.mjs'],
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: {
        // Type-aware linting (no-floating-promises, no-misused-promises, …).
        // Every linted file belongs to tsconfig.json or tsconfig.tools.json.
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
        sourceType: 'module',
        ecmaVersion: 2023,
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...tseslint.configs.recommended.rules,
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
      '@typescript-eslint/consistent-type-imports': ['error', { prefer: 'type-imports' }],
      // `debug`/`warn`/`error` only — a stray `console.log` is a bug report waiting to happen.
      'no-console': ['error', { allow: ['debug', 'warn', 'error'] }],
      eqeqeq: ['error', 'smart'],
      'prefer-const': 'error',
      'no-throw-literal': 'error',
      'no-implicit-coercion': 'error',
    },
  },

  {
    files: ['**/*.ts', '**/*.mts', '**/*.mjs'],
    extends: [tseslint.configs.recommendedTypeChecked],
  },

  // Electron is a boundary: value imports only in the two entry files.
  {
    files: ['src/**/*.ts'],
    rules: {
      '@typescript-eslint/no-restricted-imports': restricted([{ ...ELECTRON_PATH, allowTypeImports: true }]),
    },
  },
  {
    files: ['src/main.ts', 'src/preload.ts'],
    rules: {
      // Complete per-zone rule set: this block *replaces* the one above.
      '@typescript-eslint/no-restricted-imports': restricted([]),
    },
  },

  // Environment globals.
  {
    files: ['src/**/*.ts', 'tests/**/*.ts', 'esbuild.mjs', 'scripts/**/*.mjs', '*.config.mts'],
    languageOptions: { globals: globals.node },
  },
  {
    // Build/dev scripts report progress on stdout; that is their interface.
    files: ['esbuild.mjs', 'scripts/**/*.mjs', '*.config.mts'],
    rules: { 'no-console': 'off' },
  },
  {
    // Plain-JS tooling (`checkJs: false` in tsconfig.tools.json): type-aware rules
    // have no type information to work with, so the unsafe-* family is noise here
    // (the same relaxation the other packages apply to their test files).
    files: ['**/*.mjs'],
    rules: {
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
    },
  },

  // Tests intentionally build partial / violating inputs (bad configs, fake URLs).
  {
    files: ['tests/**/*.ts'],
    rules: {
      'no-console': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-return': 'off',
    },
  },

  // Last: disable formatting rules that Prettier owns.
  prettier,
);
