import { render } from '@testing-library/react';
import katex from 'katex';
import { describe, expect, it, vi } from 'vitest';

import { MarkdownText } from '../../src/webview/chat/Markdown';
import { renderMathHtml, resetMathCache } from '../../src/webview/chat/markdown/math';

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
