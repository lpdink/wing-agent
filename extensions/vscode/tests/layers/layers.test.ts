import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs';
import { builtinModules } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import ts from 'typescript';
import { describe, expect, it } from 'vitest';

/**
 * Layer guard — mechanism #3 of three (see design.md D2), and the authoritative one.
 *
 * ESLint catches the common violations while typing and the split tsconfigs stop
 * DOM/node globals from leaking across layers, but neither can express the full
 * dependency matrix: this test parses every source file, resolves every static
 * import / re-export / dynamic `import()` / `require()`, and asserts the matrix.
 *
 * It runs in `pnpm test`, so it is part of `make test` and CI.
 *
 * The renderer used to be a layer here (`src/webview/**` + the bridge protocol in
 * `src/shared/**` + the fixtures in `src/testing/**`). It is `@wing-agent/ui`
 * (`packages/ui`) now; what stayed in this package is the thin shell — the VS Code
 * transport and the entry — plus the channel constants. The UI package's own
 * import-graph gate (dependency allowlist, colours on theme variables, barrel
 * coverage) lives in `packages/ui/tests/layers.test.ts`.
 */

const PACKAGE_ROOT = fileURLToPath(new URL('../..', import.meta.url));
const SRC = path.join(PACKAGE_ROOT, 'src');
const PREVIEW = path.join(PACKAGE_ROOT, 'preview');

/**
 * Layers of the extension, and what each may import.
 *
 * The gateway capability layer used to be `src/core`; it now lives in the
 * `@wing-agent/client` workspace package and is treated as an external package
 * whose only legal importer is `host` (see `CLIENT_PACKAGE` below). The session
 * lane and the renderer are workspace packages too (`@wing-agent/session`,
 * `@wing-agent/ui`) with their own environment gates travelling with them.
 */
const LAYERS = ['shared', 'host', 'webview'] as const;
type Layer = (typeof LAYERS)[number];

const NODE_BUILTINS = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);

/**
 * The extracted gateway capability layer.
 *
 * Not a layer directory any more, so the matrix cannot express it: it is checked
 * separately — only `host` reaches the gateway, the renderer and the contract
 * types never do (that is exactly what the old `core` rule said).
 */
const CLIENT_PACKAGE = '@wing-agent/client';

/**
 * The extracted session reduction lane (`packages/session`).
 *
 * It is the session view model + the reduction that produces it — the vocabulary
 * `src/shared` used to hold itself. Two rules, checked below:
 *
 * - **every** layer may import it (the model is what the bridge carries and what
 *   the fixtures build; blocking it anywhere would only force a second copy of the
 *   types, which is what the extraction removed);
 * - **only the barrel**: `@wing-agent/session` is the contract, a deep path into
 *   the package is not (`packages/session/tests/layers.test.ts` keeps that
 *   promise from the other side).
 */
const SESSION_PACKAGE = '@wing-agent/session';

/**
 * The renderer (`packages/ui`) — and the bridge protocol it owns.
 *
 * Three public entries, each with its own audience:
 *
 * - `@wing-agent/ui` — the app itself: DOM, React, CSS. Only the **thin shell**
 *   (`src/webview`, which mounts it) may import it; the host must not (it would pull
 *   a document bundle into the extension host).
 * - `@wing-agent/ui/protocol` — the DOM-free wire contract (message unions, guards,
 *   the `WebviewTransport` interface, the patch receiver). **Any** layer may import
 *   it: the host speaks it (`src/host/bridge.ts`), `src/shared` describes *this*
 *   channel's constants next to it, and the shell implements the transport with it.
 * - `@wing-agent/ui/testing` — fixtures + scripted host. Product code must never
 *   import it (the same rule `src/testing` used to carry); `tests/` and `preview/`
 *   do.
 *
 * A deep path into the package is a violation, always: `packages/ui/tests/layers.test.ts`
 * keeps the barrel-only promise from the other side.
 */
const UI_PACKAGE = '@wing-agent/ui';

const UI_PROTOCOL_ENTRY = `${UI_PACKAGE}/protocol`;
const UI_TESTING_ENTRY = `${UI_PACKAGE}/testing`;

