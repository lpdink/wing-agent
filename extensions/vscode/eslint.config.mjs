import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';

/**
 * Layering rules — mechanism #2 of three (see design.md D2).
 *
 * This catches the common violations while you type. The complete matrix (import
 * graph over every file, dynamic `import()`, `require`, node builtins,
 * `src/testing` reachability, hardcoded colors in CSS) lives in
 * `tests/layers/layers.test.ts` — that test is the authoritative gate. Each zone
 * declares its *complete* banned set: ESLint replaces (not merges) a rule per
 * matching block, so a zone that forgot a rule would silently allow it.
 */

const VSCODE = 'vscode';
const VSCODE_MESSAGE =
  'Only src/host may import "vscode". src/core (Electron seam), src/webview and src/shared must stay portable.';
const HOST_BOUNDARY =
  'src/host must not import the renderer (src/webview): the host talks to it over the bridge only.';
const WEBVIEW_BOUNDARY =
  'src/webview must not import host/core code: the renderer only consumes src/shared contract types.';
const SHARED_BOUNDARY = 'src/shared is dependency-free: no vscode / host / core / webview / node imports.';
const CORE_BOUNDARY = 'src/core is the portable gateway layer: no vscode, no host, no renderer.';
const TESTING_BOUNDARY = 'src/testing is test/preview-only; product code must not import it.';
const NODE_BUILTIN_BOUNDARY =
  'Node builtins are unavailable here: this layer also runs in the webview bundle.';

/** Host / core / renderer / testing layer directories as glob groups. */
const LAYER_GLOBS = {
  host: ['**/host/**'],
  core: ['**/core/**'],
  webview: ['**/webview/**'],
  testing: ['**/testing/**'],
};

/**
 * `no-restricted-imports` rule.
 *
 * `group` values are arrays (minimatch patterns over the import string), which is
 * the shape ESLint 10 validates; a bare string is rejected.
 */
const restricted = ({ banVscode = false, groups = [] }) => [
  'error',
  {
    paths: banVscode ? [{ name: VSCODE, message: VSCODE_MESSAGE }] : [],
    patterns: groups.map(([group, message]) => ({ group, message })),
  },
];

export default tseslint.config(
  {
    ignores: ['out/**', 'dist/**', 'node_modules/**', 'coverage/**', '*.vsix', 'eslint.config.mjs'],
  },

  js.configs.recommended,

  {
    files: ['**/*.ts', '**/*.tsx', '**/*.mjs'],
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: {
        // Type-aware linting (no-floating-promises, no-misused-promises, …).
        // Every linted file belongs to tsconfig.node/webview/tools — see those files.
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
      // `debug`/`warn`/`error` only: a `console.log` left behind is a bug report waiting to happen.
      'no-console': ['error', { allow: ['debug', 'warn', 'error'] }],
      eqeqeq: ['error', 'smart'],
      'prefer-const': 'error',
      'no-throw-literal': 'error',
      'no-implicit-coercion': 'error',
    },
  },

  {
    files: ['**/*.ts', '**/*.tsx', '**/*.mjs'],
    extends: [tseslint.configs.recommendedTypeChecked],
  },

  // ── Layer zones ────────────────────────────────────────────────────────
  {
    // The extension host is the only layer allowed to touch the editor API.
    files: ['src/host/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted({
        groups: [
          [LAYER_GLOBS.webview, HOST_BOUNDARY],
          [LAYER_GLOBS.testing, TESTING_BOUNDARY],
        ],
      }),
    },
  },
  {
    files: ['src/core/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted({
        banVscode: true,
        groups: [
          [LAYER_GLOBS.host, CORE_BOUNDARY],
          [LAYER_GLOBS.webview, CORE_BOUNDARY],
          [LAYER_GLOBS.testing, TESTING_BOUNDARY],
        ],
      }),
    },
  },
  {
    files: ['src/shared/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted({
        banVscode: true,
        groups: [
          [LAYER_GLOBS.host, SHARED_BOUNDARY],
          [LAYER_GLOBS.core, SHARED_BOUNDARY],
          [LAYER_GLOBS.webview, SHARED_BOUNDARY],
          [['node:*'], NODE_BUILTIN_BOUNDARY],
          [LAYER_GLOBS.testing, TESTING_BOUNDARY],
        ],
      }),
    },
  },
  {
    files: ['src/webview/**/*.ts', 'src/webview/**/*.tsx'],
    rules: {
      'no-restricted-imports': restricted({
        banVscode: true,
        groups: [
          [LAYER_GLOBS.host, WEBVIEW_BOUNDARY],
          [LAYER_GLOBS.core, WEBVIEW_BOUNDARY],
          [['node:*'], NODE_BUILTIN_BOUNDARY],
          [LAYER_GLOBS.testing, TESTING_BOUNDARY],
        ],
      }),
    },
  },
  {
    files: ['src/testing/**/*.ts', 'src/testing/**/*.tsx'],
    rules: {
      'no-restricted-imports': restricted({
        banVscode: true,
        groups: [
          [LAYER_GLOBS.host, 'src/testing may only import src/shared.'],
          [LAYER_GLOBS.core, 'src/testing may only import src/shared.'],
          [LAYER_GLOBS.webview, 'src/testing may only import src/shared.'],
        ],
      }),
    },
  },

  // ── React ──────────────────────────────────────────────────────────────
  {
    files: ['src/webview/**/*.ts', 'src/webview/**/*.tsx', 'preview/**/*.tsx', 'src/testing/**/*.tsx'],
    plugins: { 'react-hooks': reactHooks },
    rules: {
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'error',
    },
  },

  // ── Environment globals ────────────────────────────────────────────────
  {
    files: ['src/host/**/*.ts', 'src/core/**/*.ts', 'esbuild.mjs', '*.config.mts'],
    languageOptions: { globals: globals.node },
  },
  {
    // Build scripts report progress on stdout; that is their interface.
    files: ['esbuild.mjs', '*.config.mts'],
    rules: { 'no-console': 'off' },
  },
  {
    files: ['src/webview/**/*.ts', 'src/webview/**/*.tsx', 'preview/**/*.ts', 'preview/**/*.tsx'],
    languageOptions: { globals: globals.browser },
  },

  // ── Tests & preview: relaxed ───────────────────────────────────────────
  {
    files: ['tests/**/*.ts', 'tests/**/*.tsx', 'preview/**/*.ts', 'preview/**/*.tsx'],
    languageOptions: { globals: { ...globals.node, ...globals.browser } },
    rules: {
      'no-console': 'off',
      // Tests intentionally build partial / violating inputs (mocks, fake webviews).
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
