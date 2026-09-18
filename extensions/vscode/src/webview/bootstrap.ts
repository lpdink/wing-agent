import type { BootstrapModel } from '../shared';
import { BRIDGE_PROTOCOL_VERSION, FALLBACK_BOOTSTRAP } from '../shared';

/**
 * Reads `window.__WING_BOOTSTRAP__` (injected by `src/host/html.ts`).
 *
 * The preview harness has no host, so a missing/invalid value degrades to
 * {@link FALLBACK_BOOTSTRAP} — the renderer must never depend on bootstrap data to
 * boot. A protocol mismatch is a warning, not a crash: the host is the one that
 * decides whether to keep talking to this document.
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
