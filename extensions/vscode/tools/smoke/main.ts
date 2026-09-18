/**
 * `pnpm run smoke:gateway` — the local end-to-end smoke.
 *
 * Real gateway + real host + scripted model, isolated from the user's world:
 *
 * ```
 * ┌ Node smoke ────────────────────────────────────────────────┐
 * │ FakeProvider (OpenAI-compatible, scripted)                 │
 * │ wing-gateway child (temp WING_HOME, OS-assigned port)      │
 * │ production WingHost + WebviewMirror (the shipped reducer)  │
 * └────────────────────────────────────────────────────────────┘
 * ```
 *
 * Exit codes (the scheduler reads them):
 *
 * - `0` — every scenario passed;
 * - `1` — a scenario failed (the report names it and dumps the world + log tail);
 * - `3` — skipped: no `wing-gateway` on this machine (or `WING_SMOKE_SKIP=1`).
 *
 * Flags: `--only <substring>`, `--keep` (keep the scratch dir), `--list`.
 */

import { mkdtempSync, rmSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';

import { FakeProvider } from './fake-provider';
import { assertIsolated, findGatewayBinary, SmokeGateway, smokeRoot } from './gateway';
import { SCENARIOS } from './scenarios';
import type { Scenario, ScenarioContext } from './scenarios';
import { SmokeWorld } from './world';
import { startNoCompressionProxy } from './ws-proxy';
import type { NoCompressionProxy } from './ws-proxy';

export const EXIT_PASS = 0;
export const EXIT_FAIL = 1;
export const EXIT_SKIP = 3;

const MODEL = 'smoke/default';
/** Hard cap per scenario (the individual waits have their own, smaller, deadlines). */
const SCENARIO_TIMEOUT_MS = 120_000;

interface Options {
  readonly only: string | null;
  readonly keep: boolean;
  readonly list: boolean;
}

function parseArgs(argv: readonly string[]): Options {
  const index = argv.indexOf('--only');
  return {
    only: index >= 0 ? (argv[index + 1] ?? null) : null,
    keep: argv.includes('--keep'),
    list: argv.includes('--list'),
  };
}

function line(text: string): void {
  process.stdout.write(`${text}\n`);
}

function selectedScenarios(options: Options): readonly Scenario[] {
  const only = options.only;
  if (only === null) {
    return SCENARIOS;
  }
  return SCENARIOS.filter((scenario) => scenario.name.includes(only));
}

async function withTimeout<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
  let timer: NodeJS.Timeout | null = null;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          reject(new Error(`${label} timed out after ${timeoutMs} ms`));
        }, timeoutMs);
      }),
    ]);
  } finally {
    if (timer !== null) {
      clearTimeout(timer);
    }
  }
}

