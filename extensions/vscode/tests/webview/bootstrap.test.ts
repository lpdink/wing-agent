import { describe, expect, it, vi } from 'vitest';

import { BRIDGE_PROTOCOL_VERSION, FALLBACK_BOOTSTRAP } from '../../src/shared';
import { readBootstrap } from '../../src/webview/bootstrap';

/**
 * Bootstrap is the only data that arrives *outside* the bridge (an inline script
 * in the generated document), so it is verified separately: a missing or malformed
 * value must degrade to defaults, never throw.
 */
describe('readBootstrap', () => {
  it('returns the injected payload', () => {
    const bootstrap = readBootstrap({
      __WING_BOOTSTRAP__: {
        protocolVersion: BRIDGE_PROTOCOL_VERSION,
        assetUris: { icon: 'vscode-resource://x.svg' },
      },
    });

    expect(bootstrap.protocolVersion).toBe(BRIDGE_PROTOCOL_VERSION);
    expect(bootstrap.assetUris['icon']).toBe('vscode-resource://x.svg');
  });

  it('warns and falls back when nothing was injected', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    const bootstrap = readBootstrap({});

    expect(bootstrap).toBe(FALLBACK_BOOTSTRAP);
    expect(warn).toHaveBeenCalledWith(expect.stringContaining('no valid bootstrap'));
    warn.mockRestore();
  });

  it('warns on a protocol mismatch but still returns the payload', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    const bootstrap = readBootstrap({ __WING_BOOTSTRAP__: { protocolVersion: 99, assetUris: {} } });

    expect(bootstrap.protocolVersion).toBe(99);
    expect(warn).toHaveBeenCalledWith(expect.stringContaining('protocol mismatch'));
    warn.mockRestore();
  });

  it('rejects malformed payloads', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    expect(readBootstrap({ __WING_BOOTSTRAP__: 'nope' }).assetUris).toEqual({});
    expect(readBootstrap({ __WING_BOOTSTRAP__: { assetUris: {} } }).protocolVersion).toBe(
      BRIDGE_PROTOCOL_VERSION,
    );
    expect(readBootstrap({ __WING_BOOTSTRAP__: null }).assetUris).toEqual({});
    expect(warn).toHaveBeenCalledTimes(3);
    warn.mockRestore();
  });
});
