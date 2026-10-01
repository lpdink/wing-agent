import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  GatewayLauncher,
  PROBE_TIMEOUT_MS,
  healthUrl,
  isLocalGatewayUrl,
  findWingExecutable,
  summarize,
  type GatewayLaunchSettings,
} from '../src/gateway/launcher';

/**
 * Ported invariants (see `extensions/vscode/tests/host/launcher.test.ts`):
 * **probe first, never restart a gateway the user is using**, start at most once,
 * fail with something actionable. Plus the desktop-only rule: a non-local
 * gateway is never started.
 */

const settings: GatewayLaunchSettings = {
  baseUrl: 'http://127.0.0.1:32523',
  wingPath: null,
  autoStart: true,
};

const temps: string[] = [];
afterEach(() => {
  while (temps.length > 0) {
    const directory = temps.pop();
    if (directory !== undefined) {
      rmSync(directory, { recursive: true, force: true });
    }
  }
});

function tempDirWithWing(): string {
  const directory = mkdtempSync(path.join(tmpdir(), 'wing-desktop-launcher-'));
  temps.push(directory);
  const executable = path.join(directory, 'wing');
  writeFileSync(executable, '#!/bin/sh\nexit 0\n');
  chmodSync(executable, 0o755);
  return directory;
}

describe('healthUrl', () => {
  it('appends the health path without doubling slashes', () => {
    expect(healthUrl('http://127.0.0.1:32523')).toBe('http://127.0.0.1:32523/api/health');
    expect(healthUrl('http://127.0.0.1:32523/')).toBe('http://127.0.0.1:32523/api/health');
    expect(healthUrl('https://wing.local:8443/prefix/')).toBe('https://wing.local:8443/prefix/api/health');
  });
});

describe('isLocalGatewayUrl', () => {
  it('accepts loopback hosts only', () => {
    for (const url of [
      'http://127.0.0.1:32523',
      'http://127.5.5.5:32523',
      'https://localhost:8443',
      'https://[::1]:8443',
      'http://dev.localhost:32523',
    ]) {
      expect(isLocalGatewayUrl(url)).toBe(true);
    }
  });

  it('rejects anything that is not this machine', () => {
    for (const url of [
      'https://192.168.1.20:32523',
      'https://wing.example.com',
      'http://10.0.0.1:32523',
      'http://127.0.0.1.evil.test',
      'not a url',
    ]) {
      expect(isLocalGatewayUrl(url)).toBe(false);
    }
  });
});

describe('findWingExecutable', () => {
  it('prefers the explicit path and rejects a broken one', () => {
    const directory = tempDirWithWing();
    expect(findWingExecutable(path.join(directory, 'wing'))).toBe(path.join(directory, 'wing'));
    expect(findWingExecutable(path.join(directory, 'missing'))).toBeNull();
  });

  it('finds `wing` on PATH when no explicit path is set', () => {
    const directory = tempDirWithWing();
    const previousHome = process.env['HOME'];
    // Well-known locations are searched before PATH — isolate HOME so this asserts
    // the PATH rule, not whatever the developer machine has installed.
    process.env['HOME'] = directory;
    try {
      expect(findWingExecutable(null, { PATH: directory })).toBe(path.join(directory, 'wing'));
    } finally {
      process.env['HOME'] = previousHome;
    }
  });

  it('returns null when nothing matches (no silent spawn attempts)', () => {
    const directory = mkdtempSync(path.join(tmpdir(), 'wing-desktop-launcher-empty-'));
    temps.push(directory);
    const previousHome = process.env['HOME'];
    process.env['HOME'] = directory;
    try {
      expect(findWingExecutable(null, { PATH: directory })).toBeNull();
    } finally {
      process.env['HOME'] = previousHome;
    }
  });
});

