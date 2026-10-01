import { describe, expect, it } from 'vitest';

import { BRIDGE_KEY, IPC_CHANNELS, parseDesktopArgs } from '../src/bridge';

/**
 * The preload contract is frozen for step 10 (the web shell consumes it through
 * `@wing-agent/desktop/bridge`), so the key, the channel names and the argument
 * parsing are asserted rather than assumed.
 */

describe('the bridge contract', () => {
  it('publishes under one documented window key', () => {
    expect(BRIDGE_KEY).toBe('wingDesktop');
  });

  it('namespaces every channel and keeps them unique', () => {
    const channels = Object.values(IPC_CHANNELS);
    expect(channels.length).toBe(new Set(channels).size);
    for (const channel of channels) {
      expect(channel.startsWith('wing:')).toBe(true);
    }
    expect(IPC_CHANNELS.settingsWrite).toBe('wing:settings-write');
  });
});

describe('parseDesktopArgs', () => {
  it('recognizes --smoke wherever it appears', () => {
    expect(parseDesktopArgs([])).toEqual({ smoke: false });
    expect(parseDesktopArgs(['.'])).toEqual({ smoke: false });
    expect(parseDesktopArgs(['.', '--smoke'])).toEqual({ smoke: true });
    expect(parseDesktopArgs(['--smoke', '--user-data-dir=/tmp/x'])).toEqual({ smoke: true });
  });

  it('leaves Chromium switches alone', () => {
    const argv = ['--user-data-dir=/tmp/x', '--remote-debugging-port=9222'];
    expect(parseDesktopArgs(argv)).toEqual({ smoke: false });
    expect(argv).toEqual(['--user-data-dir=/tmp/x', '--remote-debugging-port=9222']);
  });
});
