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
 * `main.css` and rewrites its `url(fonts/…)` references to the emitted
 * `dist/webview/assets/*` files, which the CSP already allows (`font-src
 * ${cspSource}`). `katex-swap.min.css` (`font-display: swap`) is the documented
 * alternative — not used here because the fonts ship with the extension itself
 * (same origin, no network), where the default `block` is invisible, while `swap`
 * would show formulas in a fallback face first.
 */

import katex from 'katex';
import 'katex/dist/katex.min.css';

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
