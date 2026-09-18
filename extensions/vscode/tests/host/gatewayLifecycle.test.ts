import { afterEach, describe, expect, it, vi } from 'vitest';

import type { GlobalNoticeModel } from '../../src/shared';

import { createHostHarness, flushMicrotasks } from './support/harness';
import type { HostHarness } from './support/harness';

/**
 * The host-level gateway lifecycle (review N7): the four launcher outcomes, and
 * the budget that keeps a failing `wing start` from being retried on every
 * connect attempt.
 *
 * The launcher itself is covered by `launcher.test.ts`; what is asserted here is
 * what the *host* does with each outcome (notices, errors, one-shot budget).
 */

const teardown: HostHarness[] = [];
afterEach(() => {
  vi.useRealTimers();
  while (teardown.length > 0) {
    teardown.pop()?.dispose();
  }
});

function notices(harness: HostHarness): GlobalNoticeModel[] {
  return harness
    .ofType('panels')
    .map((message) => message.panels.globalNotice)
    .filter((notice): notice is GlobalNoticeModel => notice !== null);
}

function toasts(harness: HostHarness): string[] {
  return harness
    .ofType('ui')
    .filter((message) => message.action.kind === 'toast')
    .map((message) => (message.action.kind === 'toast' ? message.action.message : ''));
}

describe('auto-start outcomes', () => {
  it('explains how to start a gateway when auto-start is off', async () => {
    const run = vi.fn();
    const harness = createHostHarness({
      settings: { autoStart: false },
      launcher: { probe: () => Promise.resolve(false), run },
    });
    teardown.push(harness);

    await harness.host.start();
    await flushMicrotasks();

    expect(run).not.toHaveBeenCalled();
    expect(toasts(harness).some((text) => text.includes('wing.autoStart'))).toBe(true);
    expect(harness.host.sessionManager.openSessionIds).toEqual([]);
  });

  it('points at `wing.wingPath` when the executable is missing', async () => {
    const harness = createHostHarness({
      launcher: {
        probe: () => Promise.resolve(false),
        findExecutable: () => null,
        run: () => Promise.resolve({ code: 1, output: 'never runs' }),
      },
    });
    teardown.push(harness);

    await harness.host.start();
    await flushMicrotasks();

    expect(harness.errors.some((text) => text.includes('wing.wingPath'))).toBe(true);
    expect(toasts(harness).some((text) => text.includes('wing.wingPath'))).toBe(true);
  });

  it('reports a failed `wing start` with the CLI output, exactly once', async () => {
    vi.useFakeTimers();
    const run = vi.fn().mockResolvedValue({
      code: 1,
      output: 'error: port 32523 is already in use by another process',
    });
    const harness = createHostHarness({
      launcher: { probe: () => Promise.resolve(false), run, findExecutable: () => '/fake/wing' },
    });
    teardown.push(harness);

    await harness.host.start();
    await flushMicrotasks();
    expect(harness.errors.some((text) => text.includes('already in use'))).toBe(true);
    // No session exists yet, so the banner is surfaced as a toast (the only
    // channel a session-less webview has).
    expect(toasts(harness).some((text) => text.includes('port 32523 is already in use'))).toBe(true);
    expect(notices(harness)).toEqual([]);

    // The connect ladder keeps retrying (the gateway is unreachable) but the
    // launcher budget is spent: one spawn attempt, no storm.
    await vi.advanceTimersByTimeAsync(120_000);
    await flushMicrotasks();
    expect(run).toHaveBeenCalledTimes(1);

    // An explicit reconnect resets the budget (the user asked for it).
    await harness.host.reconnect();
    await flushMicrotasks();
    expect(run).toHaveBeenCalledTimes(2);
  });

  it('connects and creates the first session after a successful start', async () => {
    const started: string[] = [];
    const harness = createHostHarness({
      launcher: {
        probe: () => Promise.resolve(false),
        findExecutable: () => '/fake/wing',
        run: (path, args) => {
          started.push(`${path} ${args.join(' ')}`);
          return Promise.resolve({ code: 0, output: 'gateway listening' });
        },
      },
    });
    teardown.push(harness);

    await harness.host.start();
    await harness.ready();
    await flushMicrotasks(20);

    expect(started).toEqual(['/fake/wing start']);
    expect(harness.host.connectionState?.status).toBe('connected');
    const sessionId = harness.gateway.createdOrder[0] ?? '';
    expect(sessionId).not.toBe('');
    expect(harness.hydrateFor(sessionId)).toBeDefined();
    // A healthy gateway means no notice at all.
    expect(harness.host.sessionManager.record(sessionId)?.panels.globalNotice).toBeNull();
  });
});
