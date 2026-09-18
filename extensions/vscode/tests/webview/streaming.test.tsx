import { afterEach, describe, expect, it, vi } from 'vitest';

import type { CellModel } from '../../src/shared';
import { FIXTURE_EPOCH, makeFixtureSession } from '../../src/testing/fixtures';
import type * as ParseModule from '../../src/webview/chat/markdown/parse';
import type * as RenderModule from '../../src/webview/chat/markdown/render';
import { cellElement, disposeMounted, mountWebview, pushPatch } from './harness';

/**
 * The streaming performance invariants, made deterministic.
 *
 * Three seams are counted by wrapping the modules with `vi.mock`:
 *
 * - **`render.MarkdownNodes`** — its call count *is* the render count of the
 *   blocks (a memoized `MarkdownBlock` only calls it when it re-renders);
 * - **`parse.parseMarkdown`** — the parse count **and** the parsed volume: the
 *   mock records every source string, so we can assert both how many times and
 *   *how much* text was parsed. Counting only calls cannot tell "parse the tail"
 *   apart from "parse the whole answer" (both are one call per chunk);
 * - **DOM identity** — a block that has been promoted must keep its node: a
 *   re-created node restarts the fade-in animation and drops the user's text
 *   selection.
 *
 * All counts are exact and cheap: no timing, no flake. (Counts are doubled by
 * React StrictMode in development, which every bound accounts for.)
 */

const { renders, parses } = vi.hoisted(() => ({ renders: [] as number[], parses: [] as string[] }));

vi.mock('../../src/webview/chat/markdown/render', async (importOriginal) => {
  const actual = await importOriginal<typeof RenderModule>();
  return {
    ...actual,
    MarkdownNodes: (props: { nodes: readonly unknown[]; live?: boolean }) => {
      renders.push(props.nodes.length);
      return actual.MarkdownNodes(props as Parameters<typeof actual.MarkdownNodes>[0]);
    },
  };
});

vi.mock('../../src/webview/chat/markdown/parse', async (importOriginal) => {
  const actual = await importOriginal<typeof ParseModule>();
  return {
    ...actual,
    parseMarkdown: (source: string) => {
      parses.push(source);
      return actual.parseMarkdown(source);
    },
  };
});

afterEach(() => {
  disposeMounted();
  renders.length = 0;
  parses.length = 0;
});

const BLOCK_COUNT = 20;
const APPENDS = 40;
/** `' chunk NN'` — the chunk the append loop below sends. */
const CHUNK_CHARS = 8;

function longAnswer(): string {
  const blocks: string[] = [];
  for (let index = 0; index < BLOCK_COUNT; index += 1) {
    blocks.push(`Paragraph ${index} of the answer, with **markdown** inside.`);
  }
  return blocks.join('\n\n');
}

/** Parsed characters, and the largest single parse. */
function parseStats(): { readonly chars: number; readonly max: number } {
  return {
    chars: parses.reduce((total, source) => total + source.length, 0),
    max: parses.reduce((largest, source) => Math.max(largest, source.length), 0),
  };
}

