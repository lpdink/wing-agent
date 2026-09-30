import type { Plugin } from 'vite';

/**
 * Ship only the woff2 source of every `@font-face`.
 *
 * KaTeX declares three formats per font (woff2 + woff + truetype) and Vite's
 * *library* build inlines all of them as base64 (it inlines every asset, see
 * `vite.config.mts`). The webview is Chromium, which never fetches the woff or
 * truetype copies — they are ~1.0 MB of the shipped stylesheet. Dropping them
 * leaves the woff2 data URIs (still needed: the CSP's `font-src … data:` exists for
 * exactly that) and cuts `main.css` from 1.5 MB to 414 kB (measured; see `design.md` §Bundle).
 *
 * Runs as a `pre` transform on the stylesheet *source* (`url(fonts/…)` is still
 * unresolved at that point), so the bundler never processes the dead assets at all.
 *
 * Deliberately conservative: only `@font-face` blocks are touched, and only when at
 * least one of their sources is woff2 — anything unrecognized is returned
 * byte-for-byte. `tests/artifact/webviewBundle.test.ts` asserts the result on the
 * real build (one source per face, no ttf/woff payloads) and unit-tests this
 * function.
 */
export function keepWoff2Sources(css: string): string {
  return css.replace(FONT_FACE_BLOCK, (block) => {
    // The block always ends with `}` (that is what the pattern matches): keep it out
    // of the declaration scan, because `src:` is usually the *last* declaration and
    // then has no terminating `;`.
    const body = block.endsWith('}') ? block.slice(0, -1) : block;
    const closer = block.slice(body.length);

    const start = body.indexOf('src:');
    if (start === -1) {
      return block;
    }
    const valueStart = start + 'src:'.length;
    const valueEnd = declarationEnd(body, valueStart);
    const value = body.slice(valueStart, valueEnd);
    // Split on the commas *between* entries: a data URI has one of its own
    // (`data:font/woff2;base64,…`).
    const sources = splitSources(value);
    const kept = sources.filter((source) => WOFF2_SOURCE.test(source));
    if (kept.length === 0 || kept.length === sources.length) {
      return block;
    }
    return `${body.slice(0, valueStart)}${kept.join(',')}${body.slice(valueEnd)}${closer}`;
  });
}

/** Split a `src:` value into its `url(…) format(…)` entries. */
function splitSources(value: string): string[] {
  const sources: string[] = [];
  let depth = 0;
  let start = 0;
  for (let index = 0; index < value.length; index += 1) {
    const char = value.charAt(index);
    if (char === '(') {
      depth += 1;
    } else if (char === ')') {
      depth = Math.max(0, depth - 1);
    } else if (char === ',' && depth === 0) {
      sources.push(value.slice(start, index));
      start = index + 1;
    }
  }
  sources.push(value.slice(start));
  return sources;
}

/** Vite plugin applying {@link keepWoff2Sources} to the KaTeX stylesheet. */
export function woff2OnlyFonts(): Plugin {
  return {
    name: 'wing-woff2-only-fonts',
    enforce: 'pre',
    transform(code, id) {
      const file = id.split('?')[0] ?? id;
      if (!KATEX_CSS.test(file)) {
        return null;
      }
      return { code: keepWoff2Sources(code), map: null };
    },
  };
}

/** One `@font-face` block — base64 data URIs contain neither `}` nor `{`. */
const FONT_FACE_BLOCK = /@font-face[^{}]*\{[^}]*\}/g;

/** A `src:` entry that names woff2 (KaTeX writes `format("woff2")`). */
const WOFF2_SOURCE = /format\(\s*['"]woff2['"]\s*\)/;

/** The KaTeX stylesheet, by path (ids may carry a query). */
const KATEX_CSS = /[/\\]katex[/\\]dist[/\\]katex(\.min)?\.css$/;

/**
 * End of the declaration that starts at `from`: the first `;` that is not inside
 * parentheses (a data URI contains `;base64,`).
 */
function declarationEnd(css: string, from: number): number {
  let depth = 0;
  for (let index = from; index < css.length; index += 1) {
    const char = css.charAt(index);
    if (char === '(') {
      depth += 1;
    } else if (char === ')') {
      depth = Math.max(0, depth - 1);
    } else if (char === ';' && depth === 0) {
      return index;
    }
  }
  return css.length;
}