/** `sibling` = same layer; `external` = npm packages. */
interface LayerRule {
  readonly siblings: boolean;
  readonly imports: readonly Layer[];
  readonly npm: boolean;
  readonly nodeBuiltins: boolean;
  readonly vscode: boolean;
}

const RULES: Record<Layer, LayerRule> = {
  // Channel constants: consumed by both the node and the DOM project, so it must be
  // dependency-free (that is also what makes it safe to paste into an Electron app).
  shared: { siblings: true, imports: [], npm: false, nodeBuiltins: false, vscode: false },
  // Extension host: the only layer allowed to talk to VS Code (and to the gateway).
  host: { siblings: true, imports: ['shared'], npm: true, nodeBuiltins: true, vscode: true },
  // Thin shell: the VS Code transport + the entry that mounts the renderer package.
  webview: { siblings: true, imports: ['shared'], npm: true, nodeBuiltins: false, vscode: false },
};

/**
 * Files under `dir`.
 *
 * Omit `extensions` to list **every** file (used by the coverage assertions, so a
 * stray `.js`/`.json` cannot slip past them — the matrix enumerates files, not
 * "the files we happen to parse"; review r2 [N-R2-2]).
 */
function listFiles(dir: string, extensions?: readonly string[]): string[] {
  if (!existsSync(dir)) {
    return [];
  }
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      found.push(...listFiles(full, extensions));
    } else if (extensions === undefined || extensions.some((extension) => entry.name.endsWith(extension))) {
      found.push(full);
    }
  }
  return found;
}

/**
 * Extensions allowed under `src/`: TypeScript sources and markdown notes.
 * Deliberately **no** `.js/.jsx/.mjs/.cjs/.mts/.cts`: the whole toolchain
 * (tsconfig `include`, the ESLint layer zones, the Vite entry, esbuild) is
 * TypeScript-only, so a JS module there would be invisible to typecheck *and* to
 * lint even after the matrix learned to parse it.
 *
 * CSS is gone with the renderer: the stylesheets (and the check that every colour
 * comes from a theme variable) live in `packages/ui`.
 */
const ALLOWED_SRC_EXTENSIONS = ['.ts', '.tsx', '.md'];

function layerOf(file: string): Layer | null {
  const relative = path.relative(SRC, file);
  if (relative.startsWith('..')) {
    return null;
  }
  const [first] = relative.split(path.sep);
  return first !== undefined && (LAYERS as readonly string[]).includes(first) ? (first as Layer) : null;
}

/** Resolve a relative specifier to a file on disk (TS/Vite style). */
function resolveRelative(fromFile: string, specifier: string): string | null {
  const base = path.resolve(path.dirname(fromFile), specifier);
  const candidates = [
    base,
    `${base}.ts`,
    `${base}.tsx`,
    `${base}.js`,
    `${base}.d.ts`,
    path.join(base, 'index.ts'),
    path.join(base, 'index.tsx'),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate) && statSync(candidate).isFile()) {
      return candidate;
    }
  }
  return null;
}

/** Every module specifier a file pulls in, whatever the syntax. */
function collectModuleSpecifiers(file: string): string[] {
  const source = ts.createSourceFile(
    file,
    readFileSync(file, 'utf8'),
    ts.ScriptTarget.Latest,
    /* setParentNodes */ true,
    file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  );

  const specifiers: string[] = [];

  const visit = (node: ts.Node): void => {
    if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
      const { moduleSpecifier } = node;
      if (moduleSpecifier !== undefined && ts.isStringLiteral(moduleSpecifier)) {
        specifiers.push(moduleSpecifier.text);
      }
    } else if (ts.isImportTypeNode(node)) {
      const argument = node.argument;
      if (ts.isLiteralTypeNode(argument) && ts.isStringLiteral(argument.literal)) {
        specifiers.push(argument.literal.text);
      }
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
      const { expression } = node.moduleReference;
      if (expression !== undefined && ts.isStringLiteral(expression)) {
        specifiers.push(expression.text);
      }
    } else if (ts.isCallExpression(node)) {
      const isDynamicImport = node.expression.kind === ts.SyntaxKind.ImportKeyword;
      const isRequire = ts.isIdentifier(node.expression) && node.expression.text === 'require';
      const [first] = node.arguments;
      if ((isDynamicImport || isRequire) && first !== undefined && ts.isStringLiteral(first)) {
        specifiers.push(first.text);
      }
    }
    ts.forEachChild(node, visit);
  };

  visit(source);
  return specifiers;
}

