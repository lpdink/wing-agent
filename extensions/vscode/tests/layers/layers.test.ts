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
 */

const PACKAGE_ROOT = fileURLToPath(new URL('../..', import.meta.url));
const SRC = path.join(PACKAGE_ROOT, 'src');
const PREVIEW = path.join(PACKAGE_ROOT, 'preview');

/** Layers of the single-package app, and what each may import. */
const LAYERS = ['shared', 'core', 'host', 'webview', 'testing'] as const;
type Layer = (typeof LAYERS)[number];

const NODE_BUILTINS = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);

/** `sibling` = same layer; `external` = npm packages. */
interface LayerRule {
  readonly siblings: boolean;
  readonly imports: readonly Layer[];
  readonly npm: boolean;
  readonly nodeBuiltins: boolean;
  readonly vscode: boolean;
}

const RULES: Record<Layer, LayerRule> = {
  // Contract types: consumed by both the node and the DOM project, so it must be
  // dependency-free (that is also what makes it safe to paste into an Electron app).
  shared: { siblings: true, imports: [], npm: false, nodeBuiltins: false, vscode: false },
  // Gateway client: Node runtime, no editor, no DOM (Electron seam).
  core: { siblings: true, imports: ['shared'], npm: true, nodeBuiltins: true, vscode: false },
  // Extension host: the only layer allowed to talk to VS Code.
  host: { siblings: true, imports: ['shared', 'core'], npm: true, nodeBuiltins: true, vscode: true },
  // Renderer: receives host-produced models over the bridge; only shared types.
  webview: { siblings: true, imports: ['shared'], npm: true, nodeBuiltins: false, vscode: false },
  // Fixtures + mocks: must stay portable (used by node tests *and* the jsdom ones).
  testing: { siblings: true, imports: ['shared'], npm: false, nodeBuiltins: false, vscode: false },
};

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
    if (targetLayer === 'testing') {
      violations.push({
        file: relativeTo(file),
        specifier,
        message: 'src/testing is test/preview-only; product code must not import fixtures or mocks',
      });
    }
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

interface CssViolation {
  readonly file: string;
  readonly line: number;
  readonly declaration: string;
}

/** Properties whose value must come from the theme, never from a literal. */
const COLOR_PROPERTY =
  /(?:^|-)(?:color|background|background-color|border|border-[a-z]+|outline|fill|stroke|box-shadow|text-decoration-color)$/;

const LITERAL_COLOR = /#[0-9a-fA-F]{3,8}\b|\brgba?\(|\bhsla?\(|\bcolor-mix\(|\bcolor\(/;

function checkCssColors(): CssViolation[] {
  const violations: CssViolation[] = [];
  // preview/ is excluded on purpose: `preview/preview-theme.css` *is* the emulated
  // editor theme (there is no VS Code to inject the variables there).
  for (const file of listFiles(SRC, ['.css'])) {
    const lines = readFileSync(file, 'utf8').split('\n');
    lines.forEach((line, index) => {
      const colon = line.indexOf(':');
      if (colon < 0 || line.trimStart().startsWith('*') || line.trimStart().startsWith('/*')) {
        return;
      }
      const property = line.slice(0, colon).trim();
      const value = line.slice(colon + 1).trim();
      if (!COLOR_PROPERTY.test(property)) {
        return;
      }
      if (LITERAL_COLOR.test(value)) {
        violations.push({
          file: path.relative(PACKAGE_ROOT, file),
          line: index + 1,
          declaration: line.trim(),
        });
      }
    });
  }
  return violations;
}

describe('layering', () => {
  const sourceFiles = listFiles(SRC, ['.ts', '.tsx']);

  it('finds source files to check', () => {
    expect(sourceFiles.length).toBeGreaterThan(5);
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

  it('keeps every color on a theme variable', () => {
    expect(
      checkCssColors().map((violation) => `${violation.file}:${violation.line} ${violation.declaration}`),
    ).toEqual([]);
  });

  it('has no imports of src/testing outside tests and preview', () => {
    const offenders: string[] = [];
    for (const file of sourceFiles) {
      if (layerOf(file) === 'testing') {
        continue;
      }
      for (const specifier of collectModuleSpecifiers(file)) {
        if (
          specifier.startsWith('.') &&
          (resolveRelative(file, specifier) ?? '').includes(`${path.sep}testing${path.sep}`)
        ) {
          offenders.push(`${path.relative(PACKAGE_ROOT, file)} → ${specifier}`);
        }
      }
    }
    expect(offenders).toEqual([]);
  });

  it('preview/ only imports preview, src/shared, src/testing and src/webview', () => {
    const violations: Violation[] = [];
    for (const file of listFiles(PREVIEW, ['.ts', '.tsx'])) {
      for (const specifier of collectModuleSpecifiers(file)) {
        if (!specifier.startsWith('.')) {
          if (
            specifier === 'vscode' ||
            NODE_BUILTINS.has(specifier) ||
            !['react', 'react-dom'].includes(specifier)
          ) {
            violations.push({
              file: path.relative(PACKAGE_ROOT, file),
              specifier,
              message: 'the preview harness may only import react, react-dom and its own sources',
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
        if (layer !== null && !['shared', 'testing', 'webview'].includes(layer)) {
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
