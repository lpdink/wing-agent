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
 * Extensions allowed under `src/`: TypeScript sources, CSS modules and markdown
 * notes. Deliberately **no** `.js/.jsx/.mjs/.cjs/.mts/.cts`: the whole toolchain
 * (tsconfig `include`, the ESLint layer zones, the Vite entry, esbuild) is
 * TypeScript-only, so a JS module there would be invisible to typecheck *and* to
 * lint even after the matrix learned to parse it.
 */
const ALLOWED_SRC_EXTENSIONS = ['.ts', '.tsx', '.css', '.md'];

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

/**
 * Properties whose value must come from the theme, never from a literal.
 *
 * `--*` (custom properties) are included explicitly: `app.module.css` derives
 * `--wing-*` aliases from `--vscode-*`, and without this a literal could hide
 * behind a custom property name and reach a real color through `var()`.
 */
const COLOR_PROPERTY =
  /(?:^|-)(?:color|background|background-color|border|border-[a-z]+|outline|fill|stroke|box-shadow|text-decoration-color)$/;

/** Literal color syntaxes: hex, the color functions, and `color(…)`. */
const LITERAL_COLOR = /#[0-9a-fA-F]{3,8}\b|\brgba?\(|\bhsla?\(|\bcolor-mix\(|\bcolor\(/;

/**
 * CSS named colors (the full keyword set), minus the two that carry theme meaning
 * rather than a fixed color: `transparent` / `currentColor` are used as legitimate
 * fallbacks (e.g. `var(--vscode-panel-border, transparent)`).
 *
 * Source: CSS Color Module Level 4, "Named colors" (plus the `gray`/`grey`
 * spellings).
 */
const NAMED_COLORS = [
  'aliceblue',
  'antiquewhite',
  'aqua',
  'aquamarine',
  'azure',
  'beige',
  'bisque',
  'black',
  'blanchedalmond',
  'blue',
  'blueviolet',
  'brown',
  'burlywood',
  'cadetblue',
  'chartreuse',
  'chocolate',
  'coral',
  'cornflowerblue',
  'cornsilk',
  'crimson',
  'cyan',
  'darkblue',
  'darkcyan',
  'darkgoldenrod',
  'darkgray',
  'darkgreen',
  'darkgrey',
  'darkkhaki',
  'darkmagenta',
  'darkolivegreen',
  'darkorange',
  'darkorchid',
  'darkred',
  'darksalmon',
  'darkseagreen',
  'darkslateblue',
  'darkslategray',
  'darkslategrey',
  'darkturquoise',
  'darkviolet',
  'deeppink',
  'deepskyblue',
  'dimgray',
  'dimgrey',
  'dodgerblue',
  'firebrick',
  'floralwhite',
  'forestgreen',
  'fuchsia',
  'gainsboro',
  'ghostwhite',
  'gold',
  'goldenrod',
  'gray',
  'green',
  'greenyellow',
  'grey',
  'honeydew',
  'hotpink',
  'indianred',
  'indigo',
  'ivory',
  'khaki',
  'lavender',
  'lavenderblush',
  'lawngreen',
  'lemonchiffon',
  'lightblue',
  'lightcoral',
  'lightcyan',
  'lightgoldenrodyellow',
  'lightgray',
  'lightgreen',
  'lightgrey',
  'lightpink',
  'lightsalmon',
  'lightseagreen',
  'lightskyblue',
  'lightslategray',
  'lightslategrey',
  'lightsteelblue',
  'lightyellow',
  'lime',
  'limegreen',
  'linen',
  'magenta',
  'maroon',
  'mediumaquamarine',
  'mediumblue',
  'mediumorchid',
  'mediumpurple',
  'mediumseagreen',
  'mediumslateblue',
  'mediumspringgreen',
  'mediumturquoise',
  'mediumvioletred',
  'midnightblue',
  'mintcream',
  'mistyrose',
  'moccasin',
  'navajowhite',
  'navy',
  'oldlace',
  'olive',
  'olivedrab',
  'orange',
  'orangered',
  'orchid',
  'palegoldenrod',
  'palegreen',
  'paleturquoise',
  'palevioletred',
  'papayawhip',
  'peachpuff',
  'peru',
  'pink',
  'plum',
  'powderblue',
  'purple',
  'rebeccapurple',
  'red',
  'rosybrown',
  'royalblue',
  'saddlebrown',
  'salmon',
  'sandybrown',
  'seagreen',
  'seashell',
  'sienna',
  'silver',
  'skyblue',
  'slateblue',
  'slategray',
  'slategrey',
  'snow',
  'springgreen',
  'steelblue',
  'tan',
  'teal',
  'thistle',
  'tomato',
  'turquoise',
  'violet',
  'wheat',
  'white',
  'whitesmoke',
  'yellow',
  'yellowgreen',
];

