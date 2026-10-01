import { render } from '@testing-library/react';
import katex from 'katex';
import { describe, expect, it, vi } from 'vitest';

import { MarkdownStream, MarkdownText } from '../../src/chat/Markdown';
import { MAX_MATH_CHARS, renderMathHtml, resetMathCache } from '../../src/chat/markdown/math';

/**
 * Formulas: the KaTeX boundary and the React wiring.
 *
 * The syntax (what becomes a formula) is asserted in `markdown.test.tsx`; this
 * file is about what happens *after* the AST says "formula".
 */

describe('renderMathHtml', () => {
  it('renders inline formulas without the display wrapper', () => {
    const html = renderMathHtml('\\frac{a}{b}', false);

    expect(html).not.toBeNull();
    expect(html).toContain('class="katex"');
    expect(html).toContain('katex-mathml');
    expect(html).not.toContain('katex-display');
  });

  it('renders display formulas with the display wrapper', () => {
    const html = renderMathHtml('\\begin{aligned} a &= b \\\\ c &= d \\end{aligned}', true);

    expect(html).toContain('katex-display');
    expect(html).toContain('columnalign');
  });

  it('returns null instead of throwing when KaTeX refuses the input', () => {
    // Unbalanced braces / unsupported commands — normal model output.
    expect(renderMathHtml('\\frac{1}{', false)).toBeNull();
    expect(renderMathHtml('\\notarealcommand{', false)).toBeNull();
    expect(renderMathHtml('a^{b', false)).toBeNull();
  });

  it('refuses formulas longer than the budget (they degrade to the source)', () => {
    // KaTeX layout is superlinear and synchronous (measured: 8k → 9 ms, 16k → 55 ms,
    // 32k → 283 ms, 100k → 1.8 s on the webview's main thread), so the render path
    // has an input bound. The value is the TUI lane's per-formula budget
    // (`crates/wing-math/src/guard.rs`); both lanes must agree on it.
    expect(MAX_MATH_CHARS).toBe(8192);
    expect(renderMathHtml('x'.repeat(MAX_MATH_CHARS), false)).not.toBeNull();
    expect(renderMathHtml('x'.repeat(MAX_MATH_CHARS + 1), false)).toBeNull();
    expect(renderMathHtml('x'.repeat(100_000), false)).toBeNull();
  });

  it('caps user-specified sizes so the result cannot blow up the layout', () => {
    const host = document.createElement('div');
    host.innerHTML = renderMathHtml('\\rule{100000em}{100000em}', false) ?? '';

    const styles = [...host.querySelectorAll('[style]')].map(
      (element) => element.getAttribute('style') ?? '',
    );
    expect(styles.length).toBeGreaterThan(0);
    expect(styles.filter((style) => style.includes('100000em'))).toEqual([]);
    expect(styles.some((style) => style.includes('10em'))).toBe(true);
  });

  it('memoizes one formula per (display, tex) pair', () => {
    resetMathCache();
    const spy = vi.spyOn(katex, 'renderToString');

    renderMathHtml('x^2 + y^2', false);
    renderMathHtml('x^2 + y^2', false);
    renderMathHtml('x^2 + y^2', true);

    expect(spy).toHaveBeenCalledTimes(2);
    spy.mockRestore();
  });

  it('cannot turn TeX into markup: the HTML extensions are off', () => {
    // `trust: false` (pinned in `math.ts`) is what makes `dangerouslySetInnerHTML`
    // safe for KaTeX output. Assert on the parsed DOM, not on the string: the raw
    // TeX legitimately appears *as text* inside KaTeX's MathML annotation, so only
    // the element tree can tell an injected attribute from an escaped one.
    const dom = (tex: string): HTMLElement => {
      const host = document.createElement('div');
      host.innerHTML = renderMathHtml(tex, false) ?? '';
      return host;
    };

    const href = dom('\\href{javascript:alert(1)}{x}');
    expect(href.querySelector('a')).toBeNull();
    expect(href.querySelector('[href]')).toBeNull();

    expect(dom('\\htmlClass{evil}{x}').querySelector('[class*="evil"]')).toBeNull();
    expect(dom('\\htmlStyle{color:red}{x}').querySelector('[style*="color:red"]')).toBeNull();
    expect(dom('\\htmlId{evil}{x}').querySelector('[id]')).toBeNull();
    expect(dom('\\url{javascript:alert(1)}').querySelector('[href]')).toBeNull();
    expect(dom('\\includegraphics{/etc/passwd}').querySelector('img')).toBeNull();

    const text = dom('\\text{<img src=x onerror=alert(1)>}');
    expect(text.querySelector('img')).toBeNull();
    expect(text.textContent).toContain('<img src=x onerror=alert(1)>');
  });
});

