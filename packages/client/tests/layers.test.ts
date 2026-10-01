import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs';
import { builtinModules } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import ts from 'typescript';
import { describe, expect, it } from 'vitest';

/**
 * Import-graph guard for `@wing-agent/client` — the authoritative gate for the
 * package's "environment-agnostic" promise (mechanism #3 of three; see
 * `eslint.config.mjs` and the two tsconfigs for the other two).
 *
 * It parses every source file, resolves every static import / re-export / dynamic
 * `import()` / `require()`, and asserts:
 *
 * - nothing leaves the package (a relative import that escapes `src/` means a
 *   dependency on the consumer's layout — the exact thing this package exists to
 *   avoid);
 * - no node builtin, no `vscode`, no npm import — with exactly one documented
 *   exception, the lazy `require('ws')` fallback in `transport/socket.ts`.
 *
 * It runs in `pnpm test`, so it is part of `make test-ts` and CI along with every
 * other package's gate.
 */

const PACKAGE_ROOT = fileURLToPath(new URL('..', import.meta.url));
const SRC = path.join(PACKAGE_ROOT, 'src');

const NODE_BUILTINS = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);

/**
 * The one external module reference this package may contain: the bundled `ws`
 * client, loaded lazily by `createNativeSocketFactory` on hosts without a global
 * `WebSocket` (Node < 22 — VS Code 1.100 / Electron 34). It must stay a `require`
 * call so esbuild can bundle it into the consumer.
 */
const FALLBACK_MODULE = 'ws';
const FALLBACK_FILE = path.join('transport', 'socket.ts');

/** `src/` is pure TypeScript: no `.js`, no emitted artifacts. */
const ALLOWED_SRC_EXTENSIONS = ['.ts'];

/** Files under `dir` (recursive). */
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

interface Specifier {
  readonly value: string;
  /** `import` covers `import … from`, `export … from` and dynamic `import()`. */
  readonly kind: 'import' | 'require';
}

/** Every module specifier a file pulls in, whatever the syntax. */
function collectModuleSpecifiers(file: string): Specifier[] {
  const source = ts.createSourceFile(
    file,
    readFileSync(file, 'utf8'),
    ts.ScriptTarget.Latest,
    /* setParentNodes */ true,
    ts.ScriptKind.TS,
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
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
      const { expression } = node.moduleReference;
      if (expression !== undefined && ts.isStringLiteral(expression)) {
        push(expression.text, 'import');
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
  const candidates = [base, `${base}.ts`, `${base}.js`, `${base}.d.ts`, path.join(base, 'index.ts')];
  for (const candidate of candidates) {
    if (existsSync(candidate) && statSync(candidate).isFile()) {
      return candidate;
    }
  }
  return null;
}

describe('packaging', () => {
  const sourceFiles = listFiles(SRC, ['.ts']);

  it('finds source files to check', () => {
    expect(sourceFiles.length).toBeGreaterThan(5);
  });

  it('keeps src/ TypeScript-only', () => {
    const foreign = listFiles(SRC)
      .filter((file) => !ALLOWED_SRC_EXTENSIONS.some((extension) => file.endsWith(extension)))
      .map(
        (file) =>
          `${path.relative(PACKAGE_ROOT, file)} — src/ is TypeScript-only (allowed: ${ALLOWED_SRC_EXTENSIONS.join(', ')})`,
      );
    expect(foreign).toEqual([]);
  });

  it('keeps every import inside the package', () => {
    const violations: string[] = [];
    for (const file of sourceFiles) {
      for (const { value } of collectModuleSpecifiers(file)) {
        if (!value.startsWith('.')) {
          continue;
        }
        const resolved = resolveRelative(file, value);
        const importer = path.relative(PACKAGE_ROOT, file);
        if (resolved === null) {
          violations.push(`${importer}: "${value}" does not resolve`);
          continue;
        }
        if (path.relative(SRC, resolved).startsWith('..')) {
          violations.push(
            `${importer}: "${value}" escapes src/ — the package must not depend on a consumer's layout`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('has no external dependency apart from the lazy ws fallback', () => {
    const violations: string[] = [];
    for (const file of sourceFiles) {
      const relative = path.relative(SRC, file);
      for (const { value, kind } of collectModuleSpecifiers(file)) {
        if (value.startsWith('.')) {
          continue;
        }
        const importer = path.relative(PACKAGE_ROOT, file);
        if (NODE_BUILTINS.has(value)) {
          violations.push(`${importer}: "${value}" is a node builtin — unavailable in a browser shell`);
        } else if (value === 'vscode' || value.startsWith('vscode/')) {
          violations.push(`${importer}: "${value}" — only a VS Code host may import the editor API`);
        } else if (value === FALLBACK_MODULE && kind === 'require' && relative.endsWith(FALLBACK_FILE)) {
          continue; // the documented exception
        } else {
          violations.push(
            `${importer}: "${value}" — the package has no npm dependencies ` +
              `(the only exception is a lazy require("${FALLBACK_MODULE}") in ${FALLBACK_FILE})`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('keeps the ws fallback lazy and bundled (a static import would break the smoke bundle)', () => {
    const socket = readFileSync(path.join(SRC, FALLBACK_FILE), 'utf8');
    expect(socket).toContain(`require('${FALLBACK_MODULE}')`);
    expect(socket).not.toMatch(new RegExp(`^import[\\s\\S]*?from '${FALLBACK_MODULE}'`, 'm'));
  });
});
