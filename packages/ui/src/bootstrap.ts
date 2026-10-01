import type { BootstrapModel } from './protocol';
import { BRIDGE_PROTOCOL_VERSION, FALLBACK_BOOTSTRAP } from './protocol';

/**
 * Reads `window.__WING_BOOTSTRAP__` (injected by the embedding host — the VS Code
 * extension writes it into the document in its `src/host/html.ts`).
 *
 * A harness or shell that injects nothing degrades to {@link FALLBACK_BOOTSTRAP} —
 * the renderer must never depend on bootstrap data to boot. A protocol mismatch is a
 * warning, not a crash: the host is the one that decides whether to keep talking to
 * this document.
 */
export function readBootstrap(scope: { __WING_BOOTSTRAP__?: unknown } = window): BootstrapModel {
  const raw = scope.__WING_BOOTSTRAP__;
  if (!isBootstrap(raw)) {
    console.warn('[wing] no valid bootstrap injected — using defaults');
    return FALLBACK_BOOTSTRAP;
  }
  if (raw.protocolVersion !== BRIDGE_PROTOCOL_VERSION) {
    console.warn(
      `[wing] bridge protocol mismatch: document v${raw.protocolVersion}, bundle v${BRIDGE_PROTOCOL_VERSION}`,
    );
  }
  return raw;
}

function isBootstrap(value: unknown): value is BootstrapModel {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const record = value as Record<string, unknown>;
  return typeof record['protocolVersion'] === 'number' && typeof record['assetUris'] === 'object';
}
