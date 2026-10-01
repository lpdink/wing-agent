import { act, fireEvent, render } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import { RESOLVE_IMAGES_MAX_SRCS } from '../../src/protocol';
import { FIXTURE_EPOCH, makeFixtureSession } from '../../src/testing/fixtures';

import { createMockBridge } from '../../src/testing/mockBridge';
import type { MockBridgeOptions } from '../../src/testing/mockBridge';
import { MarkdownText } from '../../src/chat/Markdown';
import { setBridgeController } from '../../src/bridge/channel';
import { createBridgeController } from '../../src/bridge/controller';
import { imageUri } from '../../src/chat/markdown/image';
import { createAppStore } from '../../src/state/store';

import { cellElement, disposeMounted, mountWebview } from './harness';

/**
 * Images in the transcript.
 *
 * The renderer owns exactly one decision — *link or image* — and the host owns the
 * path policy (`tests/host/images.test.ts`). What is asserted here is the seam:
 * one batched request per paint, the image once a URI arrives, and the link in
 * every other case (refused, unanswered, load failure) — the behaviour the
 * transcript always had.
 */

/**
 * Put a controller on the channel, the way a mounted app does.
 *
 * `postToHost` is a no-op without one, and the renderer needs nothing more from
 * the shell than that — mounting the whole app would only add unrelated fixtures.
 * `start()` is what subscribes to the host (and announces `ready`), exactly as
 * `mountApp` does it.
 */
function connectHost(options: MockBridgeOptions = {}): ReturnType<typeof createMockBridge> {
  const bridge = createMockBridge({ now: () => 0, ...options });
  const controller = createBridgeController({ transport: bridge.transport, store: createAppStore() });
  setBridgeController(controller);
  controller.start();
  return bridge;
}

/** Let the batched request flush and the host answer (both are microtasks). */
async function flush(): Promise<void> {
  await act(async () => {
    await Promise.resolve();
  });
}

