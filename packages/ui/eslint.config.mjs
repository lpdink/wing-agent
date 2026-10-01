import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';

/**
 * Lint gates for the UI package.
 *
 * The package's promise is "a renderer that ships into a browser document, plus a
 * DOM-free wire contract a Node host can consume": no `vscode`, no node builtin, and
 * the only npm modules it may pull are the ones it declares. Three mechanisms keep
 * that honest, exactly like the layering gates in `extensions/vscode`
 * (docs/dev/vscode-extension.md §2.3):
 *
 * 1. the split tsconfigs — `tsconfig.dom.json` (`src/**`, no node types) makes a bare
 *    `process` a compile error, and `tsconfig.node-probe.json` (`src/protocol`,
 *    `src/testing`, no DOM lib) makes `document` one for the host-facing entries. The
 *    *main* project deliberately carries both halves and the vitest config, which is
 *    why the two probes exist;
 * 2. these ESLint zones — `no-restricted-imports` for the module boundaries and
 *    `no-restricted-globals` for both directions of the environment split (see the
 *    NODE_GLOBALS / DOM_GLOBALS lists below); the common violations are red while you
 *    type;
 * 3. `tests/layers.test.ts` — parses the real import graph of every source file
 *    (static imports, `export … from`, dynamic `import()`, `require()`), pins the
 *    dependency allowlist, and keeps every color on a theme variable. It is the
 *    authoritative gate and runs in `pnpm test`.
 */

const VSCODE = 'vscode';
const PORTABLE_BOUNDARY = 'The UI package must stay portable: no `vscode`, no editor APIs, no host glue.';
const NODE_BUILTIN_BOUNDARY =
  'Node builtins are unavailable in a browser document — this code ships into the webview/web bundle.';
const SESSION_PACKAGE = '@wing-agent/session';
const OUTSIDE_BOUNDARY =
  'The only workspace package this one may import is the session model — and only through its barrel; everything else is a relative import inside src/.';

/**
 * Globals the package must never touch.
 *
 * The lint-time half of the promise: `tsconfig.dom.json` already makes these a
 * compile error in `src/`, but a gate you only meet at `typecheck` time is easy to
 * forget, and this one is red while you type.
 */
const NODE_GLOBALS = ['process', 'require', 'Buffer', 'module', '__dirname', '__filename', 'global'];

/**
 * The other direction: the entries a Node program consumes (`src/protocol`,
 * `src/testing`) are compiled by `tsconfig.node-probe.json` without the DOM lib, and
 * this list catches the same mistake while typing.
 */
const DOM_GLOBALS = ['document', 'window', 'navigator', 'localStorage', 'sessionStorage'];

/** `no-restricted-imports` rule (`group` values are arrays — what ESLint 10 validates). */
const restricted = (groups) => [
  'error',
  {
    paths: [{ name: VSCODE, message: PORTABLE_BOUNDARY }],
    patterns: [
      ...groups.map(([group, message]) => ({ group, message })),
      {
        group: [`${SESSION_PACKAGE}/*`],
        message: `Import "${SESSION_PACKAGE}" through its barrel only: a path into the package is not a contract.`,
      },
    ],
  },
];

const restrictedGlobals = (names, message) => [
  'error',
  ...names.map((name) => ({ name, message: `${message} (${name})` })),
];

export default tseslint.config(
  { ignores: ['node_modules/**', 'coverage/**', 'dist/**', 'eslint.config.mjs'] },

  js.configs.recommended,

  {
    files: ['**/*.ts', '**/*.tsx', '**/*.mts'],
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: {
        // Type-aware linting (no-floating-promises, no-misused-promises, …).
        // Every linted file belongs to tsconfig.json — see that file.
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
    files: ['**/*.ts', '**/*.tsx', '**/*.mts'],
    extends: [tseslint.configs.recommendedTypeChecked],
  },

  // ── Environment zones ──────────────────────────────────────────────────
  {
    files: ['src/**/*.ts', 'src/**/*.tsx'],
    rules: {
      'no-restricted-imports': restricted([
        [['node:*'], NODE_BUILTIN_BOUNDARY],
        // Everything that is not the one workspace dependency this package declares,
        // and every deep path into it: the barrel is the contract.
        [['@wing-agent/*', `!${SESSION_PACKAGE}`], OUTSIDE_BOUNDARY],
      ]),
      'no-restricted-globals': restrictedGlobals(
        NODE_GLOBALS,
        'Node globals are unavailable here: this renderer ships into a browser document',
      ),
    },
  },
  {
    // The two entries a Node host consumes (`tsconfig.node-probe.json`).
    files: ['src/protocol/**/*.ts', 'src/testing/**/*.ts'],
    rules: {
      'no-restricted-globals': restrictedGlobals(
        DOM_GLOBALS,
        'No DOM globals here: a Node host consumes this entry',
      ),
    },
  },

  // ── ANSI parsing ───────────────────────────────────────────────────────
  {
    // `no-control-regex` is exactly what this module is about: it matches ESC,
    // BEL and the other control bytes that a terminal's output carries and that
    // must never reach the DOM as literal characters. The one file it applies to
    // is the parser itself; everywhere else the rule stays on.
    files: ['src/tool/ansi.ts'],
    rules: { 'no-control-regex': 'off' },
  },

  // ── React ──────────────────────────────────────────────────────────────
  {
    files: ['src/**/*.tsx', 'tools/**/*.tsx'],
    plugins: { 'react-hooks': reactHooks },
    rules: {
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'error',
    },
  },

  // ── Environment globals ────────────────────────────────────────────────
  {
    files: ['src/**/*.ts', 'src/**/*.tsx', 'tests/**/*.ts', 'tests/**/*.tsx'],
    languageOptions: { globals: globals.browser },
  },
  {
    files: [
      'tests/**/*.ts',
      'tests/**/*.tsx',
      'vitest.config.mts',
      'vite.port-preview.config.mts',
      'eslint.config.mjs',
    ],
    languageOptions: { globals: globals.node },
  },
  // The dev-only port preview (`tools/port-preview`) is browser code: it mounts
  // React into a document and is built by the port-preview Vite config. Its
  // screenshot driver is the one Node program in there — it spawns Chrome and
  // speaks the DevTools protocol over the built-in WebSocket.
  {
    files: ['tools/**/*.ts', 'tools/**/*.tsx'],
    languageOptions: { globals: globals.browser },
  },
  {
    files: ['tools/**/*.mts'],
    languageOptions: { globals: globals.node },
  },

  // ── Tests: relaxed ─────────────────────────────────────────────────────
  {
    files: ['tests/**/*.ts', 'tests/**/*.tsx'],
    rules: {
      'no-console': 'off',
      // Tests intentionally build partial / violating inputs (mocks, fake documents).
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
