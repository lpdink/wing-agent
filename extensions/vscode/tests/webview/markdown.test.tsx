import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { MarkdownText } from '../../src/webview/chat/Markdown';
import type { MarkdownInline } from '../../src/webview/chat/markdown/parse';
import { parseMarkdown } from '../../src/webview/chat/markdown/parse';
import { splitStreamingBlocks } from '../../src/webview/chat/markdown/split';
import { highlightCode } from '../../src/webview/chat/markdown/highlight';

/**
 * Markdown layer: the block splitter (the streaming invariant), the parser
 * (tables / lists / fences) and the renderer (links, inline code, code blocks).
 */

describe('splitStreamingBlocks', () => {
  it('keeps the last block live and the rest stable', () => {
    const { stable, tail } = splitStreamingBlocks('first paragraph\n\nsecond paragraph');

    expect(stable).toEqual(['first paragraph\n']);
    expect(tail).toBe('second paragraph');
  });

  it('reports nothing live when the text ends on a boundary', () => {
    const { stable, tail } = splitStreamingBlocks('one\n\ntwo\n\n');

    expect(stable).toEqual(['one\n', 'two\n']);
    expect(tail).toBe('');
  });

  it('does not split inside a fenced code block', () => {
    const text = 'before\n\n```ts\nconst a = 1;\n\nconst b = 2;\n```\n\nafter';

    const { stable, tail } = splitStreamingBlocks(text);

    expect(stable).toEqual(['before\n', '```ts\nconst a = 1;\n\nconst b = 2;\n```\n']);
    expect(tail).toBe('after');
  });

  it('treats an unterminated fence as the active tail, even across blank lines', () => {
    const { stable, tail } = splitStreamingBlocks('intro\n\n```ts\nline one\n\nline two');

    expect(stable).toEqual(['intro\n']);
    expect(tail).toBe('```ts\nline one\n\nline two');
  });

  it('handles tildes, longer closing fences and trailing whitespace', () => {
    const { stable, tail } = splitStreamingBlocks('~~~\ncode\n\nmore\n~~~~\n\nend');

    expect(stable).toEqual(['~~~\ncode\n\nmore\n~~~~\n']);
    expect(tail).toBe('end');
  });

  it('is empty for empty text', () => {
    expect(splitStreamingBlocks('')).toEqual({ stable: [], tail: '' });
    expect(splitStreamingBlocks('\n\n')).toEqual({ stable: [], tail: '' });
  });
});

describe('splitStreamingBlocks (streaming invariant)', () => {
  /** Deterministic LCG so a failure can be reproduced from its seed. */
  function makeRandom(seed: number): () => number {
    let state = (seed * 2654435761) >>> 0;
    return () => {
      state = (state * 1664525 + 1013904223) >>> 0;
      return state / 0x1_0000_0000;
    };
  }

  const DOCUMENTS = [
    '- item 0\n- item 1\n- item 2\n- item 3\n',
    'first line\nsecond line\n\nnext paragraph\n',
    'Text with `code` and **bold**.\nAnother hard-wrapped line.\n\n> quote\n',
    '| a | b |\n| - | - |\n| 1 | 2 |\n\n```ts\nconst a = 1;\n\nconst b = 2;\n```\n\ntrailing\n',
    '```python\ndef f():\n    return 1\n```\n',
    'para\n\n\n\nblank lines in between\n\n',
    '~~~\nunclosed fence\n\nstill inside\n',
  ];

  it('never retracts or rewrites a block that was already stable (fuzz, 60 seeds)', () => {
    for (let seed = 1; seed <= 60; seed += 1) {
      const random = makeRandom(seed);
      const document = DOCUMENTS[Math.floor(random() * DOCUMENTS.length)] ?? DOCUMENTS[0] ?? '';
      const chunkSize = 1 + Math.floor(random() * 9);
      let text = '';
      let previous: readonly string[] = [];

      while (text.length < document.length) {
        text = document.slice(0, Math.min(document.length, text.length + chunkSize));
        const { stable, tail } = splitStreamingBlocks(text);

        // The property: `stable(T)` is always a prefix of `stable(T + chunk)`.
        expect({ seed, chunk: text.length, stable: stable.slice(0, previous.length) }).toEqual({
          seed,
          chunk: text.length,
          stable: previous,
        });
        // …and every block is non-empty (an empty block would be a phantom).
        expect(stable.every((block) => block.trim() !== '')).toBe(true);
        expect(tail === '' || !tail.startsWith('\u0000')).toBe(true);
        previous = stable;
      }
    }
  });

  it('keeps a line-oriented answer live until a terminated blank line arrives', () => {
    // A trailing `\n` does not end the block…
    expect(splitStreamingBlocks('- item 0\n')).toEqual({ stable: [], tail: '- item 0\n' });
    expect(splitStreamingBlocks('- item 0\n- item 1\n')).toEqual({
      stable: [],
      tail: '- item 0\n- item 1\n',
    });
    // …a terminated blank line does.
    expect(splitStreamingBlocks('- item 0\n- item 1\n\n')).toEqual({
      stable: ['- item 0\n- item 1\n'],
      tail: '',
    });
    // …and the promoted block never changes again.
    expect(splitStreamingBlocks('- item 0\n- item 1\n\n- item 2\n')).toEqual({
      stable: ['- item 0\n- item 1\n'],
      tail: '- item 2\n',
    });
  });
});

