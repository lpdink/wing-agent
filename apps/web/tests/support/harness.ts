/**
 * Test harness: the production `GatewayRuntime` wired to the in-process fake
 * gateway, with the seams a node test needs (clock, list poll interval, page
 * visibility).
 *
 * No stubs for the code under test — the runtime, the real `GatewayConnection` and
 * the real `GatewayHttpClient` all run. What the tests observe is what the product
 * does on the wire (frames, HTTP calls, the snapshot React renders).
 */

import { vi } from 'vitest';

import type { RuntimeSnapshot, GatewayRuntimeOptions } from '../../src/connection/runtime';
import { GatewayRuntime } from '../../src/connection/runtime';
import { type GatewaySettings, DEFAULT_SETTINGS } from '../../src/settings/settings';
import type { PageLocation } from '../../src/settings/urls';

import { FakeGateway } from './fake-gateway';

export const TEST_LOCATION: PageLocation = { origin: 'http://localhost:5173' };

export interface HarnessOptions {
  readonly gateway?: FakeGateway;
  readonly settings?: Partial<GatewaySettings>;
  readonly now?: () => number;
  readonly listPollIntervalMs?: number;
  readonly isPageVisible?: () => boolean;
  readonly maxCachedRecords?: number;
  readonly connectRetryBaseMs?: number;
  readonly noticeTtlMs?: number;
  readonly location?: PageLocation;
  readonly onSettingsChange?: GatewayRuntimeOptions['onSettingsChange'];
}

export interface RuntimeHarness {
  readonly runtime: GatewayRuntime;
  readonly gateway: FakeGateway;
  readonly settings: GatewaySettings;
  snapshot(): RuntimeSnapshot;
  /** Let microtasks (and timers, when fake ones are installed) settle. */
  settle(ms?: number): Promise<void>;
}

export function createHarness(options: HarnessOptions = {}): RuntimeHarness {
  const gateway = options.gateway ?? new FakeGateway();
  const settings: GatewaySettings = { ...DEFAULT_SETTINGS, ...options.settings };
  const runtime = new GatewayRuntime({
    initialSettings: settings,
    location: options.location ?? TEST_LOCATION,
    socketFactory: gateway.socketFactory,
    httpTransport: gateway.transport,
    ...(options.now === undefined ? {} : { now: options.now }),
    // Tests that do not care about polling turn it off explicitly: a default of
    // 5 s would otherwise make assertions order-dependent.
    listPollIntervalMs: options.listPollIntervalMs ?? 0,
    ...(options.isPageVisible === undefined ? {} : { isPageVisible: options.isPageVisible }),
    ...(options.maxCachedRecords === undefined ? {} : { maxCachedRecords: options.maxCachedRecords }),
    ...(options.connectRetryBaseMs === undefined ? {} : { connectRetryBaseMs: options.connectRetryBaseMs }),
    ...(options.onSettingsChange === undefined ? {} : { onSettingsChange: options.onSettingsChange }),
    // Notices are asserted explicitly; keep them until the test says otherwise.
    noticeTtlMs: options.noticeTtlMs ?? 0,
  });
  return {
    runtime,
    gateway,
    settings,
    snapshot: () => runtime.getSnapshot(),
    settle: async (ms = 0) => {
      await vi.advanceTimersByTimeAsync(ms);
    },
  };
}

/** Install fake timers for a test file (the runtime's ladders are timer-driven). */
export function useFakeTimers(): void {
  vi.useFakeTimers();
}

/** Session history fixture: one user message, one assistant reply. */
export function simpleHistory(text = 'hello'): Record<string, unknown>[] {
  return [
    { role: 'user', content: text, uuid: `u-${text}` },
    { role: 'assistant', content: `echo: ${text}`, uuid: `a-${text}` },
  ];
}
