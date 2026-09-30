import { describe, expect, it } from 'vitest';

import { keepWoff2Sources } from '../../tools/fonts.mts';

/**
 * The build-time font trim (`tools/fonts.mts`).
 *
 * This is a text transform over a stylesheet we do not own, so it is unit-tested
 * against the shapes KaTeX actually produces — before and after Vite inlines the
 * sources — and against the two ways it could go wrong: eating the closing brace of
 * a block whose `src:` is the last declaration, and running past a `;` inside a data
 * URI.
 */

/** One `@font-face` rule in KaTeX's source form (unresolved `url(fonts/…)`). */
const SOURCE_FORM =
  '@font-face{font-display:block;font-family:KaTeX_AMS;font-style:normal;font-weight:400;' +
  'src:url(fonts/KaTeX_AMS-Regular.woff2) format("woff2"),' +
  'url(fonts/KaTeX_AMS-Regular.woff) format("woff"),' +
  'url(fonts/KaTeX_AMS-Regular.ttf) format("truetype")}';

/** The same rule after Vite inlined every source (base64 payloads hold no comma). */
const INLINED_FORM =
  '@font-face{font-family:KaTeX_AMS;src:url(data:font/woff2;base64,AAAA) format("woff2"),' +
  'url(data:font/woff;base64,BBBB) format("woff"),' +
  'url(data:font/ttf;base64,CCCC) format("truetype")}';

/** Braces, for "the block is still a block" assertions. */
function braces(css: string): number {
  return css.split('{').length - css.split('}').length;
}

describe('keepWoff2Sources', () => {
  it('drops the woff and truetype sources, keeping the rest of the rule', () => {
    const trimmed = keepWoff2Sources(SOURCE_FORM);

    expect(trimmed).toContain('font-family:KaTeX_AMS');
    expect(trimmed).toContain('font-display:block');
    expect(trimmed).toContain('url(fonts/KaTeX_AMS-Regular.woff2) format("woff2")');
    // Precise needles: `…woff2` contains `…woff`, so the suffix matters.
    expect(trimmed).not.toContain('.woff)');
    expect(trimmed).not.toContain('.ttf)');
    expect(trimmed).not.toContain('truetype');
    // `src:` is the last declaration: the closing brace has to survive.
    expect(trimmed.endsWith('}')).toBe(true);
    expect(braces(trimmed)).toBe(0);
  });

  it('works on an already-inlined stylesheet (data URIs contain `;` and `,`)', () => {
    const trimmed = keepWoff2Sources(INLINED_FORM);

    expect(trimmed).toContain('data:font/woff2;base64,AAAA');
    // `data:font/woff2` starts with `data:font/woff`, so the `;` is the needle.
    expect(trimmed).not.toContain('data:font/woff;');
    expect(trimmed).not.toContain('data:font/ttf');
    expect(braces(trimmed)).toBe(0);
  });

  it('stops at the declaration boundary when another declaration follows `src:`', () => {
    const css = '@font-face{src:url(a.woff2) format("woff2"),url(a.ttf) format("truetype");font-weight:700}';

    expect(keepWoff2Sources(css)).toBe('@font-face{src:url(a.woff2) format("woff2");font-weight:700}');
  });

  it('leaves rules it does not recognize byte-for-byte alone', () => {
    const untouched = [
      '.katex{color:red}',
      '@font-face{font-family:X;src:url(a.ttf) format("truetype")}',
      '@font-face{font-family:X;font-weight:400}',
      '@media print{.katex{display:none}}',
      'body{margin:0}',
    ];
    for (const css of untouched) {
      expect({ css, out: keepWoff2Sources(css) }).toEqual({ css, out: css });
    }
  });

  it('is idempotent (running it twice changes nothing)', () => {
    const once = keepWoff2Sources(SOURCE_FORM);
    expect(keepWoff2Sources(once)).toBe(once);
  });

  it('handles a whole stylesheet, leaving the other rules in place', () => {
    const css = `.katex{font:normal 1.21em KaTeX_Main}${SOURCE_FORM}.katex-display{display:block}`;
    const trimmed = keepWoff2Sources(css);

    expect(trimmed.startsWith('.katex{font:normal 1.21em KaTeX_Main}')).toBe(true);
    expect(trimmed.endsWith('.katex-display{display:block}')).toBe(true);
    expect(trimmed.match(/@font-face/g)).toHaveLength(1);
    expect(braces(trimmed)).toBe(0);
  });
});
