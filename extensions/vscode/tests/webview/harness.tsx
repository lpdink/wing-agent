import { act } from '@testing-library/react';
import type {
  CellPatch,
  PanelsModel,
  SessionStateModel,
  SessionViewModel,
  TabModel,
  UiActionModel,
} from '../../src/shared';
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

/** Push overlay data as the host would (`panels`). */
export function pushPanels(item: Mounted, sessionId: string, panels: PanelsModel): void {
  act(() => {
    item.bridge.push({ type: 'panels', sessionId, panels });
  });
}

/** Push a one-shot UI action as the host would (`ui`). */
export function pushUi(item: Mounted, action: UiActionModel): void {
  act(() => {
    item.bridge.push({ type: 'ui', action });
  });
}

/** Push a full state replacement as the host would (`state`). */
export function pushState(item: Mounted, state: SessionStateModel): void {
  act(() => {
    item.bridge.push({ type: 'state', state });
  });
}

/** Push a tab-bar update as the host would (`tabs`). */
export function pushTabs(item: Mounted, tabs: readonly TabModel[], activeSessionId: string | null): void {
  act(() => {
    item.bridge.push({ type: 'tabs', tabs, activeSessionId });
  });
}

/** The two-tab fixture every tab-bar test starts from. */
export function twoTabs(): readonly TabModel[] {
  return [
    { sessionId: 'session-a', title: 'Shell fixture', status: 'idle', attention: 'none' },
    { sessionId: 'session-b', title: 'New session', status: 'idle', attention: 'none' },
  ];
}

/** Push a full snapshot as the host would (`hydrate`). */
export function pushHydrate(item: Mounted, session: SessionViewModel): void {
  act(() => {
    item.bridge.push({ type: 'hydrate', session });
  });
}
