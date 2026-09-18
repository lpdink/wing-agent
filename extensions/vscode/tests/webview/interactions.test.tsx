import { fireEvent, within } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { FIXTURE_EPOCH, makeFixtureSession, makeLongCells } from '../../src/testing/fixtures';
import { cellElement, disposeMounted, mountWebview, pushPatch } from './harness';

/**
 * The three interactions the user asked for (copy, open file, open diff) plus the
 * scroll contract.
 *
 * Every interaction is asserted on the *bridge message*: the renderer's only way
 * to affect the world is to ask the host, so the message is the observable
 * behaviour.
 */

afterEach(() => {
  disposeMounted();
});

const CODE = 'const answer = 42;';

function assistantWithCode(): string {
  return `Here is the plan:\n\n\`\`\`ts\n${CODE}\n\`\`\`\n`;
}

describe('code block', () => {
  it('copies the raw code (not the highlighted markup) through the host', () => {
    const { container, bridge } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'assistant',
            id: 'a1',
            createdAt: FIXTURE_EPOCH,
            text: assistantWithCode(),
            streaming: false,
          },
        ],
      }),
    ]);

    const code = within(cellElement(container, 'a1')).getByTestId('md-code');
    const copy = within(code.parentElement ?? container).getByRole('button', { name: 'Copy' });

    fireEvent.click(copy);

    expect(bridge.sentOfType('copyText')).toEqual([{ type: 'copyText', text: `${CODE}\n` }]);
    expect(copy).toHaveAttribute('data-copied', 'true');
    expect(copy.textContent).toBe('Copied');
  });

  it('renders highlighted tokens with per-theme custom properties', () => {
    const { container } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'assistant',
            id: 'a1',
            createdAt: FIXTURE_EPOCH,
            text: assistantWithCode(),
            streaming: false,
          },
        ],
      }),
    ]);

    const tokens = [...cellElement(container, 'a1').querySelectorAll('[class*="codeToken"]')];
    const keyword = tokens.find((token) => token.textContent?.trim() === 'const');
    expect(keyword?.getAttribute('style')).toContain('--shiki-light');
    expect(keyword?.getAttribute('style')).toContain('--shiki-dark');
  });
});

describe('file references', () => {
  it('asks the host to open a path from a tool call subject', () => {
    const { container, bridge } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'tool_call',
            id: 'tool-read',
            createdAt: FIXTURE_EPOCH,
            toolCallId: 'call-read',
            name: 'Read',
            status: 'success',
            display: { title: 'Read', subject: 'src/app/main.ts:42' },
            argsText: '',
            args: null,
            result: null,
            startedAt: FIXTURE_EPOCH,
            finishedAt: FIXTURE_EPOCH,
          },
        ],
      }),
    ]);

    const link = within(cellElement(container, 'tool-read')).getByText('src/app/main.ts');
    fireEvent.click(link);

    expect(bridge.sentOfType('openFile')).toEqual([{ type: 'openFile', path: 'src/app/main.ts', line: 42 }]);
  });

  it('treats `path:0` as a path without a line number', () => {
    const { container, bridge } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'tool_call',
            id: 'tool-zero',
            createdAt: FIXTURE_EPOCH,
            toolCallId: 'call-zero',
            name: 'Read',
            status: 'success',
            display: { title: 'Read', subject: 'src/app/main.ts:0' },
            argsText: '',
            args: null,
            result: null,
            startedAt: FIXTURE_EPOCH,
            finishedAt: FIXTURE_EPOCH,
          },
        ],
      }),
    ]);

    fireEvent.click(within(cellElement(container, 'tool-zero')).getByText('src/app/main.ts'));

    expect(bridge.sentOfType('openFile')).toEqual([
      { type: 'openFile', path: 'src/app/main.ts', line: null },
    ]);
  });

  it('does not turn a command subject into a link', () => {
    const { container } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'tool_call',
            id: 'tool-bash',
            createdAt: FIXTURE_EPOCH,
            toolCallId: 'call-bash',
            name: 'Bash',
            status: 'success',
            display: { title: 'Bash', subject: 'pnpm run test' },
            argsText: '',
            args: null,
            result: null,
            startedAt: FIXTURE_EPOCH,
            finishedAt: FIXTURE_EPOCH,
          },
        ],
      }),
    ]);

    expect(cellElement(container, 'tool-bash').querySelector('[data-file-path]')).toBeNull();
    expect(cellElement(container, 'tool-bash').textContent).toContain('pnpm run test');
  });

  it('opens the diff path from the diff card header', () => {
    const { container, bridge } = mountWebview([
      makeFixtureSession({ cells: [...makeFixtureSession().cells].filter((cell) => cell.kind === 'diff') }),
    ]);

    fireEvent.click(within(cellElement(container, 'diff-1')).getByText('src/webview/state/store.ts'));

    expect(bridge.sentOfType('openFile')).toEqual([
      { type: 'openFile', path: 'src/webview/state/store.ts', line: null },
    ]);
  });

  it('opens links through the host instead of navigating the webview', () => {
    const { container, bridge } = mountWebview([
      makeFixtureSession({
        cells: [
          {
            kind: 'assistant',
            id: 'a1',
            createdAt: FIXTURE_EPOCH,
            text: 'See [the docs](https://example.com/docs).',
            streaming: false,
          },
        ],
      }),
    ]);

    const link = within(cellElement(container, 'a1')).getByRole('link');
    fireEvent.click(link);

    expect(bridge.sentOfType('openLink')).toEqual([{ type: 'openLink', href: 'https://example.com/docs' }]);
  });
});

