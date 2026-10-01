import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import globals from 'globals';
import tseslint from 'typescript-eslint';

/**
 * Lint gates for the web app.
 *
 * Mechanism #2 of the three that keep the layering honest (`tsconfig.json` /
 * `tsconfig.tools.json` are #1, `tests/layers.test.ts` is the authoritative #3):
 *
 * - `src/**` is the browser bundle: no node builtin, no `node:*`, no import from
 *   `tests/` or `tools/`, and packages only through their barrel (a deep path
 *   would make the package's file layout a contract of this app).
 * - the framework-free core (`src/lib`, `src/settings`, `src/sessions`,
 *   `src/connection`) must not import React: the runtime is consumed by
 *   `useSyncExternalStore`, it does not know the renderer exists.
 * - `src/settings` and `src/lib` are the bottom of the stack: no session
 *   semantics there (settings hold an address, not a transcript).
 *
 * Every zone declares its *complete* banned set on purpose — ESLint replaces (not
 * merges) a rule per matching block, so a zone that forgets a rule silently allows
 * it.
 */

const CLIENT = '@wing-agent/client';
const SESSION = '@wing-agent/session';
const UI = '@wing-agent/ui';
/**
 * The one subpath of `@wing-agent/ui` that is allowed alongside the barrels:
 * `./protocol` is that package's DOM-free wire contract (the extension host imports
 * it from Node too, and the guard in `packages/ui/tests/layers.test.ts` enumerates it
 * — a bare `@wing-agent/ui/**` ban would reject it).
 */
const UI_PROTOCOL = '@wing-agent/ui/protocol';
/**
 * …and the second: `./styles/*` is the package's declared theme seam for browser
 * shells (its `exports` map). The web app imports the five token sheets once in
 * `src/main.tsx`; the VS Code webview must *not* (it keeps the editor's own theme,
 * which is why the package's barrel deliberately does not pull them in).
 *
 * The un-ban is two patterns, not one, and that is gitignore's rule, not a choice:
 * `@wing-agent/ui/**` excludes the `styles/` *directory* as well, and a file cannot
 * be re-included while its parent directory is excluded — so the directory is
 * re-included first (`…/styles/`), then the sheets.
 */
const UI_STYLES = ['!@wing-agent/ui/styles/', '!@wing-agent/ui/styles/*.css'];
const BARREL_ONLY =
  'Import workspace packages through their barrel only: a path into the package is not a contract (the exceptions are `@wing-agent/ui/protocol`, the DOM-free wire contract, and `@wing-agent/ui/styles/*.css`, the theme sheets a browser shell loads once).';
const NODE_BUILTIN =
  'This is the browser bundle: node builtins are unavailable (the screenshot tooling lives in tools/, which has its own tsconfig).';
const REACT_FREE =
  'The framework-free core must not import React: the runtime is consumed through useSyncExternalStore, it does not know the renderer exists.';
const NO_SESSION =
  'Settings describe an address, not a transcript: session semantics belong to src/connection / src/sessions.';
const NO_TESTS = 'Product code must not import tests/ or tools/: both are outside the bundle.';

const restricted = (paths = [], patterns = []) => ['error', { paths, patterns }];

/** One entry of the import zones: a glob group + why it is banned. */
const banned = (globs, message) => ({ group: globs, message });