async function main(): Promise<number> {
  const options = parseArgs(process.argv.slice(2));
  if (options.list) {
    for (const scenario of SCENARIOS) {
      line(`${scenario.name.padEnd(24)} ${scenario.description}`);
    }
    return EXIT_PASS;
  }

  if (process.env['WING_SMOKE_SKIP'] === '1') {
    line('SMOKE SKIP: WING_SMOKE_SKIP=1');
    return EXIT_SKIP;
  }

  let binary;
  try {
    binary = findGatewayBinary();
  } catch (error) {
    line(`SMOKE FAIL: ${error instanceof Error ? error.message : String(error)}`);
    return EXIT_FAIL;
  }
  if (binary === null) {
    line('SMOKE SKIP: no wing-gateway executable found');
    line('  searched: $WING_GATEWAY_BIN → <repo>/.venv/bin/wing-gateway → PATH');
    line('  run `uv sync` in the repository root (or set $WING_GATEWAY_BIN) and re-run:');
    line('    cd extensions/vscode && pnpm run smoke:gateway');
    return EXIT_SKIP;
  }

  const scenarios = selectedScenarios(options);
  if (scenarios.length === 0) {
    line(`SMOKE FAIL: --only ${options.only ?? ''} matched no scenario`);
    return EXIT_FAIL;
  }

  const root = mkdtempSync(path.join(smokeRoot(), 'run-'));
  const provider = new FakeProvider();
  // `WING_SMOKE_AUTH_KEY=<key>` turns the generated gateway config into an
  // authenticated one and makes the host send `Authorization: Bearer …`; every
  // scenario then exercises the real auth middleware on HTTP *and* WS (review
  // #109 [P3-6]). Unset (default, and CI) keeps the keyless configuration.
  const authKey = process.env['WING_SMOKE_AUTH_KEY'] ?? null;
  const gateway = new SmokeGateway({
    binary,
    provider,
    model: MODEL,
    root,
    logFile: path.join(root, 'gateway.log'),
    authKey,
  });
  const unhandled: string[] = [];
  const onUnhandled = (reason: unknown): void => {
    unhandled.push(reason instanceof Error ? (reason.stack ?? reason.message) : String(reason));
    line(`[smoke] unhandled rejection: ${String(reason)}`);
  };
  process.on('unhandledRejection', onUnhandled);

  let world: SmokeWorld | null = null;
  let proxy: NoCompressionProxy | null = null;
  const failures: string[] = [];

  const stopOnSignal = (signal: string): void => {
    line(`\nSMOKE ABORT: ${signal}`);
    void shutdown().then(() => {
      process.exit(EXIT_FAIL);
    });
  };
  process.once('SIGINT', () => {
    stopOnSignal('SIGINT');
  });
  process.once('SIGTERM', () => {
    stopOnSignal('SIGTERM');
  });

  async function shutdown(): Promise<void> {
    world?.dispose();
    await proxy?.stop().catch(() => undefined);
    await gateway.stop().catch(() => undefined);
    await provider.stop().catch(() => undefined);
    if (!options.keep) {
      rmSync(root, { recursive: true, force: true });
    }
  }

  try {
    await provider.start();
    await gateway.start();
    assertIsolated(gateway.port);
    // The host talks to the no-compression relay instead of the gateway directly:
    // Node's undici WS stalls on some permessage-deflate frames (see ws-proxy.ts).
    proxy = await startNoCompressionProxy(gateway.port, {
      stripExtensions: process.env['WING_SMOKE_KEEP_COMPRESSION'] !== '1',
    });

    line('wing vscode smoke — real gateway, scripted model');
    line(`  gateway:  ${binary.path} (${binary.source})`);
    line(`  port:     ${gateway.port}  (user's 32523 is never touched)`);
    line(`  relay:    ${proxy.port} → ${gateway.port} (WS without compression)`);
    line(`  WING_HOME ${gateway.wingHome}`);
    line(`  sessions: ${gateway.sessionsPath}`);
    line(`  provider: ${provider.baseUrl}`);
    line(`  model:    ${MODEL}`);
    if (authKey !== null) {
      line(`  auth:     enabled (Authorization: Bearer … on HTTP and WS)`);
    }

    // `WING_SMOKE_WS_FALLBACK=1` deletes the runtime's global WebSocket before the
    // host boots, which is the situation on a VS Code 1.100 host (Electron 34 /
    // Node 20.19): every scenario then runs through the *bundled* `ws` client
    // (review #109 [P1-3]). It proves the bundle, not just the source, can carry
    // the protocol — the unit test covers the source path.
    if (process.env['WING_SMOKE_WS_FALLBACK'] === '1') {
      const deleted = Reflect.deleteProperty(globalThis, 'WebSocket');
      if (!deleted || globalThis.WebSocket !== undefined) {
        line('SMOKE FAIL: could not remove the global WebSocket (WING_SMOKE_WS_FALLBACK=1)');
        return EXIT_FAIL;
      }
      line('  ws impl:  bundled ws fallback (global WebSocket deleted)');
    }
    line('');

    world = new SmokeWorld({
      port: proxy.port,
      workspace: gateway.workspace,
      apiKey: authKey,
      report: (message) => {
        line(`  [${message}]`);
      },
    });
    await world.start();
    await world.waitFor(() => provider.requestCount() === 0, { label: 'the world to boot' });

    for (const scenario of scenarios) {
      const started = Date.now();
      const context: ScenarioContext = {
        world,
        gateway,
        provider,
        model: MODEL,
        log: (message) => {
          line(`  · ${message}`);
        },
      };
      try {
        const detail = await withTimeout(scenario.run(context), SCENARIO_TIMEOUT_MS, scenario.name);
        line(`✓ ${scenario.name} — ${detail} (${Date.now() - started} ms)`);
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        failures.push(scenario.name);
        line(`✗ ${scenario.name} — ${message}`);
        line(world.describe());
        line(gateway.logTail());
      }
    }
  } catch (error) {
    const message = error instanceof Error ? (error.stack ?? error.message) : String(error);
    failures.push('smoke bootstrap');
    line(`✗ smoke bootstrap — ${message}`);
    line(gateway.logTail());
  } finally {
    await shutdown();
    process.off('unhandledRejection', onUnhandled);
  }

  if (unhandled.length > 0) {
    failures.push('unhandled rejection');
    for (const entry of unhandled) {
      line(`✗ unhandled rejection:\n${entry}`);
    }
  }

  line('');
  if (failures.length === 0) {
    line(`SMOKE PASS — ${scenarios.length}/${SCENARIOS.length} scenarios green`);
    return EXIT_PASS;
  }
  line(`SMOKE FAIL — ${failures.length} problem(s): ${failures.join(', ')}`);
  if (options.keep) {
    line(`scratch kept at ${root}`);
  }
  return EXIT_FAIL;
}

main()
  .then((code) => {
    process.exitCode = code;
  })
  .catch((error: unknown) => {
    line(`SMOKE FAIL: ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`);
    process.exitCode = EXIT_FAIL;
  });