describe('GatewayLauncher.ensureRunning', () => {
  it('does nothing when the gateway already answers the probe', async () => {
    const run = vi.fn();
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(true),
      run,
      findExecutable: () => '/fake/wing',
      sleep: () => Promise.resolve(),
    });

    await expect(launcher.ensureRunning(settings)).resolves.toEqual({ status: 'already-running' });
    expect(run).not.toHaveBeenCalled();
  });

  it('runs `wing start` once and waits for the probe to succeed', async () => {
    const run = vi.fn().mockResolvedValue({ code: 0, output: 'gateway ready' });
    let probes = 0;
    const launcher = new GatewayLauncher({
      probe: () => {
        probes += 1;
        return Promise.resolve(probes > 2);
      },
      run,
      findExecutable: () => '/fake/wing',
      sleep: () => Promise.resolve(),
    });

    await expect(launcher.ensureRunning(settings)).resolves.toEqual({ status: 'started' });
    expect(run).toHaveBeenCalledTimes(1);
    expect(run).toHaveBeenCalledWith('/fake/wing', ['start']);
  });

  it('probes with the configured base URL and the documented deadline', async () => {
    const probe = vi.fn().mockResolvedValue(true);
    const launcher = new GatewayLauncher({ probe, run: vi.fn() });

    await launcher.ensureRunning({ ...settings, baseUrl: 'https://wing.local:8443/' });
    expect(probe).toHaveBeenCalledWith('https://wing.local:8443/', PROBE_TIMEOUT_MS);
  });

  it('reports auto-start being disabled without spawning anything', async () => {
    const run = vi.fn();
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(false),
      run,
      findExecutable: () => '/fake/wing',
    });

    await expect(launcher.ensureRunning({ ...settings, autoStart: false })).resolves.toEqual({
      status: 'disabled',
    });
    expect(run).not.toHaveBeenCalled();
  });

  it('never starts a gateway that is not on this machine', async () => {
    const run = vi.fn();
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(false),
      run,
      findExecutable: () => '/fake/wing',
    });

    await expect(
      launcher.ensureRunning({ ...settings, baseUrl: 'https://wing.example.com' }),
    ).resolves.toEqual({ status: 'remote' });
    expect(run).not.toHaveBeenCalled();
  });

  it('reports a missing executable without spawning anything', async () => {
    const run = vi.fn();
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(false),
      run,
      findExecutable: () => null,
    });

    await expect(launcher.ensureRunning(settings)).resolves.toEqual({ status: 'not-found' });
    expect(run).not.toHaveBeenCalled();
  });

  it('passes the configured wing path to discovery', async () => {
    const findExecutable = vi.fn().mockReturnValue(null);
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(false),
      run: vi.fn(),
      findExecutable,
    });

    await launcher.ensureRunning({ ...settings, wingPath: '/opt/wing' });
    expect(findExecutable).toHaveBeenCalledWith('/opt/wing');
  });

  it('surfaces the CLI output when `wing start` fails', async () => {
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(false),
      run: () => Promise.resolve({ code: 1, output: 'Port 32523 is already in use by another process' }),
      findExecutable: () => '/fake/wing',
      sleep: () => Promise.resolve(),
    });

    const outcome = await launcher.ensureRunning(settings);
    expect(outcome.status).toBe('failed');
    if (outcome.status === 'failed') {
      expect(outcome.detail).toContain('Port 32523 is already in use');
    }
  });

  it('reports a gateway that never answers after a successful CLI run', async () => {
    const launcher = new GatewayLauncher({
      probe: () => Promise.resolve(false),
      run: () => Promise.resolve({ code: 0, output: '' }),
      findExecutable: () => '/fake/wing',
      sleep: () => Promise.resolve(),
    });

    const outcome = await launcher.ensureRunning(settings);
    expect(outcome.status).toBe('failed');
    if (outcome.status === 'failed') {
      expect(outcome.detail).toContain('did not answer /api/health');
    }
  });

  it('reports the probe result through isRunning', async () => {
    const healthy = new GatewayLauncher({ probe: () => Promise.resolve(true) });
    const dead = new GatewayLauncher({ probe: () => Promise.resolve(false) });
    await expect(healthy.isRunning(settings.baseUrl)).resolves.toBe(true);
    await expect(dead.isRunning(settings.baseUrl)).resolves.toBe(false);
  });
});

describe('summarize', () => {
  it('keeps the last non-empty lines', () => {
    expect(summarize('line 1\n\n line 2 \nline 3\n')).toBe('line 1 | line 2 | line 3');
  });

  it('truncates to the last four lines', () => {
    expect(summarize('1\n2\n3\n4\n5\n')).toBe('2 | 3 | 4 | 5');
  });
});