export default tseslint.config(
  {
    ignores: [
      'dist/**',
      'out/**',
      'node_modules/**',
      'coverage/**',
      'eslint.config.mjs',
      'esbuild.bench.mjs',
    ],
  },

  js.configs.recommended,

  {
    files: ['**/*.ts', '**/*.tsx', '**/*.mjs'],
    languageOptions: {
      parser: tseslint.parser,
      parserOptions: {
        // Type-aware linting (no-floating-promises, no-misused-promises, …).
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
        sourceType: 'module',
        ecmaVersion: 2023,
        ecmaFeatures: { jsx: true },
      },
    },
    plugins: { '@typescript-eslint': tseslint.plugin },
    rules: {
      ...tseslint.configs.recommended.rules,
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/no-unused-vars': ['error', { argsIgnorePattern: '^_', varsIgnorePattern: '^_' }],
      '@typescript-eslint/consistent-type-imports': ['error', { prefer: 'type-imports' }],
      'no-console': ['error', { allow: ['debug', 'warn', 'error', 'info'] }],
      eqeqeq: ['error', 'smart'],
      'prefer-const': 'error',
      'no-throw-literal': 'error',
      'no-implicit-coercion': 'error',
    },
  },

  {
    files: ['**/*.ts', '**/*.tsx'],
    extends: [tseslint.configs.recommendedTypeChecked],
  },

  // ── Layer zones ────────────────────────────────────────────────────────
  {
    // The browser bundle. `node:*` is a compile error too (tsconfig has no node
    // types); this is the version that is red while you type.
    files: ['src/**/*.ts', 'src/**/*.tsx'],
    rules: {
      'no-restricted-imports': restricted(
        [],
        [
          banned(['node:*'], NODE_BUILTIN),
          banned([`${CLIENT}/**`, `${SESSION}/**`, `${UI}/**`, `!${UI_PROTOCOL}`, ...UI_STYLES], BARREL_ONLY),
          banned(['../tests/**', '../tools/**', '**/tests/**', '**/tools/**'], NO_TESTS),
        ],
      ),
    },
  },
  {
    // The framework-free core: React is the renderer's business, not the
    // runtime's (the snapshot/observer shape is what keeps that true). `images`
    // (the gateway image adapter: policy / URL / resolver) and `bridge` (the
    // renderer's intent routing) are plain TypeScript with injected platforms —
    // they only *talk about* the renderer, they never render.
    files: [
      'src/lib/**/*.ts',
      'src/settings/**/*.ts',
      'src/sessions/**/*.ts',
      'src/connection/**/*.ts',
      'src/images/**/*.ts',
      'src/bridge/**/*.ts',
    ],
    rules: {
      'no-restricted-imports': restricted(
        [
          { name: 'react', message: REACT_FREE },
          { name: 'react-dom', message: REACT_FREE },
        ],
        [
          banned(['node:*'], NODE_BUILTIN),
          banned(['react', 'react-dom', 'react/**', 'react-dom/**'], REACT_FREE),
          banned([`${CLIENT}/**`, `${SESSION}/**`, `${UI}/**`, `!${UI_PROTOCOL}`, ...UI_STYLES], BARREL_ONLY),
          banned(['../tests/**', '../tools/**', '**/tests/**', '**/tools/**'], NO_TESTS),
        ],
      ),
    },
  },
  {
    // The bottom of the stack: `settings` holds an address, `lib` is generic.
    files: ['src/lib/**/*.ts', 'src/settings/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted(
        [
          { name: 'react', message: REACT_FREE },
          { name: 'react-dom', message: REACT_FREE },
          { name: SESSION, message: NO_SESSION },
        ],
        [
          banned(['node:*'], NODE_BUILTIN),
          banned(['react', 'react-dom', 'react/**', 'react-dom/**'], REACT_FREE),
          banned([`${CLIENT}/**`, `${SESSION}/**`, `${UI}/**`, `!${UI_PROTOCOL}`, ...UI_STYLES], BARREL_ONLY),
          banned(['../tests/**', '../tools/**', '**/tests/**', '**/tools/**'], NO_TESTS),
        ],
      ),
    },
  },
  {
    // Test and tool code: node globals are fine, but the browser bundle must stay
    // out of reach in the other direction (tests import src, never the reverse —
    // enforced by the zone above).
    files: ['tests/**/*.ts', 'tests/**/*.tsx', 'tools/**/*.ts'],
    languageOptions: { globals: { ...globals.node } },
  },

  prettier,
);
