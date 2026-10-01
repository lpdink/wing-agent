import type { WebviewToHostMessage } from '../protocol';

import type { BridgeController } from './controller';

/**
 * The mounted controller, as seen by components.
 *
 * Components ask for things ("send this message", "probe the channel") without
 * knowing which host they run in — every consumer mounts a controller here. Before
 * mount (and in isolated component tests) calls are no-ops, so a component never has
 * to null-check the bridge. The host side is injected through `mountApp`.
 */

let active: BridgeController | null = null;

/** Called by `mount.tsx` (production and preview). */
export function setBridgeController(controller: BridgeController | null): void {
  active = controller;
}

/** Post one message to the host. */
export function postToHost(message: WebviewToHostMessage): void {
  active?.post(message);
}

/** Round-trip probe; the result lands in `state.bridge.lastPong`. */
export function pingHost(): void {
  active?.ping();
}
