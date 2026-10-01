/**
 * KaTeX rendering for the transcript.
 *
 * `renderToString` returns an HTML *string*, so the renderer hands it to
 * `dangerouslySetInnerHTML` — the only place in this webview where markup does not
 * come from our own JSX. That is safe here for a precise reason: with `trust:
 * false` KaTeX's HTML extensions are unavailable, so TeX input can never produce
 * raw markup. Verified against katex 0.18.9: `\href` / `\url` /
 * `\htmlClass{…}{…}` / `\htmlData{…}{…}` / `\htmlId{…}{…}` / `\includegraphics`
 * all render as an error *string* (no `<a>`, no attribute, no tag), `\text{<img
 * …>}` comes out escaped, and `\color` is validated by KaTeX itself. `trust` is
 * passed explicitly (rather than relying on the default) so the posture is visible
 * at the call site; `tests/webview/math.test.tsx` pins it.
 *
 * The stylesheet rides along with the bundle: Vite inlines `katex.min.css` into
 * `main.css`, and — since its library build inlines *assets* as well — the fonts end
 * up inside that stylesheet as `data:font/…` URIs instead of as
 * `dist/webview/assets/*` files. That is exactly why the webview CSP allows `data:`
 * for fonts (`src/host/html.ts`, docs §8.2), and `tests/artifact/webviewBundle.test.ts`
 * pins the premise. Only the woff2 source of each font survives the build
 * (`tools/fonts.mts`): Chromium never fetches the woff/truetype copies.
 *
 * The input is bounded on purpose (see {@link MAX_MATH_CHARS} and `maxSize`): a
 * formula is rendered synchronously on the webview's main thread, and a model can
 * produce text of any length.
 */

import katex from 'katex';
import 'katex/dist/katex.min.css';

/**
 * Longest formula that goes to KaTeX, in characters.
 *
 * KaTeX layout is synchronous *and superlinear* in the input size (measured on this
 * machine, katex 0.18.9, plain text input: 1k → 3 ms, 4k → 4 ms, 8k → 9 ms,
 * 16k → 55 ms, 32k → 283 ms, 65k → 1.4 s, 100k → 1.8 s), and it runs inside a React
 * render — the whole sidebar is frozen for the duration. Anything longer degrades to
 * the literal source, the same path a KaTeX refusal takes (no content is lost, the
 * reader sees the TeX).
 *
 * 8192 is not arbitrary: it is the budget the TUI lane puts on one formula
 * (`crates/wing-math/src/guard.rs`, `MAX_SOURCE_CHARS`), and it keeps a single render
 * at ~10 ms. Real formulas are orders of magnitude smaller — this only catches
 * machine-generated dumps.
 */
export const MAX_MATH_CHARS = 8192;

/**
 * Cap for user-specified sizes (`\rule{…}`, `\hspace{…}` …), in ems.
 *
 * KaTeX's default is `Infinity`, so `\rule{100000em}{100000em}` produces a box that
 * big — and a box that big is a layout problem for the transcript. 10em is well past
 * anything a real formula uses (KaTeX's own documentation recommends a finite value
 * for untrusted input).
 */
const MAX_SIZE_EM = 10;

/**
 * One formula, keyed by `display` + TeX.
 *
 * KaTeX layout is pure (same input → same HTML) and costs ~0.1–1 ms per formula,
 * but a transcript re-renders it more than once: the same formula can appear in
 * several cells, and switching tabs (or a `resync` hydrate) rebuilds the cells.
 * The cache is a plain FIFO — a formula that is still being streamed evicts
 * itself — and bounded so a long session cannot grow it without limit.
 */
const CACHE_LIMIT = 200;

/** `null` records "KaTeX refused this" so a bad formula is only parsed once. */
const cache = new Map<string, string | null>();

/** KaTeX HTML for one formula, or `null` when KaTeX refuses it (caller renders `source`). */
export function renderMathHtml(tex: string, display: boolean): string | null {
  // Before the cache: an over-long formula is never rendered *and* never becomes a
  // cache key (the key is the full source).
  if (tex.length > MAX_MATH_CHARS) {
    return null;
  }

  const key = `${display ? 'D' : 'I'}\u0000${tex}`;
  const cached = cache.get(key);
  if (cached !== undefined) {
    return cached;
  }

  const html = renderUncached(tex, display);
  if (cache.size >= CACHE_LIMIT) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) {
      cache.delete(oldest);
    }
  }
  cache.set(key, html);
  return html;
}

/** Test hook: the cache holds no user-visible state, only parses. */
export function resetMathCache(): void {
  cache.clear();
}

function renderUncached(tex: string, display: boolean): string | null {
  try {
    return katex.renderToString(tex, {
      displayMode: display,
      // We render our own fallback (the literal source), never KaTeX's red error text.
      throwOnError: true,
      // Model-written LaTeX is routinely non-standard; `warn` would only spam the
      // webview console. Strictness is not a trust boundary — `trust` is.
      strict: 'ignore',
      // Explicitly off: no `\href`, no `\htmlClass`, no `\includegraphics`.
      trust: false,
      // Bound the *result* as well as the input: user-specified sizes are capped, so
      // a `\rule` cannot push a 100000em box into the transcript.
      maxSize: MAX_SIZE_EM,
    });
  } catch (error) {
    // Unsupported commands / unbalanced braces are normal model output: the caller
    // shows the original text instead. Anything else means KaTeX itself failed,
    // which is worth exactly one console line per unique formula.
    if (!(error instanceof katex.ParseError)) {
      console.warn('[wing] unexpected KaTeX failure', error);
    }
    return null;
  }
}
