import { spawn } from 'node:child_process';
import { statSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';

import type { CoreLogger } from '../../core';
import { silentLogger } from '../../core';
import type { GatewaySettings } from '../settings';

/**
 * Gateway liveness + start-up.
 *
 * Policy (design.md D8) — probe first, **never restart a gateway the user is
 * using**:
 *
 * 1. `probe()` first. A healthy gateway means `wing start` is never executed
 *    (which matters: `wing start` is only *usually* safe, and a probe is the
 *    only honest way to know).
 * 2. On a dead gateway with auto-start on, run `wing start` **once** and wait
 *    for it to exit (`crates/wing/src/cmd/start.rs` spawns the daemon itself and
 *    polls health before returning, so this is not our job to daemonize).
 * 3. Poll the probe briefly (the CLI already waited, this is the safety net).
 * 4. Otherwise return a reason the host turns into a user-facing message.
 *
 * Everything is injectable: tests drive probe/run/find/sleep without spawning
 * anything.
 */

export type LaunchOutcome =
  /** A gateway answered the health probe — nothing was spawned. */
  | { readonly status: 'already-running' }
  /** `wing start` ran and the gateway is now answering. */
  | { readonly status: 'started' }
  /** Auto-start is disabled in the settings. */
  | { readonly status: 'disabled' }
  /** No `wing` executable could be found. */
  | { readonly status: 'not-found' }
  /** The gateway did not come up; `detail` carries the CLI output / probe result. */
  | { readonly status: 'failed'; readonly detail: string };

export interface GatewayLauncherOptions {
  /** `true` when the gateway answers `GET /api/health`. */
  readonly probe?: (settings: GatewaySettings, timeoutMs: number) => Promise<boolean>;
  /** Run the CLI; resolves with the exit code and captured output. */
  readonly run?: (executable: string, args: readonly string[]) => Promise<RunResult>;
  /** Locate the `wing` executable (`null` when not found). */
  readonly findExecutable?: (settings: GatewaySettings) => string | null;
  readonly sleep?: (ms: number) => Promise<void>;
  readonly logger?: CoreLogger;
}

export interface RunResult {
  readonly code: number;
  readonly output: string;
}

/** Health probe deadline: short enough to not stall activation. */
export const PROBE_TIMEOUT_MS = 2_000;
/** `wing start` may legitimately take a while (spawn + health poll). */
export const START_TIMEOUT_MS = 20_000;
/** Polling after `wing start` returned. */
export const READY_POLL_ATTEMPTS = 20;
export const READY_POLL_INTERVAL_MS = 500;

/** Well-known install locations tried when PATH has no `wing`. */
function defaultCandidates(): string[] {
  const home = homedir();
  return [
    path.join(home, '.local', 'bin', 'wing'),
    path.join(home, '.cargo', 'bin', 'wing'),
    path.join(home, 'bin', 'wing'),
    path.join(home, '.wing', 'bin', 'wing'),
    '/usr/local/bin/wing',
    '/opt/homebrew/bin/wing',
    '/usr/bin/wing',
  ];
}

/** `true` when `candidate` is an executable file we can spawn. */
function isExecutableFile(candidate: string): boolean {
  try {
    const stats = statSync(candidate);
    return stats.isFile() && (stats.mode & 0o111) !== 0;
  } catch {
    return false;
  }
}

/** Default discovery: explicit setting → well-known paths → `PATH`. */
export function findWingExecutable(
  settings: GatewaySettings,
  env: NodeJS.ProcessEnv = process.env,
): string | null {
  if (settings.wingPath !== null) {
    return isExecutableFile(settings.wingPath) ? settings.wingPath : null;
  }
  for (const candidate of defaultCandidates()) {
    if (isExecutableFile(candidate)) {
      return candidate;
    }
  }
  const pathValue = env['PATH'] ?? '';
  for (const directory of pathValue.split(path.delimiter)) {
    if (directory === '') {
      continue;
    }
    const candidate = path.join(directory, 'wing');
    if (isExecutableFile(candidate)) {
      return candidate;
    }
  }
  return null;
}

/** Default `wing start` runner (bounded, output captured for error messages). */
function runWingStart(executable: string, args: readonly string[]): Promise<RunResult> {
  return new Promise<RunResult>((resolve) => {
    const child = spawn(executable, [...args], { stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    let settled = false;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        child.kill();
        resolve({ code: -1, output: `${output}\n(wing start timed out after ${START_TIMEOUT_MS} ms)` });
      }
    }, START_TIMEOUT_MS);
    const collect = (chunk: Buffer): void => {
      output += chunk.toString('utf8');
      if (output.length > 4_000) {
        output = output.slice(-4_000);
      }
    };
    child.stdout?.on('data', collect);
    child.stderr?.on('data', collect);
    child.on('error', (error: Error) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve({ code: -1, output: `${output}\nspawn failed: ${error.message}` });
      }
    });
    child.on('close', (code: number | null) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve({ code: code ?? -1, output });
      }
    });
  });
}

