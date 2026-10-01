import { readdirSync, readFileSync, existsSync, statSync } from 'node:fs';
import { builtinModules } from 'node:module';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import ts from 'typescript';
import { describe, expect, it } from 'vitest';

/**
 * Import-graph guard for `@wing-agent/ui` — the authoritative gate for the package's
 * environment promise (mechanism #3 of three; see `eslint.config.mjs` and the three
 * tsconfigs for the other two).
 *
 * It parses every source file, resolves every static import / re-export / dynamic
 * `import()` / `require()`, and asserts:
 *
 * - nothing leaves the package (a relative import that escapes `src/` means a
 *   dependency on the consumer's layout — the exact thing this package exists to
 *   avoid);
 * - no node builtin, no `vscode`, and no npm import outside the documented allowlist
 *   (the libraries the renderer is built from);
 * - the workspace dependency is reached through its **barrel** only;
 * - the fixtures (`src/testing/**`) are never imported by product code;
 * - every module is reachable through one of the three public barrels (a module a
 *   consumer can only reach by deep path is not part of the contract);
 *
 * and it keeps the **renderer's CSS invariants** that used to live in the
 * extension's own layer guard (`extensions/vscode/tests/layers/layers.test.ts`):
 * every colour comes from a theme variable, and every `styles.X` read exists in the
 * stylesheet it came from. Those checks moved with the stylesheets — they are
 * properties of the renderer, not of the VS Code shell around it.
 *
 * It runs in `pnpm test`, so it is part of `make test-ts` and CI along with every
 * other package's gate.
 */

const PACKAGE_ROOT = fileURLToPath(new URL('..', import.meta.url));
const SRC = path.join(PACKAGE_ROOT, 'src');

const NODE_BUILTINS = new Set([...builtinModules, ...builtinModules.map((name) => `node:${name}`)]);

/**
 * The one workspace package this one may import (a barrel import; a deep path would
 * make that package's file layout a contract here).
 */
const SESSION_PACKAGE = '@wing-agent/session';

/**
 * The npm modules `src/` may import — the libraries the renderer is built from.
 * `prefix` entries cover the deep specifiers a couple of them legitimately use
 * (markdown-it's token/state classes, shiki's grammars and themes, which are
 * *data* modules bundled at build time).
 */
const NPM_ALLOWLIST: readonly string[] = [
  'anser',
  'clsx',
  'react',
  'react-dom/client',
  'zustand',
  'zustand/vanilla',
  'katex',
  'markdown-it',
];
const NPM_PREFIX_ALLOWLIST: readonly string[] = ['katex', 'markdown-it', 'shiki'];

/** `src/` is TypeScript + CSS: no `.js`, no emitted artifacts. */
const ALLOWED_SRC_EXTENSIONS = ['.ts', '.tsx', '.css', '.md'];

/**
 * The three public entries and the barrel file each one is served by. Every module
 * under `src/` must be reachable through exactly one of them: a module nothing
 * re-exports can only be reached by a deep path, which the ESLint zones and the
 * extension's layer guard both refuse — so it would be dead weight pretending to be
 * API.
 */
const ENTRIES: readonly { readonly barrel: string; readonly subtree: string | null }[] = [
  { barrel: 'index.ts', subtree: null },
  { barrel: 'protocol/index.ts', subtree: 'protocol' },
  { barrel: 'testing/index.ts', subtree: 'testing' },
];

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

