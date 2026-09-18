import { render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { MarkdownText } from '../../src/webview/chat/Markdown';
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
