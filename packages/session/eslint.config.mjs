import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import globals from 'globals';
import tseslint from 'typescript-eslint';

/**
 * Lint gates for the session package.
 *
 * The package's promise is "environment-agnostic reduction": plain ES2023 over the
 * events `@wing-agent/client` decoded, no `vscode`, no node builtin, no npm
 * dependency beyond that one package. Three mechanisms keep that honest, exactly
 * like the layering gates in `extensions/vscode` (docs/dev/vscode-extension.md §2.3):
 *
 * 1. the split tsconfigs — `tsconfig.json` has no DOM lib, `tsconfig.dom.json` has
 *    no node types, so writing `document` or a bare `process` fails one of them;
 * 2. these ESLint zones — the common violations are red while you type;
 * 3. `tests/layers.test.ts` — parses the real import graph of every source file
 *    (static imports, `export … from`, dynamic `import()`, `require()`). It is the
 *    authoritative gate and runs in `pnpm test`.
 */

const VSCODE = 'vscode';
const PORTABLE_BOUNDARY =
  'The session package must stay environment-agnostic: no vscode, no host, no renderer.';
const NODE_BUILTIN_BOUNDARY =
  'Node builtins are unavailable in the browser build: this package runs in web shells too.';
const OUTSIDE_BOUNDARY =
  'Only the gateway capability layer may be imported from outside this package (the wire events it decodes); everything else is a relative import inside src/.';

/**
 * DOM globals the package must never touch.
 *
 * The tsconfig pair alone cannot enforce this direction any more: `types: ["node"]`
 * is required (the *tests* are node code, and they live in the same project), and
 * `@types/node` pulls `lib.dom.d.ts` in through its own `/// <reference lib="dom" />`
 * — so `document` type-checks in the main project. The browser direction still has a
 * real tsconfig gate (`tsconfig.dom.json` has `types: []`: a bare `process` fails
 * there), and the node direction is gated here.
 */
const DOM_GLOBALS = ['document', 'window', 'navigator', 'localStorage', 'sessionStorage'];

/** `no-restricted-imports` rule (`group` values are arrays — what ESLint 10 validates). */
const restricted = (groups) => [
  'error',
  {
    paths: [{ name: VSCODE, message: PORTABLE_BOUNDARY }],
    patterns: groups.map(([group, message]) => ({ group, message })),
  },
];

export default tseslint.config(
  { ignores: ['node_modules/**', 'coverage/**', 'eslint.config.mjs'] },

  js.configs.recommended,

  {
    files: ['**/*.ts', '**/*.mts'],
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
    files: ['**/*.ts', '**/*.mts'],
    extends: [tseslint.configs.recommendedTypeChecked],
  },

  {
    files: ['src/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted([
        [['node:*'], NODE_BUILTIN_BOUNDARY],
        // Everything that is not the workspace dependency this package declares,
        // and every deep path into it: the barrel is the contract.
        [['@wing-agent/*', '!@wing-agent/client'], OUTSIDE_BOUNDARY],
        [['@wing-agent/client/*'], OUTSIDE_BOUNDARY],
      ]),
      // The other direction of the portability promise — see DOM_GLOBALS.
      'no-restricted-globals': [
        'error',
        ...DOM_GLOBALS.map((name) => ({
          name,
          message: `No DOM globals here: the package also runs inside a Node host (${name}).`,
        })),
      ],
    },
  },

  // The package ships into a Node host bundle; its own build config is node code.
  {
    files: ['src/**/*.ts', 'tests/**/*.ts', 'eslint.config.mjs', '*.config.mts'],
    languageOptions: { globals: globals.node },
  },

  // Tests intentionally build partial / violating inputs (fake frames, bad payloads).
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
