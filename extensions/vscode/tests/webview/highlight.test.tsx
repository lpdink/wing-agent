import { afterEach, describe, expect, it, vi } from 'vitest';

import { FIXTURE_EPOCH, makeFixtureSession } from '../../src/testing/fixtures';
import type * as HighlightModule from '../../src/webview/chat/markdown/highlight';
import { cellElement, disposeMounted, mountWebview, pushPatch } from './harness';

/**
 * Code-block highlighting cost.
 *
 * Highlighting is the most expensive thing the renderer does (shiki's JS engine
 * needs ~53 ms for 30 KB and ~447 ms for 300 KB in this bundle), and a streamed
 * code block used to be re-tokenized on **every** chunk — 500 lines over 100
 * chunks measured 2.2 s of CPU. The rule under test:
 *
 * - a code block whose closing fence has not arrived yet renders as plain text
 *   (it cannot be highlighted incrementally anyway), and
 * - it is highlighted **exactly once** afterwards, when the fence closes (a
 *   later promotion of the same block must not re-highlight it), and
 * - blocks above the size budget are never highlighted at all.
 *
 * Counted through a mocked `highlightCode`, so the assertion is deterministic —
 * no wall-clock timing.
 */

const { highlights } = vi.hoisted(() => ({ highlights: [] as { code: string; lang: string }[] }));

vi.mock('../../src/webview/chat/markdown/highlight', async (importOriginal) => {
  const actual = await importOriginal<typeof HighlightModule>();
  return {
    ...actual,
    highlightCode: (code: string, lang: string) => {
      highlights.push({ code, lang });
      return actual.highlightCode(code, lang);
    },
  };
});

afterEach(() => {
  disposeMounted();
  highlights.length = 0;
});

function mountStreamingCell(): ReturnType<typeof mountWebview> & { readonly seq: { value: number } } {
  const mounted = mountWebview([
    makeFixtureSession({
      cells: [{ kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: '', streaming: true }],
    }),
  ]);
  return { ...mounted, seq: { value: 0 } };
}

function stream(mounted: ReturnType<typeof mountStreamingCell>, text: string): void {
  mounted.seq.value += 1;
  pushPatch(mounted, 'session-a', mounted.seq.value, [{ op: 'append_text', cellId: 'a1', text }]);
}

describe('code block highlighting', () => {
  it('stays plain while the fence is open, then highlights once and caches', () => {
    const mounted = mountStreamingCell();

    stream(mounted, '```ts\n');
    stream(mounted, 'const a = 1;\n');
    stream(mounted, 'const b = 2;\n');
    expect(highlights).toEqual([]);
    expect(cellElement(mounted.container, 'a1').textContent).toContain('const a = 1;');

    // The closing fence makes the code final: highlighted once (×2 render passes
    // under React StrictMode), never per chunk.
    stream(mounted, '```\n');
    expect(highlights.length).toBeGreaterThan(0);
    expect(highlights[0]?.code).toBe('const a = 1;\nconst b = 2;\n');
    expect(highlights[0]?.lang).toBe('ts');
    expect(highlights.length).toBeLessThanOrEqual(2);

    // More text arrives (the block is promoted to "stable"), the code is unchanged:
    // the highlight result is reused, not recomputed.
    const afterClose = highlights.length;
    stream(mounted, '\n\nDone.');
    expect(highlights.length).toBe(afterClose);

    const code = cellElement(mounted.container, 'a1').querySelector('[data-testid="md-code"]');
    expect(code?.querySelectorAll('[style*="--shiki-"]').length).toBeGreaterThan(0);
  });

  it('never highlights a block above the size budget', () => {
    const big = Array.from({ length: 1800 }, (_, index) => `const value${index} = ${index};`).join('\n');
    expect(big.length).toBeGreaterThan(32 * 1024);

    const { container } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'assistant',
            id: 'a1',
            createdAt: FIXTURE_EPOCH,
            text: `\`\`\`ts\n${big}\n\`\`\`\n`,
            streaming: false,
          },
        ],
      }),
    ]);

    expect(highlights).toEqual([]);
    const code = cellElement(container, 'a1').querySelector('[data-testid="md-code"]');
    expect(code?.textContent).toContain('const value1799 = 1799;');
    // Plain text: no per-token colour variables.
    expect(code?.querySelectorAll('[style*="--shiki-"]')).toHaveLength(0);
  });

  it('highlights a finished code block', () => {
    const { container } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'assistant',
            id: 'a1',
            createdAt: FIXTURE_EPOCH,
            text: '```ts\nconst answer = 42;\n```\n',
            streaming: false,
          },
        ],
      }),
    ]);

    expect(highlights.length).toBeGreaterThan(0);
    expect(highlights[0]?.code).toBe('const answer = 42;\n');
    const keyword = [...cellElement(container, 'a1').querySelectorAll('[class*="codeToken"]')].find(
      (token) => token.textContent?.trim() === 'const',
    );
    expect(keyword?.getAttribute('style')).toContain('--shiki-dark: #569CD6');
  });
});
