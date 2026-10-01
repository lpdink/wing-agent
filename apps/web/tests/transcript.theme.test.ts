import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

/**
 * The theme wiring's preconditions (rewritten in step 08b).
 *
 * The app now stands on two token families at once, and both are *agreements with
 * another package's source*, so they are pinned here instead of trusted:
 *
 * - the renderer's own `--wing-*` layer, which the package's barrel loads (step 06c's
 *   fix — the alias this app carried while that fix was pending is gone), and the
 *   `--vscode-*` names the package's components read but never define, which
 *   `src/ui-theme.css` maps onto this app's tokens;
 * - the ported `--dsw-*` design tables, which `src/main.tsx` imports from the
 *   package's declared `./styles/*` seam, plus the `body[data-ds-dark-theme]` /
 *   `body.vscode-dark` attributes `src/theme/color-scheme.ts` publishes.
 *
 * The "every variable a rendered stylesheet requires has a definition" check is the
 * shape step 08 introduced, generalised: it walks the *stylesheets the swapped-in
 * cards actually pull in* plus this app's own `cells.css`, and requires a
 * definition wherever the `var()` has no fallback (a missing variable is invalid at
 * computed-value time — the property silently falls back to `inherit`, which is how
 * the token layer once vanished from production builds without failing anything).
 *
 * Node-side test (file system), so it lives in `tsconfig.tools.json`'s program —
 * together with the import-graph guard.
 */

const UI_SRC = fileURLToPath(new URL('../../../packages/ui/src/', import.meta.url));
const UI_STYLES = `${UI_SRC}styles/`;
const APP = fileURLToPath(new URL('../src/', import.meta.url));

function read(path: string): string {
  return readFileSync(path, 'utf8');
}

/**
 * The five theme sheets, in the order `src/main.tsx` imports them (the cascade
 * matters: `design-platform.css` defines the `--dsw-*` table, the others alias it).
 */
const TOKEN_SHEETS = ['design-platform.css', 'base.css', 'scrollbar.css', 'focus.css', 'shiki.css'] as const;

/** The stylesheets the swapped-in cards pull in across the package. */
const CARD_STYLESHEETS = [
  'markdown/CodeBlock.module.css',
  'markdown/CodeCard.module.css',
  'tool/TerminalBlock.module.css',
  'tool/DiffBlock.module.css',
  'chat/DisclosureRow.module.css',
  'chat/TextShimmer.module.css',
  'chat/ReasoningRow.module.css',
  'chat/accessibility.module.css',
  'ask/ApprovalPanel.module.css',
  'ask/QuestionComposer.module.css',
  'ask/QuestionReplyView.module.css',
  'components/Button.module.css',
  'components/StateDot.module.css',
  'components/Pill.module.css',
  'components/ConnectionIndicator.module.css',
] as const;

/** The stylesheets the *unchanged* rows still render through. */
const TRANSCRIPT_STYLESHEETS = ['chat.module.css', 'markdown.module.css', 'tokens.css'] as const;

/** This app's own transcript stylesheet. */
const APP_CELLS = `${APP}transcript/cells/cells.css`;

/**
 * Every `var(--x)` in `text` that has **no** fallback — the ones that must exist,
 * since a missing variable makes the declaration invalid at computed-value time.
 */
function requiredVariables(text: string, prefix: string): Set<string> {
  const required = new Set<string>();
  for (const match of text.matchAll(new RegExp(`var\\((${prefix}[\\w-]+)\\)`, 'g'))) {
    const name = match[1];
    if (name !== undefined) {
      required.add(name);
    }
  }
  return required;
}

/** A CSS definition (`--name:`) anywhere in the file. */
function defines(text: string, name: string): boolean {
  return new RegExp(`${name}\\s*:`).test(text);
}

describe('the token tables (P0-1)', () => {
  const main = read(`${APP}main.tsx`);
  const barrel = read(`${UI_SRC}index.ts`);
  const theme = read(`${APP}ui-theme.css`);
  const viteConfig = read(fileURLToPath(new URL('../vite.config.mts', import.meta.url)));

  it('loads the package’s token sheets through the declared ./styles seam', () => {
    for (const sheet of TOKEN_SHEETS) {
      expect(main).toContain(`'@wing-agent/ui/styles/${sheet}'`);
    }
    // …and they exist on the package side (the import would fail the build anyway,
    // but the failure mode this guards is a rename that leaves a stale comment).
    for (const sheet of TOKEN_SHEETS) {
      expect(read(`${UI_STYLES}${sheet}`).length).toBeGreaterThan(0);
    }
  });

  it('gets the renderer’s --wing-* layer from the barrel, not from an alias', () => {
    // Step 06c's fix: the sheet is imported by the module every `.` consumer walks
    // through, so a component-only consumer build keeps it (its build-artifact test
    // in the package pins the emitted CSS).
    expect(barrel).toContain("import './styles/tokens.css'");
    // …which is why the alias this app carried (and the `@import` that used it) are
    // gone: reaching into the package for a stylesheet is no longer needed.
    expect(viteConfig).not.toContain('wing-ui-tokens.css');
    expect(viteConfig).not.toContain('packages/ui/src/styles/tokens.css');
    expect(theme).not.toContain('@import');
  });
});