/** `fetch`-based health probe (`GET /api/health`, no auth required). */
export async function probeGateway(settings: GatewaySettings, timeoutMs: number): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => {
    controller.abort();
  }, timeoutMs);
  try {
    const response = await fetch(`http://${settings.host}:${settings.port}/api/health`, {
      method: 'GET',
      signal: controller.signal,
    });
    return response.ok;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

export class GatewayLauncher {
  private readonly probe: (settings: GatewaySettings, timeoutMs: number) => Promise<boolean>;
  private readonly run: (executable: string, args: readonly string[]) => Promise<RunResult>;
  private readonly findExecutable: (settings: GatewaySettings) => string | null;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly logger: CoreLogger;

  constructor(options: GatewayLauncherOptions = {}) {
    this.probe = options.probe ?? probeGateway;
    this.run = options.run ?? runWingStart;
    this.findExecutable = options.findExecutable ?? ((settings) => findWingExecutable(settings));
    this.sleep =
      options.sleep ??
      ((ms: number) =>
        new Promise<void>((resolve) => {
          setTimeout(resolve, ms);
        }));
    this.logger = options.logger ?? silentLogger;
  }

  /** `true` when a gateway answers the health probe. */
  async isRunning(settings: GatewaySettings): Promise<boolean> {
    return this.probe(settings, PROBE_TIMEOUT_MS);
  }

  /**
   * Bring the gateway up when it is not running.
   *
   * Returns a reason even when it fails — the caller decides how loud to be
   * (`globalNotice` + `showErrorMessage`), and must not call this in a loop.
   */
  async ensureRunning(settings: GatewaySettings): Promise<LaunchOutcome> {
    if (await this.isRunning(settings)) {
      this.logger.debug('gateway already running; not starting another one');
      return { status: 'already-running' };
    }
    if (!settings.autoStart) {
      return { status: 'disabled' };
    }
    const executable = this.findExecutable(settings);
    if (executable === null) {
      return { status: 'not-found' };
    }
    this.logger.debug(`starting gateway via ${executable} start`);
    const result = await this.run(executable, ['start']);
    if (result.code !== 0) {
      return { status: 'failed', detail: summarize(result.output) };
    }
    for (let attempt = 0; attempt < READY_POLL_ATTEMPTS; attempt += 1) {
      if (await this.isRunning(settings)) {
        return { status: 'started' };
      }
      await this.sleep(READY_POLL_INTERVAL_MS);
    }
    return { status: 'failed', detail: 'wing start returned 0 but the gateway did not answer /api/health' };
  }
}

/** Last few non-empty output lines, for a human-readable failure message. */
export function summarize(output: string): string {
  const lines = output
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line !== '');
  return lines.slice(-4).join(' | ');
}