describe('parseMarkdown', () => {
  it('parses headings, paragraphs and inline emphasis', () => {
    const nodes = parseMarkdown('## Title\n\nsome **bold** and *italic* and ~~gone~~\n');

    expect(nodes).toHaveLength(2);
    expect(nodes[0]).toMatchObject({ kind: 'heading', level: 2 });
    const paragraph = nodes[1];
    expect(paragraph?.kind).toBe('paragraph');
    if (paragraph?.kind !== 'paragraph') {
      throw new Error('expected a paragraph');
    }
    expect(paragraph.children.map((child) => child.kind)).toEqual([
      'text',
      'strong',
      'text',
      'em',
      'text',
      'del',
    ]);
  });

  it('parses lists, quotes and tables', () => {
    const nodes = parseMarkdown('- a\n- b\n\n> quoted\n\n| a | b |\n| - | - |\n| 1 | 2 |\n');

    expect(nodes.map((node) => node.kind)).toEqual(['list', 'quote', 'table']);
    const list = nodes[0];
    if (list?.kind !== 'list') {
      throw new Error('expected a list');
    }
    expect(list.ordered).toBe(false);
    expect(list.items).toHaveLength(2);
    const table = nodes[2];
    if (table?.kind !== 'table') {
      throw new Error('expected a table');
    }
    expect(table.head).toHaveLength(1);
    expect(table.rows).toHaveLength(1);
  });

  it('keeps fenced code with its language and marks autolinks', () => {
    const nodes = parseMarkdown('```python\nprint(1)\n```\n\nsee https://example.com now\n');

    expect(nodes[0]).toMatchObject({ kind: 'code', lang: 'python', code: 'print(1)\n' });
    const paragraph = nodes[1];
    if (paragraph?.kind !== 'paragraph') {
      throw new Error('expected a paragraph');
    }
    expect(paragraph.children.some((child) => child.kind === 'link')).toBe(true);
  });

  it('never produces markup from raw HTML', () => {
    const nodes = parseMarkdown('<script>alert(1)</script>\n');
    const paragraph = nodes[0];
    if (paragraph?.kind !== 'paragraph') {
      throw new Error('expected a paragraph');
    }
    expect(paragraph.children).toEqual([{ kind: 'text', text: '<script>alert(1)</script>' }]);
  });
});