describe('packaging', () => {
  const sourceFiles = listFiles(SRC, ['.ts', '.tsx']);

  it('finds source files to check', () => {
    expect(sourceFiles.length).toBeGreaterThan(20);
  });

  it('keeps src/ TypeScript-only', () => {
    const foreign = listFiles(SRC)
      .filter((file) => !ALLOWED_SRC_EXTENSIONS.some((extension) => file.endsWith(extension)))
      .map(
        (file) =>
          `${path.relative(PACKAGE_ROOT, file)} — src/ is TypeScript + CSS only (allowed: ${ALLOWED_SRC_EXTENSIONS.join(', ')})`,
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

  it('has no dependency outside the declared allowlist (and no node builtin, no vscode)', () => {
    const violations: string[] = [];
    const allowed = (value: string): boolean =>
      NPM_ALLOWLIST.includes(value) || NPM_PREFIX_ALLOWLIST.some((prefix) => value.startsWith(`${prefix}/`));
    for (const file of sourceFiles) {
      for (const { value } of collectModuleSpecifiers(file)) {
        if (value.startsWith('.')) {
          continue;
        }
        const importer = path.relative(PACKAGE_ROOT, file);
        if (NODE_BUILTINS.has(value)) {
          violations.push(`${importer}: "${value}" is a node builtin — unavailable in a browser document`);
        } else if (value === 'vscode' || value.startsWith('vscode/')) {
          violations.push(
            `${importer}: "${value}" — the renderer is embedded by many hosts, not just VS Code`,
          );
        } else if (value === SESSION_PACKAGE) {
          continue; // the documented workspace dependency
        } else if (value.startsWith(`${SESSION_PACKAGE}/`)) {
          violations.push(
            `${importer}: "${value}" — import "${SESSION_PACKAGE}" through its barrel only; a deep path makes its file layout a contract`,
          );
        } else if (allowed(value)) {
          continue;
        } else {
          violations.push(
            `${importer}: "${value}" — not in the package's dependency allowlist ` +
              `(${[...NPM_ALLOWLIST, ...NPM_PREFIX_ALLOWLIST.map((prefix) => `${prefix}/…`)].join(', ')})`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('keeps the test doubles out of product code', () => {
    // Mirrors the rule the extension's layer guard applies to src/testing: fixtures
    // and the scripted host are for tests and harnesses, even inside this package.
    const violations: string[] = [];
    for (const file of sourceFiles) {
      if (path.relative(SRC, file).startsWith(`testing${path.sep}`)) {
        continue;
      }
      for (const { value } of collectModuleSpecifiers(file)) {
        if (!value.startsWith('.')) {
          continue;
        }
        const resolved = resolveRelative(file, value);
        if (resolved !== null && path.relative(SRC, resolved).startsWith(`testing${path.sep}`)) {
          violations.push(
            `${path.relative(PACKAGE_ROOT, file)}: "${value}" imports the fixtures (src/testing)`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });

  it('exports every module through one of the three barrels (the public entries)', () => {
    // A module that nothing re-exports is dead weight for the consumer: it can only
    // be reached by a deep path, which the extension's layer guard and the ESLint
    // zones both refuse.
    const allModules = listFiles(SRC, ['.ts', '.tsx'])
      .map((file) => path.relative(SRC, file).replace(/\.tsx?$/, ''))
      .filter((name) => !name.endsWith('.d'))
      .filter((name) => name !== 'index' && !name.endsWith('/index'));

    const inSubtree = (name: string, subtree: string): boolean =>
      name === subtree || name.startsWith(`${subtree}${path.sep}`);

    const violations: string[] = [];
    for (const { barrel, subtree } of ENTRIES) {
      const barrelSource = readFileSync(path.join(SRC, barrel), 'utf8');
      const modules = allModules.filter((name) =>
        subtree === null
          ? !inSubtree(name, 'protocol') && !inSubtree(name, 'testing')
          : inSubtree(name, subtree),
      );
      const barrelDir = path.dirname(barrel);
      for (const name of modules) {
        // The specifier as the barrel would write it: relative to the barrel itself.
        const specifier = `./${path.relative(barrelDir, name)}`;
        if (!barrelSource.includes(`from '${specifier}';`)) {
          violations.push(
            `${barrel} does not re-export ${specifier} — reach it through a barrel or delete it`,
          );
        }
      }
    }
    expect(violations).toEqual([]);
  });
});

// ── Renderer CSS invariants ───────────────────────────────────────────────
//
// Moved here from the extension's layer guard together with the stylesheets: the
// rules describe the renderer (its colours must come from the host's theme
// variables; a class it reads must exist), not the VS Code shell.

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
 * (charts.blue/green/orange/purple/red/yellow), which were false positives before.
 * Only the name is removed: a literal *fallback* (`var(--x, red)` / `var(--x, #ff0000)`)
 * must still be rejected.
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

/**
 * Sheets that *define* the colour vocabulary rather than consume it: the theme
 * table (`design-platform.css`) and the shiki palette (`shiki.css`) are where the
 * literals legitimately live — every other stylesheet must read them through
 * `var()`. The port batch 06b added them beside the renderer's own sheets, so the
 * rule moved with them.
 */
const COLOR_SOURCE_SHEETS = ['styles/design-platform.css', 'styles/shiki.css'];

function checkCssColors(): CssViolation[] {
  const violations: CssViolation[] = [];
  for (const file of listFiles(SRC, ['.css'])) {
    if (COLOR_SOURCE_SHEETS.includes(path.relative(SRC, file).split(path.sep).join('/'))) {
      continue;
    }
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
 * `undefined` at runtime and passes typecheck — this is the only thing that catches
 * it (see the extension's review r1: four dangling `system-*` class names).
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
    // as literals.
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

describe('renderer CSS', () => {
  it('keeps every color on a theme variable', () => {
    expect(
      checkCssColors().map((violation) => `${violation.file}:${violation.line} ${violation.declaration}`),
    ).toEqual([]);
  });

  it('every CSS-module class read in src/ exists in its stylesheet', () => {
    const missing: string[] = [];
    for (const file of listFiles(SRC, ['.ts', '.tsx'])) {
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
});
