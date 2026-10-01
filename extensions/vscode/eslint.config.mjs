import js from '@eslint/js';
import prettier from 'eslint-config-prettier';
import reactHooks from 'eslint-plugin-react-hooks';
import globals from 'globals';
import tseslint from 'typescript-eslint';

/**
 * Layering rules — mechanism #2 of three (see design.md D2).
 *
 * This catches the common violations while you type. The complete matrix (import
 * graph over every file, dynamic `import()`, `require`, node builtins, package entry
 * points) lives in `tests/layers/layers.test.ts` — that test is the authoritative
 * gate. Each zone declares its *complete* banned set: ESLint replaces (not merges) a
 * rule per matching block, so a zone that forgot a rule would silently allow it.
 */

const VSCODE = 'vscode';
const VSCODE_MESSAGE = 'Only src/host may import "vscode". src/webview and src/shared must stay portable.';
const CLIENT_PACKAGE = '@wing-agent/client';
const CLIENT_MESSAGE =
  'Only src/host may import the gateway capability layer: the renderer talks over the bridge, and src/shared stays dependency-free.';
/**
 * The session package is **not** restricted here: it is the environment-agnostic
 * view model + reduction lane (its own `tests/layers.test.ts` and the two tsconfigs
 * guard that), and every layer needs it — `src/shared` next to the channel constants,
 * the thin shell to render. What *is* restricted is the deep path: only the barrel is
 * a contract (see `tests/layers/layers.test.ts`).
 */
const SESSION_PACKAGE = '@wing-agent/session';
/**
 * The renderer package (`packages/ui`) and its two extra entries. Which entry a zone
 * may import is the interesting part (`tests/layers/layers.test.ts` is the authority):
 *
 * - `.` — the app (DOM/React/CSS): the thin shell mounts it, nothing else may;
 * - `./protocol` — the DOM-free wire contract: the host speaks it, the shell
 *   implements the transport with it;
 * - `./testing` — fixtures + scripted host: tests and the preview harness only.
 *
 * The patterns below allow the entries a zone is entitled to and ban every other
 * path into the package (a deep path is never a contract).
 */
const UI_PACKAGE = '@wing-agent/ui';
const UI_ENTRIES = {
  app: UI_PACKAGE,
  protocol: `${UI_PACKAGE}/protocol`,
  testing: `${UI_PACKAGE}/testing`,
};
const UI_DEEP_PATH_MESSAGE = `Import "${UI_PACKAGE}" through a public entry ("." , "./protocol", "./testing") — a path into the package is not a contract.`;

const HOST_BOUNDARY =
  'src/host must not import the renderer (src/webview): the host talks to it over the bridge only.';
const WEBVIEW_BOUNDARY =
  'src/webview must not import host code: the shell only mounts the renderer package and speaks its protocol.';
const SHARED_BOUNDARY =
  `src/shared is the channel constants and stays dependency-free: no vscode / host / webview / node imports, ` +
  `and no packages other than the session model it sits next to ("${SESSION_PACKAGE}").`;
const TEST_DOUBLES_MESSAGE = `${UI_ENTRIES.testing} is for tests/ and preview/ only — product code must not import fixtures or mocks.`;
const NODE_BUILTIN_BOUNDARY =
  'Node builtins are unavailable here: this layer also runs in the webview bundle.';

/** Host / renderer / shell layer directories as glob groups. */
const LAYER_GLOBS = {
  host: ['**/host/**'],
  webview: ['**/webview/**'],
};

/**
 * `no-restricted-imports` rule.
 *
 * `group` values are arrays (minimatch/gitignore patterns over the import string),
 * which is the shape ESLint 10 validates; negations inside a group (`!…`) are
 * supported and are how "these entries are fine, everything deeper is not" is
 * expressed. Note the gitignore semantics: a bare directory-like pattern
 * (`@wing-agent/ui`) also matches everything *under* it, which is exactly why the
 * exact entries are banned through `paths` (string equality) and only the deep
 * paths go through the group with negations.
 *
 * `uiEntries` lists the `@wing-agent/ui` entry points the zone may import; every
 * other entry and any deep path is banned with a message that says where to go.
 */
const restricted = ({ banVscode = false, paths = [], groups = [], uiEntries = [] }) => {
  const bannedUiEntries = Object.values(UI_ENTRIES).filter((entry) => !uiEntries.includes(entry));
  return [
    'error',
    {
      paths: [
        ...(banVscode ? [{ name: VSCODE, message: VSCODE_MESSAGE }] : []),
        ...paths,
        ...bannedUiEntries.map((entry) => ({
          name: entry,
          message: entry === UI_ENTRIES.testing ? TEST_DOUBLES_MESSAGE : UI_DEEP_PATH_MESSAGE,
        })),
      ],
      patterns: [
        ...groups.map(([group, message]) => ({ group, message })),
        {
          group: [`${SESSION_PACKAGE}/*`],
          message: `Import "${SESSION_PACKAGE}" through its barrel only: a path into the package is not a contract.`,
        },
        // Everything *under* the package: the entries above are the only public
        // ones, and a deep path is never a contract. Banned entries are negated here
        // so they are reported once, by their exact-path rule above.
        {
          group: [`${UI_PACKAGE}/*`, ...Object.values(UI_ENTRIES).map((entry) => `!${entry}`)],
          message: UI_DEEP_PATH_MESSAGE,
        },
      ],
    },
  ];
};

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
    // The extension host is the only layer allowed to touch the editor API — and
    // the only one allowed to speak to the gateway (via @wing-agent/client).
    files: ['src/host/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted({
        groups: [[LAYER_GLOBS.webview, HOST_BOUNDARY]],
        // The host speaks the wire contract and nothing else from the renderer
        // package — the app barrel would pull a document bundle into Node.
        uiEntries: [UI_ENTRIES.protocol],
      }),
    },
  },
  {
    files: ['src/shared/**/*.ts'],
    rules: {
      'no-restricted-imports': restricted({
        banVscode: true,
        paths: [{ name: CLIENT_PACKAGE, message: CLIENT_MESSAGE }],
        groups: [
          [LAYER_GLOBS.host, SHARED_BOUNDARY],
          [LAYER_GLOBS.webview, SHARED_BOUNDARY],
          [['node:*'], NODE_BUILTIN_BOUNDARY],
        ],
      }),
    },
  },
  {
    // The thin shell: the VS Code transport implementation and the entry that
    // mounts the renderer package.
    files: ['src/webview/**/*.ts', 'src/webview/**/*.tsx'],
    rules: {
      'no-restricted-imports': restricted({
        banVscode: true,
        paths: [{ name: CLIENT_PACKAGE, message: CLIENT_MESSAGE }],
        groups: [
          [LAYER_GLOBS.host, WEBVIEW_BOUNDARY],
          [['node:*'], NODE_BUILTIN_BOUNDARY],
        ],
        // The app it mounts, and the protocol its transport implements.
        uiEntries: [UI_ENTRIES.app, UI_ENTRIES.protocol],
      }),
    },
  },

  // ── React ──────────────────────────────────────────────────────────────
  {
    files: ['preview/**/*.tsx'],
    plugins: { 'react-hooks': reactHooks },
    rules: {
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'error',
    },
  },

  // ── Environment globals ────────────────────────────────────────────────
  {
    files: ['src/host/**/*.ts', 'esbuild.mjs', '*.config.mts'],
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