interface Violation {
  readonly file: string;
  readonly specifier: string;
  readonly message: string;
}

function checkSpecifier(file: string, specifier: string, violations: Violation[]): void {
  const from = layerOf(file);
  if (from === null) {
    return; // preview/** or anything outside src/ — not part of the product matrix.
  }
  const rule = RULES[from];
  const relativeTo = (target: string): string => path.relative(PACKAGE_ROOT, target);

  if (specifier.startsWith('.')) {
    const resolved = resolveRelative(file, specifier);
    if (resolved === null) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: 'unresolvable relative import (typo, or a missing file in the build)',
      });
      return;
    }
    const targetLayer = layerOf(resolved);
    if (targetLayer === null) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: `import target ${relativeTo(resolved)} is outside src/ — product code may only import from src/**`,
      });
      return;
    }
    if (targetLayer === from) {
      if (!rule.siblings) {
        violations.push({
          file: relativeTo(file),
          specifier,
          message: `layer "${from}" may not import itself`,
        });
      }
      return;
    }
    if (!rule.imports.includes(targetLayer)) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: `layer "${from}" must not import layer "${targetLayer}" (allowed: ${['self', ...rule.imports].join(', ')})`,
      });
    }
    return;
  }

  if (specifier === CLIENT_PACKAGE || specifier.startsWith(`${CLIENT_PACKAGE}/`)) {
    if (from !== 'host') {
      violations.push({
        file: relativeTo(file),
        specifier,
        message:
          `only src/host may import "${CLIENT_PACKAGE}" (the gateway capability layer): ` +
          'the renderer talks over the bridge, and src/shared stays dependency-free',
      });
    }
    return;
  }

  if (specifier === SESSION_PACKAGE || specifier.startsWith(`${SESSION_PACKAGE}/`)) {
    // Any layer may reach the session package (see SESSION_PACKAGE above) — but
    // only through its barrel: a deep path would make the package's file layout a
    // contract and let a consumer bypass the single truth the extraction created.
    if (specifier !== SESSION_PACKAGE) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: `import "${SESSION_PACKAGE}" through its barrel only — a path into the package is not a contract`,
      });
    }
    return;
  }

  if (specifier === UI_PACKAGE || specifier.startsWith(`${UI_PACKAGE}/`)) {
    if (specifier === UI_PROTOCOL_ENTRY) {
      // The DOM-free wire contract: the host speaks it, the shell implements the
      // transport with it, and the constants module sits next to it.
      return;
    }
    if (specifier === UI_PACKAGE) {
      if (from !== 'webview') {
        violations.push({
          file: relativeTo(file),
          specifier,
          message:
            `only src/webview (the thin shell that mounts the renderer) may import "${UI_PACKAGE}"; ` +
            `the host side speaks "${UI_PROTOCOL_ENTRY}" instead`,
        });
      }
      return;
    }
    if (specifier === UI_TESTING_ENTRY) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: `${UI_TESTING_ENTRY} is for tests/ and preview/ only — product code must not import fixtures or mocks`,
      });
      return;
    }
    violations.push({
      file: relativeTo(file),
      specifier,
      message: `"${UI_PACKAGE}" exposes three entries — "." , "${UI_PROTOCOL_ENTRY}" and "${UI_TESTING_ENTRY}"; a path into the package is not a contract`,
    });
    return;
  }

  if (NODE_BUILTINS.has(specifier)) {
    if (!rule.nodeBuiltins) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: `layer "${from}" must not use node builtins (it also runs in the browser bundle)`,
      });
    }
    return;
  }

  if (specifier === 'vscode' || specifier.startsWith('vscode/')) {
    if (!rule.vscode) {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: `only src/host may import "vscode" (layer "${from}" must stay portable)`,
      });
    }
    return;
  }

  if (!rule.npm) {
    violations.push({
      file: relativeTo(file),
      specifier,
      message: `layer "${from}" must stay dependency-free (no npm imports)`,
    });
  }
}

