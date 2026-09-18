import { act } from '@testing-library/react';
import type { CellPatch, SessionViewModel } from '../../src/shared';
import { createMockBridge } from '../../src/testing/mockBridge';
import type { MockBridge } from '../../src/testing/mockBridge';
import { mountApp } from '../../src/webview/mount';

/**
 * Shared mount helper for the webview tests.
 *
 * Mounts the real app against a scripted host — that is the whole point of the
 * bridge abstraction: the renderer is exercised exactly as it runs in the
 * sidebar, without VS Code.
 */

/**
 * Fixed clock for both sides of the ping round-trip.
 *
 * The bridge measures latency itself (`now()` before send, `now()` after the
 * pong), so the test must inject the *controller's* clock too — sampling the real
 * wall clock made the RTT assertion flaky (review r1 [B1] of step 01).
 */
export const FIXED_CLOCK = 1_000;

export interface Mounted {
  readonly container: HTMLElement;
  readonly bridge: MockBridge;
  dispose(): void;
}

const mounted: Mounted[] = [];

export interface MountOptions {
  /** Answer the handshake automatically (default: true). */
  readonly autoHandshake?: boolean;
}

/** Mount the real app against a scripted host and register it for cleanup. */
export function mountWebview(
  sessions: readonly SessionViewModel[] = [],
  options: MountOptions = {},
): Mounted {
  const container = document.createElement('div');
  document.body.append(container);
  const bridge = createMockBridge({
    sessions,
    now: () => FIXED_CLOCK,
    ...(options.autoHandshake === undefined ? {} : { autoHandshake: options.autoHandshake }),
  });
  let app: { dispose(): void } | undefined;
  act(() => {
    app = mountApp(container, { transport: bridge.transport, now: () => FIXED_CLOCK });
  });
  const item: Mounted = {
    container,
    bridge,
    dispose: () => {
      act(() => {
        app?.dispose();
      });
      container.remove();
    },
  };
  mounted.push(item);
  return item;
}

/** Dispose everything mounted by the current test. */
export function disposeMounted(): void {
  for (const item of mounted.splice(0)) {
    item.dispose();
  }
}

/** Push one ordered patch batch as the host would. */
export function pushPatch(
  item: Mounted,
  sessionId: string,
  seq: number,
  patches: readonly CellPatch[],
): void {
  act(() => {
    item.bridge.push({ type: 'patch', sessionId, seq, patches });
  });
}

/** Look up a cell's root element by cell id. */
export function cellElement(container: HTMLElement, cellId: string): HTMLElement {
  const element = container.querySelector<HTMLElement>(`[data-cell-id="${cellId}"]`);
  if (element === null) {
    throw new Error(`no cell rendered for id ${cellId}`);
  }
  return element;
}