describe('MathView', () => {
  it('injects KaTeX output for an inline formula', () => {
    const { container } = render(<MarkdownText text={'Euler: $e^{i\\pi} + 1 = 0$ indeed'} />);

    const math = container.querySelector('[data-testid="md-math"]');
    expect(math).not.toBeNull();
    expect(math?.getAttribute('data-display')).toBe('false');
    expect(math?.querySelector('.katex')).not.toBeNull();
    // The surrounding text is untouched, and the formula is not a link.
    expect(container.textContent).toContain('Euler:');
    expect(container.textContent).toContain('indeed');
    expect(container.querySelector('script')).toBeNull();
  });

  it('renders a display formula as a block of its own', () => {
    const { container } = render(<MarkdownText text={'before\n\n$$\n\\int_0^1 x\\,dx\n$$\n'} />);

    const math = container.querySelector('[data-testid="md-math"]');
    expect(math?.getAttribute('data-display')).toBe('true');
    expect(math?.className).toContain('mathDisplay');
    expect(math?.querySelector('.katex-display')).not.toBeNull();
  });

  it('falls back to the literal source when KaTeX refuses the formula', () => {
    const { container } = render(<MarkdownText text={'broken $\\frac{1}{$ here'} />);

    // No KaTeX output, and — the point of the test — no missing text either.
    expect(container.querySelector('.katex')).toBeNull();
    expect(container.querySelector('[data-testid="md-math"]')).toBeNull();
    expect(container.querySelector('[data-testid="md-math-source"]')?.textContent).toBe('$\\frac{1}{$');
    expect(container.textContent).toContain('broken');
    expect(container.textContent).toContain('here');
  });

  it('renders a bare AMS environment as a display formula', () => {
    const { container } = render(<MarkdownText text={'\\begin{align}\na &= b\n\\end{align}\n'} />);

    const math = container.querySelector('[data-testid="md-math"]');
    expect(math?.getAttribute('data-display')).toBe('true');
    expect(math?.querySelector('.katex-display')).not.toBeNull();
  });

  it('falls back to the environment source when KaTeX refuses it', () => {
    // `\notacommand` is not a macro: the layout fails and the reader still sees
    // exactly what the model wrote — the TUI lane's degradation rule, verbatim.
    const source = '\\begin{align}\\notacommand{x}\\end{align}';
    const { container } = render(<MarkdownText text={source} />);

    expect(container.querySelector('.katex')).toBeNull();
    expect(container.querySelector('[data-testid="md-math-source"]')?.textContent).toBe(source);
  });

  it('shows an over-long `\\(…\\)` as ordinary text, with every character', () => {
    // Beyond the span budget the LaTeX delimiters are not recognized at all
    // (the TUI's normalizer refuses them too), so this is plain markdown text:
    // `\(` and `\)` go through markdown's own escaping — which is exactly what
    // the TUI prints for that input — and nothing is dropped.
    const filler = 'a'.repeat(10_000);
    const { container } = render(<MarkdownText text={`\\(x${filler}x\\)`} />);

    expect(container.querySelector('[data-testid="md-math"]')).toBeNull();
    expect(container.querySelector('[data-testid="md-math-source"]')).toBeNull();
    expect(container.textContent).toBe(`(x${filler}x)`);
  });

  it('turns an environment into a formula only once it is complete', () => {
    // The streaming path: while `\end{align}` is still missing the block is
    // text; when it arrives the very same block becomes a formula. No text is
    // ever dropped on the way (`md-math-source` is the same fallback).
    const { container, rerender } = render(<MarkdownStream text={'\\begin{align}\na &= b\n'} streaming />);
    expect(container.querySelector('[data-testid="md-math"]')).toBeNull();
    expect(container.textContent).toContain('a &= b');

    rerender(<MarkdownStream text={'\\begin{align}\na &= b\n\\end{align}\n'} streaming={false} />);
    // KaTeX rendered it (the raw TeX also appears inside its MathML annotation,
    // which is why this asserts on the elements, not on the text).
    expect(container.querySelector('[data-testid="md-math"]')?.querySelector('.katex')).not.toBeNull();
    expect(container.querySelector('[data-testid="md-math-source"]')).toBeNull();
  });

  it('cannot turn an environment into markup either (parsed DOM)', () => {
    // The new path (a bare environment) goes through the same `trust: false`
    // KaTeX entry point: assert on the parsed tree, not on the string.
    const dom = (text: string): HTMLElement => {
      const { container } = render(<MarkdownText text={text} />);
      return container;
    };

    const payload = '\\begin{align}\\text{<img src=x onerror=alert(1)>}\\end{align}';
    const text = dom(payload);
    expect(text.querySelector('img')).toBeNull();
    expect(text.textContent).toContain('<img src=x onerror=alert(1)>');

    const href = dom('\\begin{align}\\href{javascript:alert(1)}{x}\\end{align}');
    expect(href.querySelector('a')).toBeNull();
    expect(href.querySelector('[href]')).toBeNull();
  });

  it('renders a formula inside an HTML block as text, never through KaTeX', () => {
    // The line starts an HTML block: the TUI prints it verbatim and the webview
    // keeps markdown's escaping (`\(x\)` → `(x)`), but no formula is built —
    // the masked path never reaches KaTeX at all (review r1 [S1]).
    const { container } = render(<MarkdownText text={'<div>\n\\(x\\)\n</div>\n'} />);

    expect(container.querySelector('[data-testid="md-math"]')).toBeNull();
    expect(container.querySelector('.katex')).toBeNull();
    expect(container.textContent).toContain('<div>');
    expect(container.textContent).toContain('(x)');
  });

  it('shows an over-long formula as its source instead of laying it out', () => {
    // A machine-generated dump is not worth freezing the sidebar for (see
    // MAX_MATH_CHARS); it must still be *visible*, not dropped.
    const tex = 'x'.repeat(MAX_MATH_CHARS + 100);
    const { container } = render(<MarkdownText text={`$${tex}$`} />);

    expect(container.querySelector('.katex')).toBeNull();
    expect(container.querySelector('[data-testid="md-math-source"]')?.textContent).toBe(`$${tex}$`);
  });

  it('keeps `$` inside code spans and fences literal', () => {
    const { container } = render(<MarkdownText text={'`$x$`\n\n```ts\nconst price = "$1";\n```\n'} />);

    expect(container.querySelector('[data-testid="md-math"]')).toBeNull();
    expect(container.querySelector('code')?.textContent).toBe('$x$');
    expect(container.textContent).toContain('const price = "$1";');
  });

  it('leaves currency alone', () => {
    const { container } = render(<MarkdownText text={'costs $100 and $200 in total'} />);

    expect(container.querySelector('[data-testid="md-math"]')).toBeNull();
    expect(container.textContent).toBe('costs $100 and $200 in total');
  });
});
