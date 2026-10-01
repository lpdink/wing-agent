import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

/**
 * The theme bridge's preconditions.
 *
 * `apps/web/src/ui-theme.css` adapts the shared renderer's `--vscode-*` variables to
 * this app's tokens, and repairs one selector the package cannot match any more (see
 * the file's long comment). Both are *agreements with another package's source*, so
 * they are pinned here instead of trusted: if the renderer stops reading the
 * variables (or stops writing shiki's two inline custom properties), this test goes
 * red instead of the transcript quietly losing its colours.
 *
 * Node-side test (file system), so it lives in `tsconfig.tools.json`'s program —
 * together with the import-graph guard.
 */

const UI_STYLES = fileURLToPath(new URL('../../../packages/ui/src/styles/', import.meta.url));
const UI_RENDER = fileURLToPath(
  new URL('../../../packages/ui/src/chat/markdown/render.tsx', import.meta.url),
);
const UI_MOUNT = fileURLToPath(new URL('../../../packages/ui/src/mount.tsx', import.meta.url));
const APP_THEME = fileURLToPath(new URL('../src/ui-theme.css', import.meta.url));
const APP_VITE_CONFIG = fileURLToPath(new URL('../vite.config.mts', import.meta.url));

/** The stylesheets the transcript pulls in (`chat` / `markdown` + the token layer). */
const TRANSCRIPT_STYLESHEETS = ['chat.module.css', 'markdown.module.css', 'tokens.css'] as const;

function read(path: string): string {
  return readFileSync(path, 'utf8');
}

/**
 * Every `var(--vscode-X)` that has **no** fallback in `text`.
 *
 * Those are the ones that must exist: a missing variable would make the declaration
 * invalid at computed-value time (the property falls back to `inherit`/`transparent`
 * — i.e. a silently unstyled row), while `var(--vscode-X, fallback)` is fine without
 * the bridge.
 */
function requiredVariables(text: string): Set<string> {
  const required = new Set<string>();
  for (const match of text.matchAll(/var\((--vscode-[\w-]+)\)/g)) {
    const name = match[1];
    if (name !== undefined) {
      required.add(name);
    }
  }
  return required;
}

describe('web theme bridge', () => {
  const theme = read(APP_THEME);

  it('defines every --vscode-* variable the transcript stylesheets require', () => {
    const missing: string[] = [];
    for (const stylesheet of TRANSCRIPT_STYLESHEETS) {
      for (const name of requiredVariables(read(`${UI_STYLES}${stylesheet}`))) {
        if (!theme.includes(`${name}:`)) {
          missing.push(`${stylesheet}: ${name}`);
        }
      }
    }
    expect(missing).toEqual([]);
  });

  it('repairs the dark code-token switch the CSS-module transform broke', () => {
    // The package's rule (still there, still unmatchable: both class names are hashed).
    expect(read(`${UI_STYLES}markdown.module.css`)).toContain('body.vscode-dark .codeToken');
    // …and the package still emits both variants per token, which is what the repair
    // keys on (it must not depend on the mangled class name).
    const render = read(UI_RENDER);
    expect(render).toContain("'--shiki-light'");
    expect(render).toContain("'--shiki-dark'");
    // The repair itself, in this app's stylesheet.
    expect(theme).toMatch(/\[style\*='--shiki-dark'\]\[style\*='--shiki-light'\]/);
    expect(theme).toMatch(/color: var\(--shiki-dark/);
  });

  it('maps the themes the renderer reads onto this app’s tokens, for both schemes', () => {
    // The card surfaces and the text ramp: without these the transcript renders on
    // transparent/black in one of the two schemes.
    for (const name of [
      '--vscode-foreground',
      '--vscode-descriptionForeground',
      '--vscode-editor-background',
    ]) {
      expect(theme).toContain(`${name}:`);
    }
    expect(theme).toMatch(/@media \(prefers-color-scheme: dark\)/);
  });
});

describe('the renderer’s token layer', () => {
  const theme = read(APP_THEME);
  const config = read(APP_VITE_CONFIG);

  it('is loaded by the app through the documented alias', () => {
    // The package imports `styles/tokens.css` from `mount.tsx` only, and every other
    // module is side-effect-free — so a bundler that drops the unused `mountApp`
    // export drops the tokens with it, and every `var(--wing-*)` in the renderer
    // silently resolves to nothing (rows lose their padding and the ask card its
    // border) *in the production build only*. The app therefore imports the file
    // itself, through a Vite alias; both ends are pinned here.
    expect(read(UI_MOUNT)).toContain("import './styles/tokens.css'");
    expect(config).toContain("'wing-ui-tokens.css'");
    expect(config).toContain('packages/ui/src/styles/tokens.css');
    expect(theme).toContain("@import 'wing-ui-tokens.css'");
  });

  it('defines the variables the transcript stylesheets read', () => {
    // Tokens the renderer *uses* but the package never defines. `padding:
    // var(--wing-tool-card-title-padding)` on the tool card's title is one today:
    // it resolves to nothing in the VS Code webview too, i.e. it is a package-side
    // gap, not something this app can supply (inventing a value here would make the
    // web disagree with the editor). Listed, so a *new* undefined token still fails.
    const KNOWN_UNDEFINED = new Set(['--wing-tool-card-title-padding']);
    const tokens = read(`${UI_STYLES}tokens.css`);
    const used = new Set<string>();
    for (const stylesheet of TRANSCRIPT_STYLESHEETS) {
      for (const match of read(`${UI_STYLES}${stylesheet}`).matchAll(/var\((--wing-[\w-]+)\)/g)) {
        if (match[1] !== undefined) {
          used.add(match[1]);
        }
      }
    }
    expect(used.size).toBeGreaterThan(20);
    const missing = [...used].filter((name) => !KNOWN_UNDEFINED.has(name) && !tokens.includes(`${name}:`));
    expect(missing).toEqual([]);
  });
});
