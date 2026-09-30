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

  it('does not mistake currency or a digit suffix for a formula', () => {
    expect(inline('costs $100 and $200 total')).toEqual([
      { kind: 'text', text: 'costs $100 and $200 total' },
    ]);
    expect(inline('$x$1')).toEqual([{ kind: 'text', text: '$x$1' }]);
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
