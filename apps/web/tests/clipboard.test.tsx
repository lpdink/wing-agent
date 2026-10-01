import { afterEach, describe, expect, it, vi } from 'vitest';

import { copyTextToClipboard, type ClipboardDocument } from '../src/bridge/clipboard';
import { createWebBridge } from '../src/bridge/webBridge';

/**
 * Copying, including the path a non-secure context needs.
 *
 * The renderer's copy buttons post `copyText` and always show "Copied" (their feedback
 * is fire-and-forget), so the host side is where "did it actually work" is decided —
 * and where the user is told when it did not. jsdom has no `execCommand`, which is
 * exactly why the fallback takes its document as an argument.
 */

/** The throwaway textarea the fallback creates, as the test sees it. */
interface TextField {
  value: string;
  readonly style: Record<string, string>;
  removed: boolean;
  setAttribute(name: string, value: string): void;
  select(): void;
}

/** A `Document` stand-in that records what the fallback did to it. */
function fakeDocument(options: { readonly exec?: (command: string) => boolean } = {}): {
  readonly document: ClipboardDocument;
  readonly fields: TextField[];
  readonly commands: string[];
} {
  const fields: TextField[] = [];
  const commands: string[] = [];
  const document: ClipboardDocument = {
    execCommand: (command) => {
      commands.push(command);
      return options.exec?.(command) ?? true;
    },
    createElement: () => {
      const field: TextField = {
        value: '',
        style: {},
        removed: false,
        setAttribute: () => undefined,
        select: () => undefined,
      };
      fields.push(field);
      return field;
    },
    body: {
      appendChild: () => undefined,
      removeChild: (node) => {
        // `node` is what `createElement` handed out (`never` here only because the seam
        // avoids depending on the DOM types).
        const field = fields.find((candidate) => candidate === node);
        if (field !== undefined) {
          field.removed = true;
        }
      },
    },
  };
  return { document, fields, commands };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe('copyTextToClipboard', () => {
  it('uses the async clipboard API when the page has one', async () => {
    const writeText = vi.fn(() => Promise.resolve());
    const { document, commands } = fakeDocument();

    await expect(copyTextToClipboard('hello', { clipboard: { writeText }, document })).resolves.toBe(
      'clipboard-api',
    );
    expect(writeText).toHaveBeenCalledWith('hello');
    expect(commands).toEqual([]); // the fallback stayed out of it
  });

  it('falls back to a throwaway textarea when the clipboard API refuses', async () => {
    const writeText = vi.fn(() => Promise.reject(new Error('permission denied')));
    const { document, fields, commands } = fakeDocument();

    await expect(copyTextToClipboard('multi\nline ✓', { clipboard: { writeText }, document })).resolves.toBe(
      'exec-command',
    );
    expect(commands).toEqual(['copy']);
    expect(fields).toHaveLength(1);
    expect(fields[0]?.value).toBe('multi\nline ✓');
    // Off-screen and cleaned up: the copy must not scroll the transcript or leave nodes.
    expect(fields[0]?.style['position']).toBe('fixed');
    expect(fields[0]?.removed).toBe(true);
  });

  it('falls back when there is no clipboard API at all (http on a LAN)', async () => {
    const { document } = fakeDocument();

    await expect(copyTextToClipboard('hi', { clipboard: null, document })).resolves.toBe('exec-command');
  });

  it('reports a failure the caller can tell the user about', async () => {
    const denied = fakeDocument({ exec: () => false });
    await expect(copyTextToClipboard('hi', { clipboard: null, document: denied.document })).resolves.toBe(
      'failed',
    );
    // No document at all (a non-DOM environment) is the same outcome.
    await expect(copyTextToClipboard('hi', { clipboard: null })).resolves.toBe('failed');
    expect(denied.fields[0]?.removed).toBe(true); // even a failed attempt cleans up
  });
});

describe('the bridge’s copy intent', () => {
  it('tells the user when neither copy path worked', async () => {
    // jsdom has no `navigator.clipboard` and no `document.execCommand`: this is the
    // "the page cannot copy" state, and the renderer's own "Copied" label lies about it.
    const notify = vi.fn();
    const bridge = createWebBridge({
      host: {
        answerAsk: vi.fn(),
        approveTool: vi.fn(),
        notify,
        sendMessage: vi.fn(),
        interrupt: vi.fn(),
        setModel: vi.fn(),
        setThinking: vi.fn(),
        setEffort: vi.fn(),
        setYolo: vi.fn(),
        runPromptCommand: vi.fn(),
        openModelPickerAction: vi.fn(),
        closeOverlays: vi.fn(),
        compact: vi.fn(),
      },
      images: { resolve: () => Promise.resolve([]), clear: () => undefined },
      logger: { debug: vi.fn(), warn: vi.fn(), error: vi.fn() },
    });

    bridge.post({ type: 'copyText', text: 'echo hello' });
    await vi.waitFor(() => {
      expect(notify).toHaveBeenCalledWith(
        'warning',
        'Could not copy — select the text and copy it manually.',
      );
    });
  });
});