describe('the dark switch', () => {
  const colorScheme = read(`${APP}theme/color-scheme.ts`);
  const main = read(`${APP}main.tsx`);

  it('publishes the two attributes the sheets switch on', () => {
    // The packages' preconditions: the design tables override on the body attribute,
    // the renderer's dark code colours on the body class (the selector step 06c
    // repaired — `:global()` keeps the host class unhashed).
    expect(read(`${UI_STYLES}design-platform.css`)).toContain('body[data-ds-dark-theme]');
    expect(read(`${UI_STYLES}shiki.css`)).toContain('body[data-ds-dark-theme]');
    expect(read(`${UI_STYLES}markdown.module.css`)).toContain(':global(body.vscode-dark) .codeToken');
    // …and this app is the second host that publishes both.
    expect(colorScheme).toContain("toggleAttribute('data-ds-dark-theme', dark)");
    expect(colorScheme).toContain("classList.toggle('vscode-dark', dark)");
  });

  it('runs the switch before React mounts', () => {
    const call = main.indexOf('watchColorScheme()');
    const mount = main.indexOf('createRoot(');
    expect(call).toBeGreaterThan(-1);
    expect(mount).toBeGreaterThan(-1);
    expect(call).toBeLessThan(mount);
  });
});

describe('the --vscode-* bridge', () => {
  const theme = read(`${APP}ui-theme.css`);

  it('defines every --vscode-* variable the transcript stylesheets require', () => {
    const missing: string[] = [];
    for (const stylesheet of TRANSCRIPT_STYLESHEETS) {
      for (const name of requiredVariables(read(`${UI_STYLES}${stylesheet}`), '--vscode-')) {
        if (!defines(theme, name)) {
          missing.push(`${stylesheet}: ${name}`);
        }
      }
    }
    expect(missing).toEqual([]);
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

  it('no longer carries the dark-shiki repair (the package fixed the selector)', () => {
    // The old workaround keyed on shiki's inline custom properties; with the package
    // matching `body.vscode-dark` again, the repair would be a silent second source.
    expect(theme).not.toContain('[style*=');
    expect(read(`${APP}theme/color-scheme.ts`)).toContain('vscode-dark');
  });
});

describe('the variables the swapped-in cards require', () => {
  const sheets = TOKEN_SHEETS.map((sheet) => read(`${UI_STYLES}${sheet}`)).join('\n');
  const cardSheets = CARD_STYLESHEETS.map((sheet) => read(`${UI_SRC}${sheet}`));

  it('has a definition for every --dsw-* / --dsh-* a card reads without a fallback', () => {
    const knownLocal = new Set([
      // Defined and consumed inside `QuestionComposer.module.css` itself (a layout
      // switch, not a design token).
      '--dsh-answer-field-padding',
    ]);
    const local = cardSheets.join('\n');
    const missing: string[] = [];
    for (const [index, text] of cardSheets.entries()) {
      for (const name of [...requiredVariables(text, '--dsw-'), ...requiredVariables(text, '--dsh-')]) {
        if (knownLocal.has(name) || defines(sheets, name) || defines(local, name)) {
          continue;
        }
        missing.push(`${CARD_STYLESHEETS[index]}: ${name}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it('defines the --win-* / --vscode-* names this app’s own cells.css reads', () => {
    // The row chrome and the caret are transcriptions of the package's rules (see
    // cells.css), so they read the same tokens — a renamed token must not leave this
    // app's stylesheet silently unstyled.
    const cells = read(APP_CELLS);
    const tokens = read(`${UI_STYLES}tokens.css`);
    const theme = read(`${APP}ui-theme.css`);
    const missing: string[] = [];
    for (const name of requiredVariables(cells, '--wing-')) {
      if (!defines(tokens, name)) {
        missing.push(`cells.css: ${name}`);
      }
    }
    for (const name of requiredVariables(cells, '--vscode-')) {
      if (!defines(theme, name)) {
        missing.push(`cells.css: ${name}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it('keeps the known-undefined --wing-* list honest for the stylesheets still mounted', () => {
    // Tokens the renderer *uses* but the package never defines; the list is empty
    // except for one entry, kept because the *stylesheet* still ships: the old tool
    // card's `padding: var(--wing-tool-card-title-padding)` (step 08b replaced that
    // card with this app's own, whose title carries the value sourced from
    // PARTS/chatConfirmationWidget.css — but `chat.module.css` also styles the cells
    // that were not swapped, so its rules are still in the bundle). A *new*
    // undefined token in the mounted sheets still fails this test.
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
    const missing = [...used].filter((name) => !KNOWN_UNDEFINED.has(name) && !defines(tokens, name));
    expect(missing).toEqual([]);
  });
});
