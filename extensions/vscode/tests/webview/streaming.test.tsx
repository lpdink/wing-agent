import { afterEach, describe, expect, it, vi } from 'vitest';

import { FIXTURE_EPOCH, makeFixtureSession } from '../../src/testing/fixtures';
import type * as ParseModule from '../../src/webview/chat/markdown/parse';
import type * as RenderModule from '../../src/webview/chat/markdown/render';
import { disposeMounted, mountWebview, pushPatch } from './harness';

/**
 * The streaming performance invariant, made deterministic.
 *
 * "Appending text must not re-render the whole answer" is asserted by counting
 * calls into two seams:
 *
 * - **`render.MarkdownNodes`** — its call count *is* the render count of the
 *   blocks (the memoized `MarkdownBlock` only calls it when it re-renders);
 * - **`parse.parseMarkdown`** — the parse count, which the block-local `useMemo`
 *   must keep proportional to the tail.
 *
 * A naive renderer (render + parse every block on every patch) lands at
 * `appends × blocks`; the stable-prefix renderer stays proportional to `appends`.
 * The counts are exact and cheap — no timing, no flake.
 */

const { renders, parses } = vi.hoisted(() => ({ renders: [] as number[], parses: [] as string[] }));

vi.mock('../../src/webview/chat/markdown/render', async (importOriginal) => {
  const actual = await importOriginal<typeof RenderModule>();
  return {
    ...actual,
    MarkdownNodes: (props: { nodes: readonly unknown[] }) => {
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

function longAnswer(): string {
  const blocks: string[] = [];
  for (let index = 0; index < BLOCK_COUNT; index += 1) {
    blocks.push(`Paragraph ${index} of the answer, with **markdown** inside.`);
  }
  return blocks.join('\n\n');
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
