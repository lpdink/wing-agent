/**
 * React bindings for the framework-free runtime.
 *
 * One hook, one rule: the renderer never holds a second copy of the state. React
 * subscribes to the runtime's snapshot (a new object identity exactly when
 * something changed) and re-renders from it — the same shape the VS Code webview
 * gets from the bridge, minus the bridge (`design.md` D10).
 */

import { useSyncExternalStore } from 'react';

import type { GatewayRuntime, RuntimeSnapshot } from '../connection/runtime';

/** The current snapshot; re-renders on every runtime change. */
export function useRuntimeSnapshot(runtime: GatewayRuntime): RuntimeSnapshot {
  return useSyncExternalStore(runtime.subscribe, runtime.getSnapshot);
}
