import { builtinModules } from 'node:module';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import ts from 'typescript';
import { describe, expect, it } from 'vitest';

/**
 * Import-graph guard for the web app — the authoritative layering gate (the two
 * tsconfigs and the ESLint zones are the fast, per-file copies of it; design.md D1).
 *
 * It parses every source file, resolves every static import / re-export / dynamic
 * `import()` / `require()`, and asserts:
 *
 * - `src/**` is the browser bundle: no node builtin, no `tests/` import, and no
 *   deep path into a workspace package (only its barrel — plus the two declared
 *   subpaths `@wing-agent/ui/protocol` and `@wing-agent/ui/styles/*` — is a contract);
 * - the framework-free core (`src/lib`, `src/settings`, `src/sessions`,
 *   `src/connection`) never imports React;
 * - `src/lib` and `src/settings` carry no session semantics;
 * - `tools/**` (the screenshot infrastructure, node) never reaches into `src/**`:
 *   it drives the *built* app through a browser, not the app's modules.
 *
 * It runs in `pnpm test`, so it is part of `make test-ts` and CI.
 */

const APP_ROOT = fileURLToPath(new URL('..', import.meta.url));
const SRC = path.join(APP_ROOT, 'src');
const NODE_BUILTINS = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);

const CLIENT = '@wing-agent/client';
const SESSION = '@wing-agent/session';
const UI = '@wing-agent/ui';
/**
 * The allowed subpaths beside the barrels: `@wing-agent/ui/protocol` is that
 * package's declared DOM-free wire contract, and `@wing-agent/ui/styles/*.css` its
 * declared theme sheets (both are entries of its `exports` map, and the package's
 * own layer guard enumerates them). Everything else has to go through the barrel,
 * exactly like `@wing-agent/client` / `@wing-agent/session`.
 */
const UI_PROTOCOL = '@wing-agent/ui/protocol';
/**
 * …and the second: the five theme sheets of the package's `./styles/*` export.
 * `src/main.tsx` imports them once (they are the ported design tokens the shared
 * cards are drawn with); nothing else may reach into the package, and the sheets
 * themselves are CSS, so they cannot smuggle code into the bundle.
 */
const UI_STYLES = [
  '@wing-agent/ui/styles/design-platform.css',
  '@wing-agent/ui/styles/base.css',
  '@wing-agent/ui/styles/scrollbar.css',
  '@wing-agent/ui/styles/focus.css',
  '@wing-agent/ui/styles/shiki.css',
] as const;

const SRC_EXTERNALS = [
  CLIENT,
  SESSION,
  UI,
  UI_PROTOCOL,
  ...UI_STYLES,
  'react',
  'react-dom',
  'react-dom/client',
  'react/jsx-runtime',
];
const TEST_EXTERNALS = [
  ...SRC_EXTERNALS,
  'vitest',
  'typescript',
  '@testing-library/react',
  '@testing-library/dom',
];
const TOOL_EXTERNALS = [CLIENT, SESSION, 'ws', 'playwright', 'esbuild'];

/** Files under `dir` (recursive), filtered by extension. */
function listFiles(dir: string, extensions: readonly string[]): string[] {
  if (!existsSync(dir)) {
    return [];
  }
  const found: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      found.push(...listFiles(full, extensions));
    } else if (extensions.some((extension) => entry.name.endsWith(extension))) {
      found.push(full);
    }
  }
  return found;
}

interface Specifier {
  readonly value: string;
  readonly kind: 'import' | 'require';
}

/** Every module specifier a file pulls in, whatever the syntax. */
function collectModuleSpecifiers(file: string): Specifier[] {
  const source = ts.createSourceFile(
    file,
    readFileSync(file, 'utf8'),
    ts.ScriptTarget.Latest,
    /* setParentNodes */ true,
    file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  );
  const specifiers: Specifier[] = [];
  const push = (value: string, kind: Specifier['kind']): void => {
    specifiers.push({ value, kind });
  };
  const visit = (node: ts.Node): void => {
    if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
      const { moduleSpecifier } = node;
      if (moduleSpecifier !== undefined && ts.isStringLiteral(moduleSpecifier)) {
        push(moduleSpecifier.text, 'import');
      }
    } else if (ts.isImportTypeNode(node)) {
      const argument = node.argument;
      if (ts.isLiteralTypeNode(argument) && ts.isStringLiteral(argument.literal)) {
        push(argument.literal.text, 'import');
      }
    } else if (ts.isCallExpression(node)) {
      const isDynamicImport = node.expression.kind === ts.SyntaxKind.ImportKeyword;
      const isRequire = ts.isIdentifier(node.expression) && node.expression.text === 'require';
      const [first] = node.arguments;
      if ((isDynamicImport || isRequire) && first !== undefined && ts.isStringLiteral(first)) {
        push(first.text, isRequire ? 'require' : 'import');
      }
    }
    ts.forEachChild(node, visit);
  };
  visit(source);
  return specifiers;
}