describe('math syntax (parseMarkdown)', () => {
  /** The inline children of the first paragraph — the shape most assertions want. */
  function inline(source: string): readonly MarkdownInline[] {
    const paragraph = parseMarkdown(source)[0];
    if (paragraph?.kind !== 'paragraph') {
      throw new Error(`expected a paragraph, got ${paragraph?.kind ?? 'nothing'}`);
    }
    return paragraph.children;
  }

  it('parses `$…$` into an inline formula with its literal source', () => {
    expect(inline('before $\\frac{a}{b}$ after')).toEqual([
      { kind: 'text', text: 'before ' },
      { kind: 'math', tex: '\\frac{a}{b}', source: '$\\frac{a}{b}$', display: false },
      { kind: 'text', text: ' after' },
    ]);
  });

  it('parses `$$…$$` on one line as a display formula inside the paragraph', () => {
    expect(inline('$$x^2$$')).toEqual([{ kind: 'math', tex: 'x^2', source: '$$x^2$$', display: true }]);
  });

  it('parses a `$$` block as a block node spanning several lines', () => {
    const nodes = parseMarkdown(
      'text\n\ntext\n$$\n\\begin{aligned}\na &= b\\\\\nc &= d\n\\end{aligned}\n$$\nafter\n',
    );

    expect(nodes.map((node) => node.kind)).toEqual(['paragraph', 'paragraph', 'math', 'paragraph']);
    const math = nodes[2];
    expect(math).toMatchObject({
      kind: 'math',
      display: true,
      tex: '\\begin{aligned}\na &= b\\\\\nc &= d\n\\end{aligned}',
      source: '$$\n\\begin{aligned}\na &= b\\\\\nc &= d\n\\end{aligned}\n$$',
    });
  });

  it('lets a `$$` block interrupt a paragraph without a blank line', () => {
    const nodes = parseMarkdown('a line right above\n$$\nx\n$$\nbelow\n');

    expect(nodes.map((node) => node.kind)).toEqual(['paragraph', 'math', 'paragraph']);
  });

  it('parses the LaTeX delimiters `\\(…\\)` and `\\[…\\]`', () => {
    expect(inline('\\(\\alpha+\\)')).toEqual([
      { kind: 'math', tex: '\\alpha+', source: '\\(\\alpha+\\)', display: false },
    ]);
    expect(inline('\\[\\alpha\\]')).toEqual([
      { kind: 'math', tex: '\\alpha', source: '\\[\\alpha\\]', display: true },
    ]);
  });

  it('keeps `$` inside code spans and fences literal', () => {
    expect(inline('`$x$`')).toEqual([{ kind: 'code', text: '$x$' }]);
    expect(parseMarkdown('```\n$\\frac{1}{2}$\n```\n')[0]).toMatchObject({
      kind: 'code',
      code: '$\\frac{1}{2}$\n',
    });
  });

  it('does not mistake currency for a formula', () => {
    expect(inline('costs $100 and $200 total')).toEqual([
      { kind: 'text', text: 'costs $100 and $200 total' },
    ]);
    expect(inline('a \\$5 note')).toEqual([{ kind: 'text', text: 'a $5 note' }]);
  });

  it('leaves unterminated or empty formulas as text (never drops content)', () => {
    expect(inline('$\\frac{1}{2}')).toEqual([{ kind: 'text', text: '$\\frac{1}{2}' }]);
    // An unterminated `$$` opener stays literal — including the text after it.
    expect(inline('$$\nstill open')).toEqual([
      { kind: 'text', text: '$$' },
      { kind: 'break', hard: false },
      { kind: 'text', text: 'still open' },
    ]);
    // An empty formula is not a formula: `$$` alone, or `$$` … `$$` with no body.
    expect(parseMarkdown('$$\n$$\n')[0]).toMatchObject({ kind: 'paragraph' });
    expect(inline('$$$$')).toEqual([{ kind: 'text', text: '$$$$' }]);
  });

  it('keeps `\\$` escapes inside a formula from ending it early', () => {
    expect(inline('$\\$5 + \\$7$')).toEqual([
      { kind: 'math', tex: '\\$5 + \\$7', source: '$\\$5 + \\$7$', display: false },
    ]);
  });

  // ── the TUI lane's identification rules ─────────────────────────────
  //
  // Everything below is aligned with `crates/wing/src/render/markdown/math.rs`
  // and its tables in `docs/dev/tui-rendering.md` §2.5 (see
  // `docs/dev/vscode-extension.md` §8.5 for the user-facing summary and the
  // differences that stay). The TUI lane reaches the same spans by *rewriting
  // the source text* before parsing; here the rules recognize them directly and
  // never touch the input — an unrecognized span is ordinary markdown text.

  it('wraps a bare AMS environment (no `$$`) as display math', () => {
    const source = '\\begin{align}\nf(x) &= x^2 \\\\\n&= (x+1)^2\n\\end{align}';
    expect(inline(source)).toEqual([
      // `tex` and `source` are the same text: the environment as written is what
      // KaTeX gets (it supports `align` in display mode) and what a refusal
      // falls back to.
      { kind: 'math', tex: source, source, display: true },
    ]);
  });

  it('recognizes an environment inside a sentence, leaving the prose around it', () => {
    expect(inline('f(x) = \\begin{cases}1 & x > 0\\end{cases} done')).toEqual([
      { kind: 'text', text: 'f(x) = ' },
      {
        kind: 'math',
        tex: '\\begin{cases}1 & x > 0\\end{cases}',
        source: '\\begin{cases}1 & x > 0\\end{cases}',
        display: true,
      },
      { kind: 'text', text: ' done' },
    ]);
  });

  it('accepts the starred variants and the matrix family', () => {
    expect(inline('\\begin{align*}a\\end{align*}')[0]).toMatchObject({
      kind: 'math',
      display: true,
      tex: '\\begin{align*}a\\end{align*}',
    });
    expect(inline('\\begin{pmatrix}a & b\\end{pmatrix}')[0]).toMatchObject({ kind: 'math', display: true });
    expect(inline('\\begin{gather}\na = b\n\\end{gather}')[0]).toMatchObject({ kind: 'math', display: true });
  });

  it('leaves an unknown or unterminated environment exactly as written', () => {
    expect(inline('\\begin{tikzcd} a \\arrow[r] & b \\end{tikzcd}')).toEqual([
      { kind: 'text', text: '\\begin{tikzcd} a \\arrow[r] & b \\end{tikzcd}' },
    ]);
    expect(inline('\\begin{align}\na &= b\n')).toEqual([
      { kind: 'text', text: '\\begin{align}' },
      { kind: 'break', hard: false },
      { kind: 'text', text: 'a &= b' },
    ]);
  });

  it('counts same-name nesting before closing', () => {
    const source = '\\begin{array}{l}\\begin{array}{l}a\\end{array}\\end{array}';
    expect(inline(source)).toEqual([{ kind: 'math', tex: source, source, display: true }]);
  });

  it('refuses an environment body that carries code or a formula of its own', () => {
    // The TUI bails on a backtick or a `$` inside the span, and both sides then
    // render the inner construct as what it is.
    expect(inline('\\begin{align}a `b` b\\end{align}')).toEqual([
      { kind: 'text', text: '\\begin{align}a ' },
      { kind: 'code', text: 'b' },
      { kind: 'text', text: ' b\\end{align}' },
    ]);
    expect(inline('\\begin{align}x $y$ z\\end{align}')).toEqual([
      { kind: 'text', text: '\\begin{align}x ' },
      { kind: 'math', tex: 'y', source: '$y$', display: false },
      { kind: 'text', text: ' z\\end{align}' },
    ]);
  });

  it('refuses an environment or `\\(…\\)` that touches an existing `$`', () => {
    // The TUI's `fuses_with_dollar`: an inserted `$$` next to a `$` that is
    // already there would re-pair it.
    expect(inline('\\begin{align}a\\end{align}$')).toEqual([
      { kind: 'text', text: '\\begin{align}a\\end{align}$' },
    ]);
    expect(inline('\\(x\\)$$\\\\(y\\\\)')).toEqual([{ kind: 'text', text: '(x)$$\\(y\\)' }]);
  });

  it('leaves a span longer than the budget as text (the TUI does the same)', () => {
    const long = `\\(x${'a'.repeat(10_000)}x\\)`;
    const children = inline(long);
    expect(children.some((child) => child.kind === 'math')).toBe(false);
    // No content is lost: markdown's own escaping turns `\(` / `\)` into the
    // parens, exactly as it does in the TUI for a span its normalizer refuses.
    expect(children).toEqual([{ kind: 'text', text: `(x${'a'.repeat(10_000)}x)` }]);
    expect(inline(`\\begin{align}${'a'.repeat(9_000)}\\end{align}`)).toEqual([
      { kind: 'text', text: `\\begin{align}${'a'.repeat(9_000)}\\end{align}` },
    ]);
  });

  it('keeps a `$…$` span of any length a formula (pulldown pairs it too)', () => {
    // Not the same rule as the LaTeX delimiters: pulldown pairs `$…$` whatever
    // its length, and the TUI degrades at *render* time — so does this side
    // (`math.ts` refuses it and `MathView` shows the complete source).
    const tex = 'y'.repeat(9_000);
    expect(inline(`$${tex}$`)).toEqual([{ kind: 'math', tex, source: `$${tex}$`, display: false }]);
  });

  // ── opaque regions ──────────────────────────────────────────────────

  it('never recognizes math inside code spans, fences or indented code', () => {
    expect(inline('use `\\(x\\)` here but \\(y\\) there')).toEqual([
      { kind: 'text', text: 'use ' },
      { kind: 'code', text: '\\(x\\)' },
      { kind: 'text', text: ' here but ' },
      { kind: 'math', tex: 'y', source: '\\(y\\)', display: false },
      { kind: 'text', text: ' there' },
    ]);
    expect(parseMarkdown('```latex\n\\(x\\)\n```\n')[0]).toMatchObject({ kind: 'code' });
    expect(parseMarkdown('before\n\n    \\(x\\)\n\nafter\n')).toEqual([
      { kind: 'paragraph', children: [{ kind: 'text', text: 'before' }] },
      { kind: 'code', lang: '', code: '\\(x\\)\n', closed: true },
      { kind: 'paragraph', children: [{ kind: 'text', text: 'after' }] },
    ]);
  });

  it('keeps a fence behind a block prefix opaque (and a lazy line prose)', () => {
    // `> ~~~` and `- ``` ` are fences to the parser, so their content is code —
    // the TUI strips the prefix to decide, markdown-it resolves it itself.
    const quoted = parseMarkdown('> ~~~\n> \\(x\\)\n> ~~~\n');
    expect(quoted[0]).toMatchObject({ kind: 'quote' });
    expect(parseMarkdown('- ```\n  \\(x\\)\n  ```\n')[0]).toMatchObject({
      kind: 'list',
    });
    // A 4-space line below paragraph text is a lazy continuation, not code.
    expect(inline('text\n    \\(x\\)')).toEqual([
      { kind: 'text', text: 'text' },
      { kind: 'break', hard: false },
      { kind: 'math', tex: 'x', source: '\\(x\\)', display: false },
    ]);
  });

  it('leaves link and image destinations (and titles) untouched', () => {
    // The URL is what a click opens; a formula there would also break it. The
    // `\(` in the destination is not math — `link` rule territory, never ours.
    expect(inline('[a](http://x/\\(y\\))')).toEqual([
      { kind: 'link', href: 'http://x/(y)', children: [{ kind: 'text', text: 'a' }] },
    ]);
    expect(inline('[a](http://x/(y)/\\(z\\))')).toEqual([
      { kind: 'link', href: 'http://x/(y)/(z)', children: [{ kind: 'text', text: 'a' }] },
    ]);
    expect(inline('[a](http://x/\\(y\\) "t \\(z\\)")')).toEqual([
      { kind: 'link', href: 'http://x/(y)', children: [{ kind: 'text', text: 'a' }] },
    ]);
    expect(inline('![img](http://x/\\(y\\) "t \\(z\\)")')).toEqual([
      { kind: 'image', src: 'http://x/(y)', alt: 'img' },
    ]);
    // …while the *label* is prose and still normalizes (`[\\(x\\)](…)`).
    expect(inline('[\\(x\\)](http://e/\\(y\\))')).toEqual([
      {
        kind: 'link',
        href: 'http://e/(y)',
        children: [{ kind: 'math', tex: 'x', source: '\\(x\\)', display: false }],
      },
    ]);
  });

  it('leaves a link reference definition line alone', () => {
    // The block parser consumes the definition (destination + title) before any
    // inline rule runs; the TUI's line-based rule reaches the same conclusion.
    expect(parseMarkdown('[ref]: http://x/\\(y\\) "title \\(z\\)"\n\nuse \\(a\\)\n')).toEqual([
      {
        kind: 'paragraph',
        children: [
          { kind: 'text', text: 'use ' },
          { kind: 'math', tex: 'a', source: '\\(a\\)', display: false },
        ],
      },
    ]);
  });

  it('keeps HTML blocks out of math recognition', () => {
    // A tag-like line opens an opaque region to the next blank line, exactly
    // like the TUI's HTML block. `html: false` means this is text, not markup —
    // markdown's own escaping still applies (`\(` → `(`), which is what this
    // webview did before formulas existed.
    expect(inline('<div>\n\\(x\\)\n</div>')).toEqual([
      { kind: 'text', text: '<div>' },
      { kind: 'break', hard: false },
      { kind: 'text', text: '(x)' },
      { kind: 'break', hard: false },
      { kind: 'text', text: '</div>' },
    ]);
    expect(inline('text\n<div>\n\\(x\\)')).toEqual([
      { kind: 'text', text: 'text' },
      { kind: 'break', hard: false },
      { kind: 'text', text: '<div>' },
      { kind: 'break', hard: false },
      { kind: 'text', text: '(x)' },
    ]);
    // The block starts after a blank line (the run boundary is above the tag,
    // not below it) …
    expect(parseMarkdown('text\n\n<div>\n$$\nx\n$$\n')[1]).toMatchObject({ kind: 'paragraph' });
    // … and a formula *after* the HTML region (a blank line away) is a formula.
    expect(parseMarkdown('<b>t</b>\n\n$$\nx\n$$\n')[1]).toMatchObject({ kind: 'math' });
    expect(inline('<!-- c -->\n\\(x\\)')[0]).toEqual({ kind: 'text', text: '<!-- c -->' });
    // …and the block rule for a standalone `$$` obeys the same region.
    expect(parseMarkdown('<div>\n$$\nx\n$$\n')).toEqual([
      {
        kind: 'paragraph',
        children: [
          { kind: 'text', text: '<div>' },
          { kind: 'break', hard: false },
          { kind: 'text', text: '$$' },
          { kind: 'break', hard: false },
          { kind: 'text', text: 'x' },
          { kind: 'break', hard: false },
          { kind: 'text', text: '$$' },
        ],
      },
    ]);
  });

  it('keeps an inline HTML tag / autolink opaque, but not the prose between tags', () => {
    // Attribute values live inside `<…>`: never a formula (the TUI's
    // `skip_inline_html`).
    expect(inline('<a href="http://x/\\(y\\)">label</a>').some((child) => child.kind === 'math')).toBe(false);
    expect(inline('code: <https://example.com/\\(y\\)>').some((child) => child.kind === 'math')).toBe(false);
    // Between two tags the text is prose — the TUI renders the formula too.
    expect(inline('before <b>\\(x\\)</b> after')).toEqual([
      { kind: 'text', text: 'before <b>' },
      { kind: 'math', tex: 'x', source: '\\(x\\)', display: false },
      { kind: 'text', text: '</b> after' },
    ]);
    // A stray `<` is not a region: comparisons and arrows stay prose.
    expect(inline('a <- \\(x\\)')).toEqual([
      { kind: 'text', text: 'a <- ' },
      { kind: 'math', tex: 'x', source: '\\(x\\)', display: false },
    ]);
  });

  it('keeps the HTML-block region inside a quote or a list item too', () => {
    // Prefixes are the parser's business (it strips `>` and list markers before
    // the block rule looks at a line), so `> <div>` opens an HTML block just
    // like a bare `<div>` does.
    for (const source of ['> <div>\n> $$\n> x\n> $$\n', '- <div>\n  $$\n  x\n  $$\n']) {
      const nodes = parseMarkdown(source);
      expect(JSON.stringify(nodes)).not.toContain('"kind":"math"');
      expect(JSON.stringify(nodes)).toContain('$$');
    }
  });

  it('accepts whitespace around a `$$` block delimiter line', () => {
    // The closer line is compared after trimming, without allocating (the block
    // rule walks lines with that comparison).
    expect(parseMarkdown('$$\n  x  \n  $$  \n')[0]).toMatchObject({
      kind: 'math',
      display: true,
    });
    // …and an opener that never closes stays one paragraph, verbatim.
    expect(parseMarkdown('\\[\nunclosed x\n\\[\nmore\n')[0]).toMatchObject({ kind: 'paragraph' });
  });

  // ── the normalizer's purity guards ──────────────────────────────────

  it('treats `$x$1` as a formula followed by `1`', () => {
    // pulldown-cmark has no "the closer must not be followed by a digit" rule
    // (that is pandoc's), so the TUI renders `x` — and so does this side now.
    expect(inline('$x$1 and 2')).toEqual([
      { kind: 'math', tex: 'x', source: '$x$', display: false },
      { kind: 'text', text: '1 and 2' },
    ]);
  });

  it('stops recognizing LaTeX delimiters after an unpaired `$$`', () => {
    // Positional rule (the TUI's `stray_display_delim`): the rest of the block
    // is left alone, so an inserted `$$` cannot re-pair the stray one.
    expect(inline('stray $$ here and \\(x\\)')).toEqual([{ kind: 'text', text: 'stray $$ here and (x)' }]);
    // A single `$` does not disarm (`$100` in prose is common).
    expect(inline('costs $100 and \\(x\\)')).toEqual([
      { kind: 'text', text: 'costs $100 and ' },
      { kind: 'math', tex: 'x', source: '\\(x\\)', display: false },
    ]);
  });

  it('does not recognize a span carrying code or a nested formula', () => {
    expect(inline('\\(a `b` c\\)')).toEqual([
      { kind: 'text', text: '(a ' },
      { kind: 'code', text: 'b' },
      { kind: 'text', text: ' c)' },
    ]);
    expect(inline('\\(a $b$ c\\)')).toEqual([
      { kind: 'text', text: '(a ' },
      { kind: 'math', tex: 'b', source: '$b$', display: false },
      { kind: 'text', text: ' c)' },
    ]);
  });

  it('does not recognize a whitespace-only or block-crossing span', () => {
    // `\(   \)` has nothing to render; the TUI leaves it alone and both sides
    // show markdown's escaped form.
    expect(inline('\\(   \\)')).toEqual([{ kind: 'text', text: '(   )' }]);
    // A span may not cross a blank line (it would be a different block here and
    // outside the TUI's span window there).
    expect(parseMarkdown('a \\(x\n\n+ y\\) b\n')).toEqual([
      { kind: 'paragraph', children: [{ kind: 'text', text: 'a (x' }] },
      {
        kind: 'list',
        ordered: false,
        start: 1,
        items: [[{ kind: 'paragraph', children: [{ kind: 'text', text: 'y) b' }] }]],
      },
    ]);
  });
});