describe('MarkdownImage', () => {
  it('asks the host once and renders the image it answers with', async () => {
    const bridge = connectHost();
    const { container } = render(<MarkdownText text={'![the plot](docs/plot.png)'} />);

    // Before the answer the renderer shows what it always showed: a link.
    expect(container.querySelector('[data-testid="md-image"]')).toBeNull();
    expect(container.querySelector('a')?.getAttribute('href')).toBe('docs/plot.png');

    await flush();

    const image = container.querySelector('[data-testid="md-image"]');
    expect(image?.getAttribute('src')).toBe('mock-resource://workspace/docs/plot.png');
    expect(image?.getAttribute('alt')).toBe('the plot');
    expect(container.querySelector('a')).toBeNull();
    expect(bridge.sentOfType('resolveImages')).toEqual([{ type: 'resolveImages', srcs: ['docs/plot.png'] }]);
  });

  it('sends one request per paint, not one per image', async () => {
    const bridge = connectHost();

    render(<MarkdownText text={'![a](a.png) ![b](b.png)\n\n![c](c.png)'} />);
    await flush();

    expect(bridge.sentOfType('resolveImages')).toEqual([
      { type: 'resolveImages', srcs: ['a.png', 'b.png', 'c.png'] },
    ]);
  });

  it('asks once per distinct source, however often it appears', async () => {
    const bridge = connectHost();

    render(<MarkdownText text={'![a](plot.png)\n\n![b](plot.png)'} />);
    await flush();

    expect(bridge.sentOfType('resolveImages')).toEqual([{ type: 'resolveImages', srcs: ['plot.png'] }]);
    expect(bridge.sentOfType('resolveImages')).toHaveLength(1);
  });

  it('sends follow-up requests when a paint carries more images than one may hold', async () => {
    const bridge = connectHost();
    const total = RESOLVE_IMAGES_MAX_SRCS + 3;
    const images = Array.from({ length: total }, (_value, index) => `![x](${index}.png)`);

    render(<MarkdownText text={images.join(' ')} />);
    await flush();

    // The host walks the list, so the protocol caps one request (see
    // `RESOLVE_IMAGES_MAX_SRCS`); the renderer must not lose the tail.
    const requests = bridge.sentOfType('resolveImages');
    expect(requests.map((request) => request.srcs.length)).toEqual([RESOLVE_IMAGES_MAX_SRCS, 3]);
    expect(requests[0]?.srcs[0]).toBe('0.png');
    expect(requests[1]?.srcs[2]).toBe(`${total - 1}.png`);
    expect(imageUri(`${total - 1}.png`)).toBe(`mock-resource://workspace/${total - 1}.png`);
  });

  it('keeps the link when the host refuses the source', async () => {
    connectHost({ imageUri: () => null });
    const { container } = render(<MarkdownText text={'![photo](https://example.com/x.png)'} />);

    await flush();

    expect(container.querySelector('[data-testid="md-image"]')).toBeNull();
    expect(container.querySelector('a')?.getAttribute('href')).toBe('https://example.com/x.png');
    expect(container.querySelector('a')?.textContent).toBe('photo');
  });

  it('keeps the link when the host never answers', async () => {
    const bridge = connectHost({ autoHandshake: false });
    const { container } = render(<MarkdownText text={'![alt](plot.png)'} />);

    await flush();

    expect(container.querySelector('[data-testid="md-image"]')).toBeNull();
    expect(container.querySelector('a')?.getAttribute('href')).toBe('plot.png');
    // …and the request was still made (the handshake only silences the answer).
    expect(bridge.sentOfType('resolveImages')).toHaveLength(1);
  });

  it('falls back to the link when the file cannot be loaded', async () => {
    connectHost();
    const { container } = render(<MarkdownText text={'![plot](plot.png)'} />);
    await flush();
    const image = container.querySelector('[data-testid="md-image"]');
    expect(image).not.toBeNull();

    // What a missing file, or a file that is not really an image, looks like: the
    // browser reports a load error and the renderer goes back to the link.
    fireEvent.error(image as Element);

    expect(container.querySelector('[data-testid="md-image"]')).toBeNull();
    expect(container.querySelector('a')?.getAttribute('href')).toBe('plot.png');
    expect(container.querySelector('a')?.textContent).toBe('plot');
  });

  it('forgets a load failure when the node is handed a different source', async () => {
    connectHost();
    const { container, rerender } = render(<MarkdownText text={'![plot](first.png)'} />);
    await flush();
    fireEvent.error(container.querySelector('[data-testid="md-image"]') as Element);
    expect(container.querySelector('[data-testid="md-image"]')).toBeNull();

    // Streaming can shift inline nodes, so the same component instance may end up
    // rendering a different image: a remembered failure must not stick to it.
    rerender(<MarkdownText text={'![plot](second.png)'} />);
    await flush();

    expect(container.querySelector('[data-testid="md-image"]')?.getAttribute('src')).toBe(
      'mock-resource://workspace/second.png',
    );
  });

  it('labels the fallback link with the path when there is no alt text', () => {
    const { container } = render(<MarkdownText text={'![](bare.png)'} />);

    expect(container.querySelector('a')?.textContent).toBe('bare.png');
  });

  it('hands a click on the fallback link to the editor', async () => {
    const bridge = connectHost({ imageUri: () => null });
    const { container } = render(<MarkdownText text={'![plot](plot.png)'} />);
    await flush();

    fireEvent.click(container.querySelector('a') as Element);

    expect(bridge.sentOfType('openLink')).toEqual([{ type: 'openLink', href: 'plot.png' }]);
  });

  it('asks about nothing when there is no image', async () => {
    const bridge = connectHost();

    render(<MarkdownText text={'just text, and no `![x](y.png)` in code either'} />);
    await flush();

    expect(bridge.sentOfType('resolveImages')).toHaveLength(0);
  });
});

/**
 * The mounted path: transcript → assistant cell → streaming markdown → image.
 *
 * The component tests above render the markdown stage directly; this one exists
 * because the transcript renders through `MarkdownStream` (block splitting), which
 * is a different entry point into the same renderer.
 */
describe('images in the transcript', () => {
  afterEach(() => {
    disposeMounted();
  });

  it('renders a formula and a workspace image inside a mounted assistant cell', async () => {
    const session = makeFixtureSession({
      cells: [
        {
          kind: 'assistant',
          id: 'assistant-media',
          createdAt: FIXTURE_EPOCH,
          streaming: false,
          text: 'The area is $\\pi r^2$:\n\n![plot](docs/plot.png)\n',
        },
      ],
    });

    const mounted = mountWebview([session]);
    await flush();

    const cell = cellElement(mounted.container, 'assistant-media');
    expect(cell.querySelector('[data-testid="md-math"] .katex')).not.toBeNull();
    expect(cell.querySelector('[data-testid="md-image"]')?.getAttribute('src')).toBe(
      'mock-resource://workspace/docs/plot.png',
    );
    expect(cell.querySelector('[data-testid="md-image"]')?.getAttribute('alt')).toBe('plot');
  });
});
