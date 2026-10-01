import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { builtinModules } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import ts from 'typescript';
import { describe, expect, it } from 'vitest';

/**
 * Import-graph guard for the Electron shell — the authoritative gate for the
 * "electron is a boundary" promise (mechanism #3 of three; see `eslint.config.mjs`
 * and the single node tsconfig for the other two).
 *
 * It parses every source file and resolves every static import / re-export /
 * dynamic `import()` / `require()` / import type, then asserts:
 *
 * - nothing leaves `src/` (a relative import that escapes means the module
 *   depends on a consumer's layout);
 * - `electron` is imported **for value** only by `src/main.ts` and
 *   `src/preload.ts` (type-only imports are erased by esbuild and stay legal —
 *   that is how `src/menu.ts` speaks Electron's menu vocabulary);
 * - no other module: `apps/desktop` has zero runtime dependencies, so anything
 *   but a node builtin or electron is a packaging bug.
 *
 * It runs in `pnpm test`, so it is part of `make test-ts` and CI.
 */

const PACKAGE_ROOT = fileURLToPath(new URL('..', import.meta.url));
const SRC = path.join(PACKAGE_ROOT, 'src');

const NODE_BUILTINS = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);

const ELECTRON = 'electron';

/** The only files allowed to import electron for value. */
const ELECTRON_ENTRY_FILES = ['main.ts', 'preload.ts'];

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
  readonly kind: 'import' | 'require';
  /** `true` when the reference is erased at compile time (`import type …`, `import { type X } …`). */
  readonly typeOnly: boolean;
}

/** `import … from 'x'` (or `import {} from 'x'`) — type-only when nothing runtime-bound is imported. */
function importDeclarationIsTypeOnly(node: ts.ImportDeclaration): boolean {
  const clause = node.importClause;
  if (clause === undefined) {
    // `import 'x'` — a side-effect import runs at runtime.
    return false;
  }
  if (clause.isTypeOnly) {
    return true;
  }
  if (clause.name !== undefined) {
    // A default import is always a value.
    return false;
  }
  const bindings = clause.namedBindings;
  if (bindings === undefined) {
    return true;
  }
  if (ts.isNamespaceImport(bindings)) {
    return false;
  }
  return bindings.elements.length > 0 && bindings.elements.every((element) => element.isTypeOnly);
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
  const push = (value: string, kind: Specifier['kind'], typeOnly: boolean): void => {
    specifiers.push({ value, kind, typeOnly });
  };

  const visit = (node: ts.Node): void => {
    if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) {
      const { moduleSpecifier } = node;
      if (moduleSpecifier !== undefined && ts.isStringLiteral(moduleSpecifier)) {
        const typeOnly = ts.isImportDeclaration(node) ? importDeclarationIsTypeOnly(node) : node.isTypeOnly;
        push(moduleSpecifier.text, 'import', typeOnly);
      }
    } else if (ts.isImportTypeNode(node)) {
      // `import('electron').X` inside a type position: always erased.
      const argument = node.argument;
      if (ts.isLiteralTypeNode(argument) && ts.isStringLiteral(argument.literal)) {
        push(argument.literal.text, 'import', true);
      }
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference)) {
      const { expression } = node.moduleReference;
      if (expression !== undefined && ts.isStringLiteral(expression)) {
        push(expression.text, 'import', false);
      }
    } else if (ts.isCallExpression(node)) {
      const isDynamicImport = node.expression.kind === ts.SyntaxKind.ImportKeyword;
      const isRequire = ts.isIdentifier(node.expression) && node.expression.text === 'require';
      const [first] = node.arguments;
      if ((isDynamicImport || isRequire) && first !== undefined && ts.isStringLiteral(first)) {
        push(first.text, isRequire ? 'require' : 'import', false);
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

/** `main.ts`, `gateway/launcher.ts`, … — stable across platforms. */
function srcRelative(file: string): string {
  return path.relative(SRC, file).split(path.sep).join('/');
}

describe('packaging', () => {
  const sourceFiles = listFiles(SRC, ALLOWED_SRC_EXTENSIONS);

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

  it('keeps every relative import inside the package', () => {
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
            `${importer}: "${value}" escapes src/ — the shell must not depend on a consumer's layout`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('imports electron for value only from the two entry files', () => {
    const violations: string[] = [];
    for (const file of sourceFiles) {
      const relative = srcRelative(file);
      for (const { value, typeOnly } of collectModuleSpecifiers(file)) {
        if (value !== ELECTRON || typeOnly) {
          continue;
        }
        if (!ELECTRON_ENTRY_FILES.includes(relative)) {
          violations.push(
            `${relative}: imports electron for value — only ${ELECTRON_ENTRY_FILES.join(' / ')} may ` +
              `(use \`import type\` for Electron types)`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('keeps the entry files the ones that really import electron', () => {
    const importers = new Set<string>();
    for (const file of sourceFiles) {
      for (const { value, typeOnly } of collectModuleSpecifiers(file)) {
        if (value === ELECTRON && !typeOnly) {
          importers.add(srcRelative(file));
        }
      }
    }
    expect([...importers].sort()).toEqual([...ELECTRON_ENTRY_FILES].sort());
  });

  it('has no dependency other than electron and node builtins', () => {
    const violations: string[] = [];
    for (const file of sourceFiles) {
      const importer = path.relative(PACKAGE_ROOT, file);
      for (const { value } of collectModuleSpecifiers(file)) {
        if (value.startsWith('.') || value === ELECTRON || NODE_BUILTINS.has(value)) {
          continue;
        }
        violations.push(
          `${importer}: "${value}" — the shell has no runtime dependencies (electron + node builtins only)`,
        );
      }
    }
    expect(violations).toEqual([]);
  });
});