describe('transcript scrolling', () => {
  /** Give the jsdom element a scrollable geometry (jsdom does not lay out). */
  function makeScrollable(element: HTMLElement, scrollHeight = 1000, clientHeight = 300): void {
    Object.defineProperty(element, 'scrollHeight', { value: scrollHeight, configurable: true });
    Object.defineProperty(element, 'clientHeight', { value: clientHeight, configurable: true });
  }

  function mountLong() {
    const mounted = mountWebview([makeFixtureSession({ sessionId: 'session-a', cells: makeLongCells(5) })]);
    const transcript = mounted.container.querySelector<HTMLElement>('[data-testid="transcript"]');
    if (transcript === null) {
      throw new Error('no transcript');
    }
    makeScrollable(transcript);
    return { ...mounted, transcript };
  }

  it('follows new content while the user is at the bottom', () => {
    const mounted = mountLong();

    pushPatch(mounted, 'session-a', 1, [
      { op: 'append', cell: { kind: 'separator', id: 'sep-new', createdAt: FIXTURE_EPOCH, label: 'next' } },
    ]);

    expect(mounted.transcript.scrollTop).toBe(1000);
    expect(mounted.container.querySelector('[data-testid="scroll-to-bottom"]')).toBeNull();
  });

  it('unpins when the user scrolls up and does not steal the view back', () => {
    const mounted = mountLong();
    const { transcript } = mounted;

    transcript.scrollTop = 100;
    fireEvent.scroll(transcript);

    const button = mounted.container.querySelector<HTMLElement>('[data-testid="scroll-to-bottom"]');
    expect(button).not.toBeNull();

    pushPatch(mounted, 'session-a', 1, [
      {
        op: 'append',
        cell: { kind: 'assistant', id: 'a-new', createdAt: FIXTURE_EPOCH, text: 'more', streaming: false },
      },
    ]);

    expect(transcript.scrollTop).toBe(100);
    expect(mounted.container.querySelector('[data-testid="scroll-to-bottom"]')).not.toBeNull();
  });

  it('re-pins and jumps to the end when the button is used', () => {
    const mounted = mountLong();
    const { transcript } = mounted;

    transcript.scrollTop = 100;
    fireEvent.scroll(transcript);

    fireEvent.click(mounted.container.querySelector('[data-testid="scroll-to-bottom"]') as HTMLElement);

    expect(transcript.scrollTop).toBe(1000);
    expect(mounted.container.querySelector('[data-testid="scroll-to-bottom"]')).toBeNull();

    pushPatch(mounted, 'session-a', 1, [
      {
        op: 'append',
        cell: { kind: 'assistant', id: 'a-new', createdAt: FIXTURE_EPOCH, text: 'more', streaming: false },
      },
    ]);
    expect(transcript.scrollTop).toBe(1000);
  });

  /**
   * The "follow" state must survive height changes that do not come with a host
   * patch: a cell expanding (the user opening a collapsed tool call) and a
   * streaming block re-laying out after it collapses (checkpoint② #4).
   */
  describe('follows content height changes (no patch involved)', () => {
    /** Scripted `ResizeObserver`: the test decides when a resize is delivered. */
    class FakeResizeObserver {
      static instances: FakeResizeObserver[] = [];
      readonly targets: Element[] = [];
      constructor(private readonly callback: () => void) {
        FakeResizeObserver.instances.push(this);
      }
      observe(target: Element): void {
        this.targets.push(target);
      }
      disconnect(): void {
        this.targets.length = 0;
      }
      trigger(): void {
        this.callback();
      }
    }

    function withObserver<T>(run: () => T): T {
      const previous = (globalThis as { ResizeObserver?: unknown }).ResizeObserver;
      (globalThis as { ResizeObserver?: unknown }).ResizeObserver = FakeResizeObserver;
      FakeResizeObserver.instances = [];
      try {
        return run();
      } finally {
        (globalThis as { ResizeObserver?: unknown }).ResizeObserver = previous;
      }
    }

    function lastObserver(): FakeResizeObserver {
      const observer = FakeResizeObserver.instances.at(-1);
      if (observer === undefined) {
        throw new Error('no ResizeObserver was created');
      }
      return observer;
    }

    it('observes the content wrapper, not the scroller', () => {
      withObserver(() => {
        const mounted = mountLong();
        const content = mounted.container.querySelector('[data-testid="transcript-content"]');
        expect(lastObserver().targets).toEqual([content]);
      });
    });

    it('stays pinned when the content grows without a patch', () => {
      withObserver(() => {
        const mounted = mountLong();
        const { transcript } = mounted;
        // Pin first (jsdom's first paint has no geometry): a patch is the only
        // signal the initial mount ever gets.
        pushPatch(mounted, 'session-a', 1, [
          { op: 'append', cell: { kind: 'separator', id: 'sep-pin', createdAt: FIXTURE_EPOCH, label: '' } },
        ]);
        expect(transcript.scrollTop).toBe(1000);

        // The cell list grew (an expanded tool call); the host sent nothing.
        Object.defineProperty(transcript, 'scrollHeight', { value: 1600, configurable: true });
        lastObserver().trigger();

        expect(transcript.scrollTop).toBe(1600);
        expect(mounted.container.querySelector('[data-testid="scroll-to-bottom"]')).toBeNull();
      });
    });

    it('shrinks gracefully when a collapsing block shortens the content', () => {
      withObserver(() => {
        const mounted = mountLong();
        const { transcript } = mounted;

        // A streaming thinking block collapses at turn end: the content gets
        // shorter, and the browser's own anchoring is off (CSS) so we own the
        // position. Following stays at the bottom.
        Object.defineProperty(transcript, 'scrollHeight', { value: 700, configurable: true });
        lastObserver().trigger();

        expect(transcript.scrollTop).toBe(700);
      });
    });

    it('never steals the view back while the user is reading above', () => {
      withObserver(() => {
        const mounted = mountLong();
        const { transcript } = mounted;

        transcript.scrollTop = 100;
        fireEvent.scroll(transcript);
        expect(mounted.container.querySelector('[data-testid="scroll-to-bottom"]')).not.toBeNull();

        Object.defineProperty(transcript, 'scrollHeight', { value: 1600, configurable: true });
        lastObserver().trigger();

        expect(transcript.scrollTop).toBe(100);
      });
    });
  });
});