describe('streaming transcript', () => {
  it('re-renders and re-parses only the growing tail, never the finished blocks', () => {
    const mounted = mountWebview([
      makeFixtureSession({
        cells: [
          { kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: longAnswer(), streaming: true },
        ],
      }),
    ]);
    const { container } = mounted;

    // The initial mount renders every block once (doubled by React StrictMode in
    // development) — O(blocks), not O(blocks²).
    expect(renders.length).toBeLessThanOrEqual((BLOCK_COUNT + 1) * 2);
    expect(parses.length).toBeLessThanOrEqual(BLOCK_COUNT * 2);
    renders.length = 0;
    parses.length = 0;

    const firstParagraph = container.querySelector('[data-cell-id="a1"] p');
    expect(firstParagraph?.textContent).toContain('Paragraph 0');

    for (let index = 0; index < APPENDS; index += 1) {
      pushPatch(mounted, 'session-a', index + 1, [
        { op: 'append_text', cellId: 'a1', text: ` chunk ${index}` },
      ]);
    }

    // Only the live tail renders/parses per patch (×2 for StrictMode). Without the
    // memoized stable prefix this would be APPENDS × BLOCK_COUNT = 800.
    expect(renders.length).toBeLessThanOrEqual(APPENDS * 2 + 4);
    expect(parses.length).toBeLessThanOrEqual(APPENDS * 2 + 4);
    expect(renders.length).toBeGreaterThanOrEqual(APPENDS);
    expect(renders.length).toBeLessThan((APPENDS * BLOCK_COUNT) / 4);

    // …and the *volume* stays tail-sized. A renderer that re-parses the whole
    // answer once per chunk has the same call count (1/chunk) but parses
    // `APPENDS × answer` characters, so the counts above cannot see it — these two
    // bounds can (measured: 18,290 chars / 403 max for the tail-only version,
    // 101,090 / 1,438 for the whole-answer mutation).
    const blockChars = Math.ceil(longAnswer().length / BLOCK_COUNT);
    const tailBudget = blockChars + APPENDS * CHUNK_CHARS;
    const { chars, max } = parseStats();
    expect(max).toBeLessThan(tailBudget * 2);
    expect(chars).toBeLessThan(2 * (BLOCK_COUNT * blockChars + APPENDS * tailBudget));

    // The stable prefix is not even re-created: the first paragraph is the same node.
    expect(container.querySelector('[data-cell-id="a1"] p')).toBe(firstParagraph);
    expect(container.textContent).toContain('chunk 39');
  });

  it('does bounded work when a tail block becomes stable', () => {
    const mounted = mountWebview([
      makeFixtureSession({
        cells: [
          { kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: 'first paragraph', streaming: true },
        ],
      }),
    ]);

    renders.length = 0;
    parses.length = 0;

    pushPatch(mounted, 'session-a', 1, [
      { op: 'append_text', cellId: 'a1', text: '\n\nsecond paragraph\n\nthird' },
    ]);

    // A constant amount of work: the promoted block (its first render) plus the new
    // tail — not once per block of the answer.
    expect(renders.length).toBeLessThanOrEqual(8);
    expect(parses.length).toBeLessThanOrEqual(8);
    expect(mounted.container.textContent).toContain('third');
  });
});

// ── block promotion must not re-create DOM ────────────────────────────

/**
 * Top-level block elements of one cell's Markdown region.
 *
 * The streaming caret is not a block: it lives inside the growing tail (or as a
 * sibling span when the text paused on a boundary) and is legitimately re-created
 * when the tail moves — the invariant under test is about *content* blocks.
 */
function blockNodes(container: HTMLElement, cellId = 'a1'): HTMLElement[] {
  const markdown = cellElement(container, cellId).firstElementChild;
  if (markdown === null) {
    return [];
  }
  return [...markdown.children]
    .map((node) => node as HTMLElement)
    .filter((node) => node.dataset['testid'] !== 'stream-caret');
}

function mountStreaming(): ReturnType<typeof mountWebview> & { readonly seq: { value: number } } {
  const mounted = mountWebview([
    makeFixtureSession({
      cells: [{ kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: '', streaming: true }],
    }),
  ]);
  return { ...mounted, seq: { value: 0 } };
}

/** Send one `append_text` chunk (keeping `seq` consistent). */
function stream(
  mounted: ReturnType<typeof mountStreaming>,
  text: string,
  cell: CellModel | null = null,
): void {
  void cell;
  mounted.seq.value += 1;
  pushPatch(mounted, 'session-a', mounted.seq.value, [{ op: 'append_text', cellId: 'a1', text }]);
}

