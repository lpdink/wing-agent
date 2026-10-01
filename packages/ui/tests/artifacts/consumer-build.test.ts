import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import react from '@vitejs/plugin-react';
import { build } from 'vite';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';

/**
 * Build-artifact gate for the two defects step 08 found in the *emitted* bundle
 * (both invisible to typecheck, lint and every source-level test):
 *
 * 1. **the token sheet was tree-shaken away.** `tokens.css` used to be imported only
 *    by `src/mount.tsx`; a consumer that takes components from the barrel never
 *    loads that module, so the bundler dropped it — and every `var(--wing-*)` in the
 *    renderer resolved to nothing (zeroed paddings, borderless cards). It only
 *    showed in a *production* build: `vite dev` does no tree-shaking and the VS Code
 *    webview mounts `mountApp` (which used to be the importer).
 * 2. **the dark-theme code-token rule was hashed into the void.** `markdown.module.css`
 *    wrote `body.vscode-dark .codeToken`; CSS Modules hashed *both* class names
 *    (`body._vscode-dark_mf69w_276 ._codeToken_mf69w_267`), so the rule never matched
 *    in any host — the VS Code webview included.
 *
 * Both are properties of the artifact, so this test builds the artifact — with the
 * repository's own Vite (the bundler every shell uses) and the two consumer shapes
 * that matter: a component-only shell (web/Electron) and the mount seam (webview).
 * Then it asserts the emitted CSS, following the *emitted* class names rather than
 * the source ones, so a "fixed" selector that stopped matching the hashed class
 * cannot pass.
 *
 * The negative control is documented in `task.md` and was run by hand: pointing the
 * token import back at `mount.tsx` turns the token assertions red, and restoring the
 * old dark selector turns the selector assertions red.
 */

const PACKAGE_ROOT = fileURLToPath(new URL('../..', import.meta.url));
const FIXTURES = path.join(PACKAGE_ROOT, 'tests/artifacts/fixtures');

interface BuiltConsumer {
  /** Concatenated emitted stylesheets. */
  readonly css: string;
  /** Concatenated emitted modules (the CSS-module class maps live here). */
  readonly js: string;
}

let outDir: string;
let transcript: BuiltConsumer;
let mount: BuiltConsumer;

/** Build one fixture through Vite, the way a shell would, into a temp directory. */
async function buildConsumer(entry: string, directory: string): Promise<BuiltConsumer> {
  await build({
    // No config file: the fixture must exercise the package as a *consumer* sees it
    // (module graph + `sideEffects`), not this repository's preview config.
    configFile: false,
    root: PACKAGE_ROOT,
    logLevel: 'silent',
    plugins: [react()],
    build: {
      outDir: directory,
      emptyOutDir: true,
      lib: { entry, formats: ['es'], fileName: 'consumer' },
      // The package ships CSS two ways (global sheets + CSS modules); a lib build
      // keeps them in one stylesheet, which is what a shell loads.
      cssCodeSplit: false,
    },
  });

  const files = readdirSync(directory, { withFileTypes: true, recursive: true })
    .filter((entry) => entry.isFile())
    .map((entry) => path.join(entry.parentPath, entry.name));
  const read = (...extensions: readonly string[]): string =>
    files
      .filter((file) => extensions.some((extension) => file.endsWith(extension)))
      .map((file) => readFileSync(file, 'utf8'))
      .join('\n');
  return { css: read('.css'), js: read('.js', '.mjs', '.cjs') };
}

beforeAll(async () => {
  outDir = mkdtempSync(path.join(tmpdir(), 'wing-ui-consumer-'));
  // Sequentially: two builds racing inside one Vite process add nothing but noise.
  transcript = await buildConsumer(
    path.join(FIXTURES, 'consumer-transcript.ts'),
    path.join(outDir, 'transcript'),
  );
  mount = await buildConsumer(path.join(FIXTURES, 'consumer-mount.ts'), path.join(outDir, 'mount'));
}, 240_000);

afterAll(() => {
  rmSync(outDir, { recursive: true, force: true });
});

/**
 * The hashed class name a CSS module maps a local name to, read out of the built
 * JavaScript — the same value the runtime puts on the element.
 */
function emittedClass(js: string, local: string): string {
  const match = new RegExp(`\\b${local}\\s*:\\s*"([^"]+)"`).exec(js);
  if (match === null || match[1] === undefined) {
    throw new Error(`the built bundle never maps the CSS-module class "${local}"`);
  }
  return match[1];
}

/** One-line snippet around each occurrence, so failures stay readable. */
function occurrences(source: string, pattern: RegExp): string[] {
  const found: string[] = [];
  for (const match of source.matchAll(pattern)) {
    if (found.length >= 3) break;
    found.push(source.slice(Math.max(0, match.index - 40), match.index + 80).replaceAll('\n', ' '));
  }
  return found;
}

describe('the token sheet survives a component-only consumer build', () => {
  it('emits the --wing-* definitions', () => {
    // Definitions, not uses: a build that kept `var(--wing-…)` but dropped the
    // sheet is exactly the defect — it renders as "no value" at runtime.
    expect(occurrences(transcript.css, /--wing-chat-row-padding-y:\s*5px/g)).not.toEqual([]);
    expect(occurrences(transcript.css, /--wing-md-line-height:/g)).not.toEqual([]);
    expect(transcript.css).toContain('--wing-fg-description:');
  });

  it('keeps the rules that consume them', () => {
    // The second half of the same property: the sheet alone proves nothing if the
    // renderer's own rules were dropped.
    expect(transcript.css).toContain('var(--wing-chat-row-padding-y)');
    expect(transcript.css).toContain('var(--wing-md-line-height)');
  });

  it('keeps them for the mount seam too (the VS Code webview shape)', () => {
    expect(occurrences(mount.css, /--wing-chat-row-padding-y:\s*5px/g)).not.toEqual([]);
    expect(occurrences(mount.css, /var\(--wing-chat-row-padding-y\)/g)).not.toEqual([]);
  });
});

describe('the dark-theme token rule survives CSS Modules', () => {
  it('anchors on the literal body class and the emitted local class', () => {
    const codeToken = emittedClass(transcript.js, 'codeToken');
    // `:global(body.vscode-dark) .codeToken` must compile to a selector that pairs
    // the *hashed* local class with the un-hashed host class.
    expect(
      occurrences(transcript.css, new RegExp(`body\\.vscode-dark\\s+\\.${codeToken}\\s*[,{]`, 'g')),
    ).not.toEqual([]);
    expect(
      occurrences(transcript.css, new RegExp(`body\\.vscode-high-contrast\\s+\\.${codeToken}\\s*[,{]`, 'g')),
    ).not.toEqual([]);
  });

  it('never hashes the host body class', () => {
    // The defect shape: `body._vscode-dark_<hash>_<line>` — a class that exists in no
    // document. Both class names were hashed, so the rule silently never applied.
    expect(occurrences(transcript.css, /body\._[A-Za-z0-9-]*vscode-dark/g)).toEqual([]);
    expect(occurrences(transcript.css, /body\._[A-Za-z0-9-]*vscode-high-contrast/g)).toEqual([]);
  });

  it('keeps the dark declaration itself', () => {
    expect(transcript.css).toContain('var(--shiki-dark');
  });
});