describe('layering', () => {
  const sourceFiles = listFiles(SRC, ['.ts', '.tsx']);

  it('finds source files to check', () => {
    expect(sourceFiles.length).toBeGreaterThan(3);
  });

  it('each layer directory exists (so the matrix has a target)', () => {
    const missing = LAYERS.filter((layer) => !existsSync(path.join(SRC, layer)));
    expect(missing).toEqual([]);
  });

  it('respects the dependency matrix for every import form', () => {
    const violations: Violation[] = [];
    for (const file of sourceFiles) {
      for (const specifier of collectModuleSpecifiers(file)) {
        checkSpecifier(file, specifier, violations);
      }
    }
    expect(
      violations.map((violation) => `${violation.file}: "${violation.specifier}" — ${violation.message}`),
    ).toEqual([]);
  });

  it('keeps src/ covered by the matrix: every file lives in a declared layer', () => {
    // Every *file*, not just the parsed extensions: a `.js`/`.json`/anything under
    // src/ must still be inside a declared layer.
    const strays = listFiles(SRC)
      .map((file) => path.relative(SRC, file))
      .filter((relative) => {
        const [first] = relative.split(path.sep);
        return first === undefined || !(LAYERS as readonly string[]).includes(first);
      });
    expect(
      strays.map(
        (relative) => `src/${relative} — new layer dir needs a rule (or move the file into a declared layer)`,
      ),
    ).toEqual([]);
  });

  it('keeps src/ TypeScript-only (a JS module here would bypass every gate)', () => {
    const foreign = listFiles(SRC)
      .filter((file) => !ALLOWED_SRC_EXTENSIONS.some((extension) => file.endsWith(extension)))
      .map(
        (file) =>
          `${path.relative(PACKAGE_ROOT, file)} — src/ is TypeScript-only (allowed: ${ALLOWED_SRC_EXTENSIONS.join(', ')}); ` +
          'a JS module would be invisible to typecheck, to the ESLint layer zones and to the bundlers',
      );
    expect(foreign).toEqual([]);
  });

  it('preview/ only imports preview, src/shared, src/webview, the session package and the ui package', () => {
    const violations: Violation[] = [];
    for (const file of listFiles(PREVIEW, ['.ts', '.tsx'])) {
      for (const specifier of collectModuleSpecifiers(file)) {
        if (!specifier.startsWith('.')) {
          if (
            specifier === 'vscode' ||
            NODE_BUILTINS.has(specifier) ||
            ![
              'react',
              'react-dom',
              SESSION_PACKAGE,
              UI_PACKAGE,
              UI_TESTING_ENTRY,
              UI_PROTOCOL_ENTRY,
            ].includes(specifier)
          ) {
            violations.push({
              file: path.relative(PACKAGE_ROOT, file),
              specifier,
              message:
                'the preview harness may only import react, react-dom, the session/ui packages and its own sources',
            });
          }
          continue;
        }
        const resolved = resolveRelative(file, specifier);
        if (resolved === null) {
          violations.push({
            file: path.relative(PACKAGE_ROOT, file),
            specifier,
            message: 'unresolvable import',
          });
          continue;
        }
        if (path.relative(PACKAGE_ROOT, resolved).startsWith('..')) {
          violations.push({
            file: path.relative(PACKAGE_ROOT, file),
            specifier,
            message: 'escaping the package root',
          });
          continue;
        }
        const layer = layerOf(resolved);
        if (layer !== null && !['shared', 'webview'].includes(layer)) {
          violations.push({
            file: path.relative(PACKAGE_ROOT, file),
            specifier,
            message: `preview must not import src/${layer}`,
          });
        }
      }
    }
    expect(
      violations.map((violation) => `${violation.file}: "${violation.specifier}" — ${violation.message}`),
    ).toEqual([]);
  });
});