const NAMED_COLOR_PATTERN = new RegExp(`\\b(?:${NAMED_COLORS.join('|')})\\b`, 'i');

/** Properties that take a color but never match {@link COLOR_PROPERTY}. */
const COLOR_TAKING_PROPERTY = /^(?:border|outline|box-shadow|text-shadow|background)$/;

/**
 * The color literal hidden in a declaration value, or `null` when the value is
 * theme-driven.
 *
 * `var()` **property names** are blanked out first — `--vscode-charts-orange`
 * contains the keyword `orange`, and VS Code ships exactly six such variables
 * (charts.blue/green/orange/purple/red/yellow), which were false positives before
 * (review r1/r2 [S-R2]). Only the name is removed: a literal *fallback*
 * (`var(--x, red)` / `var(--x, #ff0000)`) must still be rejected.
 */
function colorLiteralIn(value: string): string | null {
  const withoutVarNames = value.replace(/var\(\s*--[\w-]+/g, 'var(');
  const literal = LITERAL_COLOR.exec(withoutVarNames);
  if (literal !== null) {
    return literal[0];
  }
  const named = NAMED_COLOR_PATTERN.exec(withoutVarNames);
  return named === null ? null : named[0];
}

function checkCssColors(): CssViolation[] {
  const violations: CssViolation[] = [];
  // preview/ is excluded on purpose: `preview/preview-theme.css` *is* the emulated
  // editor theme (there is no VS Code to inject the variables there).
  for (const file of listFiles(SRC, ['.css'])) {
    const lines = readFileSync(file, 'utf8').split('\n');
    lines.forEach((line, index) => {
      const trimmed = line.trimStart();
      if (trimmed.startsWith('*') || trimmed.startsWith('/*')) {
        return;
      }
      const colon = line.indexOf(':');
      if (colon < 0) {
        return;
      }
      const property = line.slice(0, colon).trim();
      const value = line.slice(colon + 1).trim();
      const isColorProperty =
        COLOR_PROPERTY.test(property) || (property.startsWith('--') && !isLengthOnly(value));
      if (!isColorProperty && !COLOR_TAKING_PROPERTY.test(property)) {
        return;
      }
      if (colorLiteralIn(value) !== null) {
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

/**
 * True for a custom property whose value cannot be a color at all (plain lengths):
 * `--wing-radius: 4px` must not be dragged into the color check.
 */
function isLengthOnly(value: string): boolean {
  return /^[\d.]+(?:px|em|rem|%|fr|vh|vw|ms|s|deg)?(?:\s+[\d.]+(?:px|em|rem|%|fr|vh|vw|ms|s|deg)?)*$/.test(
    value,
  );
}

/** Class names declared by a CSS module (selector `.foo` tokens). */
function cssModuleClasses(file: string): Set<string> {
  const source = readFileSync(file, 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');
  const classes = new Set<string>();
  // Everything between the previous `}`/start and a `{` is a selector list; inside
  // at-rules the inner selectors are matched by the next iteration.
  for (const match of source.matchAll(/([^{}]+)\{/g)) {
    const selector = match[1] ?? '';
    for (const classMatch of selector.matchAll(/\.([A-Za-z_][\w-]*)/g)) {
      const name = classMatch[1];
      if (name !== undefined) {
        classes.add(name);
      }
    }
  }
  return classes;
}

interface CssModuleUsage {
  readonly importer: string;
  readonly specifier: string;
  readonly cssFile: string | null;
  readonly names: readonly string[];
}

/**
 * Every `styles.X` / `styles['X']` read of a CSS-module import in one file.
 *
 * `vite/client` types CSS modules as `Record<string, string>`, so a typo yields
 * `undefined` at runtime and passes typecheck — this is the only thing that
 * catches it (see review r1: four dangling `system-*` class names).
 */
function collectCssModuleUsages(file: string): CssModuleUsage[] {
  const source = ts.createSourceFile(
    file,
    readFileSync(file, 'utf8'),
    ts.ScriptTarget.Latest,
    /* setParentNodes */ true,
    file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS,
  );

  /** local binding name → module specifier */
  const bindings = new Map<string, string>();
  const collectBindings = (node: ts.Node): void => {
    if (
      ts.isImportDeclaration(node) &&
      node.importClause !== undefined &&
      ts.isStringLiteral(node.moduleSpecifier) &&
      node.moduleSpecifier.text.endsWith('.module.css')
    ) {
      const specifier = node.moduleSpecifier.text;
      const { name, namedBindings } = node.importClause;
      if (name !== undefined) {
        bindings.set(name.text, specifier);
      }
      if (namedBindings !== undefined && ts.isNamespaceImport(namedBindings)) {
        bindings.set(namedBindings.name.text, specifier);
      }
    }
    ts.forEachChild(node, collectBindings);
  };
  collectBindings(source);
  if (bindings.size === 0) {
    return [];
  }

  const usages = new Map<string, Set<string>>();
  const addName = (specifier: string, name: string): void => {
    const bucket = usages.get(specifier) ?? new Set<string>();
    bucket.add(name);
    usages.set(specifier, bucket);
  };
  const collectUsages = (node: ts.Node): void => {
    if (
      ts.isPropertyAccessExpression(node) &&
      ts.isIdentifier(node.expression) &&
      bindings.has(node.expression.text)
    ) {
      const specifier = bindings.get(node.expression.text);
      if (specifier !== undefined) {
        addName(specifier, node.name.text);
      }
    } else if (
      ts.isElementAccessExpression(node) &&
      ts.isIdentifier(node.expression) &&
      bindings.has(node.expression.text)
    ) {
      const argument = node.argumentExpression;
      const specifier = bindings.get(node.expression.text);
      if (specifier !== undefined && argument !== undefined && ts.isStringLiteral(argument)) {
        addName(specifier, argument.text);
      }
    }
    ts.forEachChild(node, collectUsages);
  };
  collectUsages(source);

  return [...usages.entries()].map(([specifier, names]) => ({
    importer: file,
    specifier,
    cssFile: resolveRelative(file, specifier),
    names: [...names].sort(),
  }));
}

describe('color literals', () => {
  it('accepts theme variables, including the six chart colors', () => {
    // `--vscode-charts-{blue,green,orange,purple,red,yellow}` are real VS Code
    // theme ids whose *names* contain a CSS color keyword — they must not be read
    // as literals (review r2 [S-R2]).
    const themeDriven = [
      'var(--vscode-charts-blue)',
      'var(--vscode-charts-green)',
      'var(--vscode-charts-orange)',
      'var(--vscode-charts-purple)',
      'var(--vscode-charts-red)',
      'var(--vscode-charts-yellow)',
      'var(--vscode-panel-border, transparent)',
      'var(--vscode-editorWarning-foreground, var(--vscode-foreground))',
      'currentColor',
    ];
    for (const value of themeDriven) {
      expect({ value, literal: colorLiteralIn(value) }).toEqual({ value, literal: null });
    }
  });

  it('still rejects literals, including literals used as a var() fallback', () => {
    const literals = [
      '#ff0000',
      '#fff',
      'rgb(1, 2, 3)',
      'hsl(1deg 2% 3%)',
      'color-mix(in srgb, red, blue)',
      'var(--wing-accent, #ff0000)',
      'var(--wing-accent, red)',
      '1px solid white',
      'var(--vscode-editorWarning-foreground, white)',
    ];
    for (const value of literals) {
      expect({ value, rejected: colorLiteralIn(value) !== null }).toEqual({ value, rejected: true });
    }
  });
});

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

  it('every CSS-module class read in src/ exists in its stylesheet', () => {
    const missing: string[] = [];
    for (const file of [...listFiles(SRC, ['.ts', '.tsx'])]) {
      for (const usage of collectCssModuleUsages(file)) {
        const importer = path.relative(PACKAGE_ROOT, usage.importer);
        if (usage.cssFile === null) {
          missing.push(`${importer}: "${usage.specifier}" does not resolve to a stylesheet`);
          continue;
        }
        const declared = cssModuleClasses(usage.cssFile);
        for (const name of usage.names) {
          if (!declared.has(name)) {
            missing.push(
              `${importer}: styles.${name} is not declared in ${path.relative(PACKAGE_ROOT, usage.cssFile)}`,
            );
          }
        }
      }
    }
    expect(missing).toEqual([]);
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