describe('MarkdownText', () => {
  it('renders structure without innerHTML', () => {
    const { container } = render(
      <MarkdownText text={'Text with `code` and a [link](https://example.com).\n\n- one\n- two\n'} />,
    );

    expect(container.querySelector('code')?.textContent).toBe('code');
    expect(container.querySelector('a')?.getAttribute('href')).toBe('https://example.com');
    expect(container.querySelectorAll('li')).toHaveLength(2);
  });
});

describe('highlightCode', () => {
  it('highlights a known language into per-theme token variants', () => {
    const lines = highlightCode('const answer = 42;\n', 'ts');

    expect(lines).not.toBeNull();
    const tokens = (lines ?? []).flat();
    expect(tokens.map((token) => token.content).join('')).toBe('const answer = 42;');
    // `const` is a keyword: Dark+ #569CD6 / Light+ #0000FF — the same values as
    // `dark_plus.json` / `light_vs.json` in the VS Code sources.
    const keyword = tokens.find((token) => token.content.trim() === 'const');
    expect(keyword?.dark.color?.toLowerCase()).toBe('#569cd6');
    expect(keyword?.light.color?.toLowerCase()).toBe('#0000ff');
  });

  it('returns null for unknown languages and empty input instead of throwing', () => {
    expect(highlightCode('x', 'not-a-language')).toBeNull();
    expect(highlightCode('', 'ts')).toBeNull();
    expect(highlightCode('x', '')).toBeNull();
  });
});
