import { spawn } from 'node:child_process';
import { statSync } from 'node:fs';
import { isIPv4 } from 'node:net';
import { homedir } from 'node:os';
import path from 'node:path';

/**
 * Gateway liveness + start-up, ported from
 * `extensions/vscode/src/host/gateway/launcher.ts` (same policy, same constants).
 *
 * Policy — probe first, **never restart a gateway the user is using**:
 *
 * 1. `probe()` first. A healthy gateway means `wing start` is never executed.
 * 2. On a dead gateway with auto-start on, run `wing start` **once** and wait for
 *    it to exit (`crates/wing/src/cmd/start.rs` spawns the daemon itself and polls
 *    health before returning, so daemonizing is not our job).
 * 3. Poll the probe briefly (the CLI already waited; this is the safety net).
 * 4. Otherwise return a reason the shell turns into a log line / user message.
 *
 * Two differences from the VS Code version:
 *
 * - the input is a full `baseUrl` (https and path prefixes are allowed), not a
 *   `host` + `port` pair;
 * - a **non-local** base URL is never auto-started (`remote`): running
 *   `wing start` on this machine would not help a remote gateway, and could
 *   surprise a user who is running their own.
 *
 * Everything is injectable — the unit tests drive probe/run/find/sleep without
 * spawning anything.
 */

export interface GatewayLaunchSettings {
  /** Gateway origin, e.g. `http://127.0.0.1:32523` (no trailing slash required). */
  readonly baseUrl: string;
  /** Explicit `wing` executable; `null` = discover on well-known paths / `PATH`. */
  readonly wingPath: string | null;
  /** When `false`, a dead gateway is reported instead of started. */
  readonly autoStart: boolean;
}

export type LaunchOutcome =
  /** A gateway answered the health probe — nothing was spawned. */
  | { readonly status: 'already-running' }
  /** `wing start` ran and the gateway is now answering. */
  | { readonly status: 'started' }
  /** Auto-start is disabled in the config. */
  | { readonly status: 'disabled' }
  /** The gateway is not on this machine: `wing start` is never attempted. */
  | { readonly status: 'remote' }
  /** No `wing` executable could be found. */
  | { readonly status: 'not-found' }
  /** The gateway did not come up; `detail` carries the CLI output / probe result. */
  | { readonly status: 'failed'; readonly detail: string };

export interface RunResult {
  readonly code: number;
  readonly output: string;
}

export interface GatewayLauncherOptions {
  /** `true` when the gateway answers `GET <baseUrl>/api/health`. */
  readonly probe?: (baseUrl: string, timeoutMs: number) => Promise<boolean>;
  /** Run the CLI; resolves with the exit code and captured output. */
  readonly run?: (executable: string, args: readonly string[]) => Promise<RunResult>;
  /** Locate the `wing` executable (`null` when not found). */
  readonly findExecutable?: (wingPath: string | null) => string | null;
  readonly sleep?: (ms: number) => Promise<void>;
}

/** Health probe deadline: short enough to not stall start-up. */
export const PROBE_TIMEOUT_MS = 2_000;
/** `wing start` may legitimately take a while (spawn + health poll). */
export const START_TIMEOUT_MS = 20_000;
/** Polling after `wing start` returned. */
export const READY_POLL_ATTEMPTS = 20;
export const READY_POLL_INTERVAL_MS = 500;

/** `http://host:port[/prefix]` → `http://host:port[/prefix]/api/health`. */
export function healthUrl(baseUrl: string): string {
  return `${baseUrl.replace(/\/+$/u, '')}/api/health`;
}

/**
 * `true` when the gateway runs on this machine (loopback host or `localhost`).
 * Only then does `wing start` make sense.
 */
export function isLocalGatewayUrl(baseUrl: string): boolean {
  let host: string;
  try {
    host = new URL(baseUrl).hostname.toLowerCase();
  } catch {
    return false;
  }
  // IPv6 loopback arrives bracketed (`[::1]`) from `URL`.
  if (host === '[::1]' || host === '::1' || host === 'localhost') {
    return true;
  }
  if (host.endsWith('.localhost')) {
    return true;
  }
  // `127.0.0.0/8`, but only as a real address: `127.0.0.1.evil.test` is not local.
  return isIPv4(host) && host.startsWith('127.');
}

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

/**
 * Default discovery: explicit config → well-known paths → `PATH`.
 *
 * The explicit setting is authoritative in both directions: a broken `wingPath`
 * returns `null` instead of silently falling back to another binary.
 */
export function findWingExecutable(
  wingPath: string | null,
  env: NodeJS.ProcessEnv = process.env,
): string | null {
  if (wingPath !== null) {
    return isExecutableFile(wingPath) ? wingPath : null;
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

/**
 * Default probe: node's `fetch`.
 *
 * The shell injects an Electron `net.fetch` implementation instead, because only
 * Chromium's network stack honours the session's certificate policy. This default
 * keeps the module free of Electron imports (and is what the unit tests stub out).
 */
export async function probeGateway(baseUrl: string, timeoutMs: number): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => {
    controller.abort();
  }, timeoutMs);
  try {
    const response = await fetch(healthUrl(baseUrl), { method: 'GET', signal: controller.signal });
    return response.ok;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

export class GatewayLauncher {
  private readonly probe: (baseUrl: string, timeoutMs: number) => Promise<boolean>;
  private readonly run: (executable: string, args: readonly string[]) => Promise<RunResult>;
  private readonly findExecutable: (wingPath: string | null) => string | null;
  private readonly sleep: (ms: number) => Promise<void>;

  constructor(options: GatewayLauncherOptions = {}) {
    this.probe = options.probe ?? probeGateway;
    this.run = options.run ?? runWingStart;
    this.findExecutable = options.findExecutable ?? ((wingPath) => findWingExecutable(wingPath));
    this.sleep =
      options.sleep ??
      ((ms: number) =>
        new Promise<void>((resolve) => {
          setTimeout(resolve, ms);
        }));
  }

  /** `true` when a gateway answers the health probe. */
  async isRunning(baseUrl: string): Promise<boolean> {
    return this.probe(baseUrl, PROBE_TIMEOUT_MS);
  }

  /**
   * Bring the gateway up when it is not running.
   *
   * Returns a reason even when it fails — the caller decides how loud to be, and
   * must not call this in a loop.
   */
  async ensureRunning(settings: GatewayLaunchSettings): Promise<LaunchOutcome> {
    if (await this.isRunning(settings.baseUrl)) {
      return { status: 'already-running' };
    }
    if (!settings.autoStart) {
      return { status: 'disabled' };
    }
    if (!isLocalGatewayUrl(settings.baseUrl)) {
      return { status: 'remote' };
    }
    const executable = this.findExecutable(settings.wingPath);
    if (executable === null) {
      return { status: 'not-found' };
    }
    const result = await this.run(executable, ['start']);
    if (result.code !== 0) {
      return { status: 'failed', detail: summarize(result.output) };
    }
    for (let attempt = 0; attempt < READY_POLL_ATTEMPTS; attempt += 1) {
      if (await this.isRunning(settings.baseUrl)) {
        return { status: 'started' };
      }
      await this.sleep(READY_POLL_INTERVAL_MS);
    }
    return {
      status: 'failed',
      detail: 'wing start returned 0 but the gateway did not answer /api/health',
    };
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