describe('block promotion (a growing list / line-oriented output)', () => {
  it('keeps every block node alive while lines are appended one by one', () => {
    const mounted = mountStreaming();
    const seen: HTMLElement[] = [];

    const record = (): void => {
      for (const node of blockNodes(mounted.container)) {
        if (!seen.includes(node)) {
          seen.push(node);
        }
      }
      for (const node of seen) {
        expect(node.isConnected).toBe(true);
      }
    };

    record();
    // A list that is still being written: every chunk extends the *same* block
    // (a trailing `\n` is not a block boundary — the next line can still belong to
    // it), so the list must not be re-created chunk by chunk.
    for (const chunk of ['- a\n', '- b', '\n', '- c\n', '- d\n']) {
      stream(mounted, chunk);
      record();
      expect(blockNodes(mounted.container)).toHaveLength(1);
    }

    const list = blockNodes(mounted.container)[0];
    expect(list?.tagName).toBe('UL');
    expect([...list!.children].map((item) => item.textContent)).toEqual(['a', 'b', 'c', 'd']);

    // The first *terminated* blank line ends the list: it is promoted in place and
    // the next block mounts as a genuinely new node.
    stream(mounted, '\n\nsecond paragraph');
    record();

    const afterPromotion = blockNodes(mounted.container);
    expect(afterPromotion).toHaveLength(2);
    expect(afterPromotion[0]).toBe(list);
    expect(afterPromotion[1]?.tagName).toBe('P');
  });

  it('keeps the streaming caret in one place while a paragraph grows line by line', () => {
    const mounted = mountStreaming();
    let previous: HTMLElement | null = null;

    // Every chunk ends a line (`\n`), i.e. right after each one the text *looks*
    // like it could be finished. The caret must stay inside the paragraph the user
    // is reading — an unterminated trailing line is not a block boundary.
    for (const chunk of ['first line\n', 'second ', 'line\n', 'third line']) {
      stream(mounted, chunk);
      const caret = mounted.container.querySelector<HTMLElement>('[data-testid="stream-caret"]');
      expect(caret).not.toBeNull();
      if (previous !== null) {
        // Same element: no re-created caret, no restarted ellipsis animation, no
        // jump between "inside the paragraph" and "after the block".
        expect(caret).toBe(previous);
      }
      previous = caret;
    }

    expect(mounted.container.textContent).toContain('first line');
    expect(mounted.container.textContent).toContain('third line');
  });

  it('never disconnects a promoted block, even at 4 characters per chunk', () => {
    const mounted = mountStreaming();
    const answer = [
      '## Why',
      '',
      '- the renderer keeps a stable prefix',
      '- only the tail re-parses',
      '',
      'Code:',
      '',
      '```ts',
      'const tail = blocks.at(-1);',
      '```',
      '',
      'Done.',
      '',
    ].join('\n');

    /** Nodes that were seen while *not* being the live tail. */
    const promoted = new Set<HTMLElement>();
    /** Nodes the renderer dropped — only ever the live block (see below). */
    const dropped = new Set<HTMLElement>();
    const seen = new Set<HTMLElement>();

    for (let offset = 0; offset < answer.length; offset += 4) {
      stream(mounted, answer.slice(offset, offset + 4));
      const current = blockNodes(mounted.container);
      current.forEach((node, index) => {
        seen.add(node);
        if (index < current.length - 1) {
          promoted.add(node);
        }
      });
      for (const node of seen) {
        if (!node.isConnected) {
          dropped.add(node);
        }
      }
      expect([...promoted].filter((node) => !node.isConnected).map((node) => node.tagName)).toEqual([]);
    }

    // A block that has been promoted is final and must survive to the end.
    expect([...promoted].every((node) => node.isConnected)).toBe(true);
    expect(blockNodes(mounted.container).length).toBeGreaterThanOrEqual(4);
    expect(mounted.container.textContent).toContain('only the tail re-parses');
    expect(mounted.container.textContent).toContain('const tail = blocks.at(-1);');

    // The only node the renderer is allowed to drop is the *live* block changing
    // shape: a code fence is a paragraph until its opening line is complete, and
    // then markdown says it is a code block. That happens once per fence — never
    // per chunk — and never to a promoted block.
    expect([...dropped].length).toBeLessThanOrEqual(2);
  });
});
