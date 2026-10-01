/**
 * Syntax highlighting for fenced code blocks (shiki).
 *
 * ## Why the JavaScript engine
 *
 * `createJavaScriptRegexEngine()` compiles Oniguruma patterns to JS `RegExp`
 * objects; the default (oniguruma/wasm) engine would need
 * `WebAssembly.compile`, which the webview's `script-src 'nonce-…'` CSP blocks
 * without `'wasm-unsafe-eval'` — a host-side (CSP) change we do not own. The JS
 * engine path contains no `new Function` / `eval` (verified in
 * `@shikijs/engine-javascript` + `oniguruma-to-es` + `oniguruma-parser`), so it
 * runs under the strict CSP we already have.
 *
 * `forgiving: true` degrades individual grammar patterns the JS engine cannot
 * express into "no colour" instead of throwing.
 *
 * ## Themes
 *
 * `light-plus` / `dark-plus` are VS Code's `Light+` / `Dark+` token colours —
 * which are exactly what `Light Modern` / `Dark Modern` inherit
 * (`extensions/theme-defaults/themes/dark_modern.json` has
 * `"include": "./dark_plus.json"`). Both themes are resident and the *renderer*
 * decides which one applies through CSS custom properties, so switching the VS
 * Code theme does not re-highlight anything.
 *
 * ## Cost control
 *
 * Languages are imported statically (no runtime fetch — the webview has no
 * network by design) and only the grammars we ship are bundled. The highlighter
 * is built lazily on the first code block; failure to build degrades to plain
 * text (`highlightCode` returns `null`) instead of breaking the transcript.
 */

import { createHighlighterCoreSync } from 'shiki/core';
import { createJavaScriptRegexEngine } from 'shiki/engine/javascript';
import bash from 'shiki/langs/bash.mjs';
import css from 'shiki/langs/css.mjs';
import diff from 'shiki/langs/diff.mjs';
import html from 'shiki/langs/html.mjs';
import javascript from 'shiki/langs/javascript.mjs';
import json from 'shiki/langs/json.mjs';
import python from 'shiki/langs/python.mjs';
import rust from 'shiki/langs/rust.mjs';
import sql from 'shiki/langs/sql.mjs';
import toml from 'shiki/langs/toml.mjs';
import tsx from 'shiki/langs/tsx.mjs';
import typescript from 'shiki/langs/typescript.mjs';
import yaml from 'shiki/langs/yaml.mjs';
import darkPlus from 'shiki/themes/dark-plus.mjs';
import lightPlus from 'shiki/themes/light-plus.mjs';

type Highlighter = ReturnType<typeof createHighlighterCoreSync>;
type TokenVariants = ReturnType<Highlighter['codeToTokensWithThemes']>;

/** One highlightable token, resolved to the two theme variants we ship. */
export interface HighlightToken {
  readonly content: string;
  readonly light: TokenStyle;
  readonly dark: TokenStyle;
}

export interface TokenStyle {
  readonly color: string | null;
  readonly italic: boolean;
  readonly bold: boolean;
  readonly underline: boolean;
}

/** Highlighted lines. Words never wrap inside a token, so lines are token arrays. */
export type HighlightedCode = readonly (readonly HighlightToken[])[];

/**
 * Theme *names* (must match the registrations above) …
 */
export const THEME_LIGHT = 'light-plus';
export const THEME_DARK = 'dark-plus';

/**
 * … and the variant *keys* they are requested under.
 *
 * `codeToTokensWithThemes` keys `token.variants` by the keys of its `themes`
 * option, not by theme name — the renderer reads `variants.light` / `variants.dark`.
 */
const VARIANT_LIGHT = 'light';
const VARIANT_DARK = 'dark';

let highlighter: Highlighter | null = null;
let unavailable = false;

function getHighlighter(): Highlighter | null {
  if (highlighter !== null || unavailable) {
    return highlighter;
  }
  try {
    highlighter = createHighlighterCoreSync({
      engine: createJavaScriptRegexEngine({ forgiving: true }),
      themes: [darkPlus, lightPlus],
      langs: [typescript, javascript, tsx, json, bash, python, rust, css, html, yaml, toml, sql, diff],
    });
  } catch (error) {
    // A broken grammar must not take the transcript down with it.
    unavailable = true;
    console.warn('[wing] syntax highlighting unavailable, falling back to plain text', error);
  }
  return highlighter;
}

/**
 * Highlight `code` as `lang`, or `null` when the language is unknown / the
 * highlighter is unavailable (callers render plain text then).
 *
 * Note the language is whatever the model wrote in the fence info string; shiki
 * resolves the usual aliases (`ts`, `py`, `sh`, …) from the grammars themselves.
 */
export function highlightCode(code: string, lang: string): HighlightedCode | null {
  if (lang === '' || code === '') {
    return null;
  }
  const instance = getHighlighter();
  if (instance === null) {
    return null;
  }
  let lines: TokenVariants;
  try {
    lines = instance.codeToTokensWithThemes(code, {
      lang,
      themes: { [VARIANT_LIGHT]: THEME_LIGHT, [VARIANT_DARK]: THEME_DARK },
    });
  } catch {
    // Unknown language, or a pattern the engine refuses: plain text is fine.
    return null;
  }
  return lines.map((line) =>
    line.map((token) => ({
      content: token.content,
      light: toStyle(token.variants[VARIANT_LIGHT]),
      dark: toStyle(token.variants[VARIANT_DARK]),
    })),
  );
}

/** shiki's `FontStyle` is a bit mask (1 = italic, 2 = bold, 4 = underline). */
function toStyle(style: { color?: string; fontStyle?: number } | undefined): TokenStyle {
  const mask = style?.fontStyle ?? 0;
  return {
    color: style?.color ?? null,
    italic: (mask & 1) !== 0,
    bold: (mask & 2) !== 0,
    underline: (mask & 4) !== 0,
  };
}
