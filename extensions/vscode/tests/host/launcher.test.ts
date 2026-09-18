import { chmodSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { GatewayLauncher, findWingExecutable, summarize } from '../../src/host/gateway/launcher';
import type { GatewaySettings } from '../../src/host/settings';
import { DEFAULT_GATEWAY_PORT, readGatewaySettings } from '../../src/host/settings';
import { disposeLog } from '../../src/host/log';
import { mockState } from '../mocks/vscode';

/**
 * Launcher invariants (design.md D8): **probe first, never restart a gateway
 * the user is using**, start at most once, and fail with something actionable.
 */

const settings: GatewaySettings = {
  host: '127.0.0.1',
  port: 32_523,
  apiKey: null,
  wingPath: null,
  autoStart: true,
};

const temps: string[] = [];
afterEach(() => {
  while (temps.length > 0) {
    const dir = temps.pop();
    if (dir !== undefined) {
      rmSync(dir, { recursive: true, force: true });
    }
  }
});

function tempDirWithWing(): string {
  const dir = mkdtempSync(path.join(tmpdir(), 'wing-host-test-'));
  temps.push(dir);
  const executable = path.join(dir, 'wing');
  writeFileSync(executable, '#!/bin/sh\nexit 0\n');
  chmodSync(executable, 0o755);
  return dir;
}

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
  });
});

describe('findWingExecutable', () => {
  it('prefers the explicit setting and rejects a non-executable path', () => {
    const dir = tempDirWithWing();
    expect(findWingExecutable({ ...settings, wingPath: path.join(dir, 'wing') })).toBe(
      path.join(dir, 'wing'),
    );
    expect(findWingExecutable({ ...settings, wingPath: path.join(dir, 'missing') })).toBeNull();
  });

  it('finds `wing` on PATH when no explicit path is set', () => {
    const dir = tempDirWithWing();
    const previousHome = process.env['HOME'];
    // Well-known locations are searched before PATH — isolate HOME so this
    // asserts the PATH rule, not whatever the developer machine has installed.
    process.env['HOME'] = dir;
    try {
      const found = findWingExecutable({ ...settings, wingPath: null }, { PATH: dir });
      expect(found).toBe(path.join(dir, 'wing'));
    } finally {
      process.env['HOME'] = previousHome;
    }
  });

  it('returns null when nothing matches (no silent spawn attempts)', () => {
    const dir = mkdtempSync(path.join(tmpdir(), 'wing-host-empty-'));
    temps.push(dir);
    const previousHome = process.env['HOME'];
    process.env['HOME'] = dir;
    try {
      expect(findWingExecutable({ ...settings, wingPath: null }, { PATH: dir })).toBeNull();
    } finally {
      process.env['HOME'] = previousHome;
    }
  });
});

describe('summarize', () => {
  it('keeps the last non-empty lines', () => {
    expect(summarize('line 1\n\n line 2 \nline 3\n')).toBe('line 1 | line 2 | line 3');
  });
});

describe('settings', () => {
  beforeEach(() => {
    mockState.reset();
    disposeLog();
  });

  it('normalizes blank keys, invalid ports and defaults', () => {
    mockState.configuration.set('wing.host', ' localhost ');
    mockState.configuration.set('wing.port', 0);
    mockState.configuration.set('wing.apiKey', '   ');
    mockState.configuration.set('wing.wingPath', '  ');

    const read = readGatewaySettings();
    expect(read.host).toBe('localhost');
    expect(read.port).toBe(DEFAULT_GATEWAY_PORT);
    expect(read.apiKey).toBeNull();
    expect(read.wingPath).toBeNull();
    expect(read.autoStart).toBe(true);
  });

  it('accepts string ports and explicit values', () => {
    mockState.configuration.set('wing.port', '41234');
    mockState.configuration.set('wing.apiKey', ' secret ');
    mockState.configuration.set('wing.autoStart', false);

    const read = readGatewaySettings();
    expect(read.port).toBe(41_234);
    expect(read.apiKey).toBe('secret');
    expect(read.autoStart).toBe(false);
  });
});