/** Resolve a relative specifier to a file on disk (TS/Vite style). */
function resolveRelative(fromFile: string, specifier: string): string | null {
  const base = path.resolve(path.dirname(fromFile), specifier);
  const candidates = [
    base,
    `${base}.ts`,
    `${base}.tsx`,
    `${base}.css`,
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

function isInside(child: string, parent: string): boolean {
  const relative = path.relative(parent, child);
  return relative !== '' && !relative.startsWith('..') && !path.isAbsolute(relative);
}

function isRelative(specifier: string): boolean {
  return specifier.startsWith('./') || specifier.startsWith('../');
}

const sourceFiles = listFiles(SRC, ['.ts', '.tsx']);
const testFiles = listFiles(path.join(APP_ROOT, 'tests'), ['.ts', '.tsx']);
const toolFiles = listFiles(path.join(APP_ROOT, 'tools'), ['.ts']);

function externalAllowed(specifier: string, allowlist: readonly string[]): boolean {
  if (allowlist.includes(specifier)) {
    return true;
  }
  // `@wing-agent/client/protocol/…` and `react-dom/client` style subpaths: only the
  // exact barrel entries above are contracts, so a subpath is never allowed.
  return false;
}

describe('layer guard: src', () => {
  it('has sources to check (the guard is not vacuous)', () => {
    expect(sourceFiles.length).toBeGreaterThan(8);
  });

  it('imports only relative modules, the package barrels and react', () => {
    const violations: string[] = [];
    for (const file of sourceFiles) {
      for (const specifier of collectModuleSpecifiers(file)) {
        const short = path.relative(APP_ROOT, file);
        if (isRelative(specifier.value)) {
          const resolved = resolveRelative(file, specifier.value);
          if (resolved === null) {
            violations.push(`${short}: unresolved relative import "${specifier.value}"`);
          } else if (!isInside(resolved, SRC)) {
            violations.push(`${short}: "${specifier.value}" leaves src/`);
          }
          continue;
        }
        if (NODE_BUILTINS.has(specifier.value)) {
          violations.push(`${short}: node builtin "${specifier.value}" in the browser bundle`);
          continue;
        }
        if (!externalAllowed(specifier.value, SRC_EXTERNALS)) {
          violations.push(`${short}: unexpected external import "${specifier.value}"`);
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('keeps the framework-free core free of React', () => {
    const core = sourceFiles.filter((file) =>
      ['connection', 'settings', 'sessions', 'lib', 'images', 'bridge'].some((dir) =>
        isInside(file, path.join(SRC, dir)),
      ),
    );
    expect(core.length).toBeGreaterThan(3);
    const violations: string[] = [];
    for (const file of core) {
      for (const specifier of collectModuleSpecifiers(file)) {
        if (/^react(-dom)?(\/|$)/.test(specifier.value)) {
          violations.push(`${path.relative(APP_ROOT, file)}: imports "${specifier.value}"`);
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('keeps session semantics out of settings and lib', () => {
    const lower = sourceFiles.filter(
      (file) => isInside(file, path.join(SRC, 'settings')) || isInside(file, path.join(SRC, 'lib')),
    );
    expect(lower.length).toBeGreaterThan(2);
    const violations: string[] = [];
    for (const file of lower) {
      for (const specifier of collectModuleSpecifiers(file)) {
        if (specifier.value === SESSION || specifier.value.startsWith(`${SESSION}/`)) {
          violations.push(`${path.relative(APP_ROOT, file)}: imports "${specifier.value}"`);
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('never reaches into tests/ or tools/', () => {
    const violations: string[] = [];
    for (const file of sourceFiles) {
      for (const specifier of collectModuleSpecifiers(file)) {
        if (/(^|\/)tests?\/|(^|\/)tools?\//.test(specifier.value)) {
          violations.push(`${path.relative(APP_ROOT, file)}: imports "${specifier.value}"`);
        }
      }
    }
    expect(violations).toEqual([]);
  });
});

describe('layer guard: tests and tools', () => {
  it('lets tests import the app and the tooling, nothing else', () => {
    const violations: string[] = [];
    for (const file of testFiles) {
      for (const specifier of collectModuleSpecifiers(file)) {
        const short = path.relative(APP_ROOT, file);
        const value = specifier.value;
        if (isRelative(value)) {
          const resolved = resolveRelative(file, value);
          if (resolved === null) {
            violations.push(`${short}: unresolved relative import "${value}"`);
            continue;
          }
          // Tests may exercise the app and the node tooling (both are node-side
          // here); they must not reach anywhere else (the repo, the packages' files).
          const inside =
            isInside(resolved, path.join(APP_ROOT, 'tests')) ||
            isInside(resolved, SRC) ||
            isInside(resolved, path.join(APP_ROOT, 'tools'));
          if (!inside) {
            violations.push(`${short}: "${value}" escapes tests/, src/ and tools/`);
          }
          continue;
        }
        if (NODE_BUILTINS.has(value) || externalAllowed(value, TEST_EXTERNALS)) {
          continue;
        }
        violations.push(`${short}: unexpected external import "${value}"`);
      }
    }
    expect(violations).toEqual([]);
  });

  it('keeps the tooling out of the app sources', () => {
    const violations: string[] = [];
    for (const file of toolFiles) {
      for (const specifier of collectModuleSpecifiers(file)) {
        const short = path.relative(APP_ROOT, file);
        const value = specifier.value;
        if (isRelative(value)) {
          const resolved = resolveRelative(file, value);
          if (resolved === null || isInside(resolved, SRC)) {
            violations.push(`${short}: "${value}" must not reach into src/`);
          }
          continue;
        }
        if (NODE_BUILTINS.has(value) || externalAllowed(value, TOOL_EXTERNALS)) {
          continue;
        }
        violations.push(`${short}: unexpected external import "${value}"`);
      }
    }
    expect(violations).toEqual([]);
  });
});
