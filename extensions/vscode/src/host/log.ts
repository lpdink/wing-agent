import * as vscode from 'vscode';

import type { CoreLogger } from '../core';

/**
 * The extension's output channel.
 *
 * Everything the host wants a human to see (reconnects, dropped messages,
 * protocol mistakes) goes here instead of `console.log`, so it is greppable in
 * the Output panel and survives in bug reports.
 */

export const OUTPUT_CHANNEL_NAME = 'Wing';

let channel: vscode.LogOutputChannel | undefined;

/** The shared output channel, created on first use. */
export function log(): vscode.LogOutputChannel {
  channel ??= vscode.window.createOutputChannel(OUTPUT_CHANNEL_NAME, { log: true });
  return channel;
}

/** Close the channel (extension deactivation / test teardown). */
export function disposeLog(): void {
  channel?.dispose();
  channel = undefined;
}

/**
 * The channel as a disposable — register it on the extension context so the host
 * closes the channel on deactivate instead of holding the buffer until the
 * extension host process exits.
 */
export function logDisposable(): vscode.Disposable {
  return new vscode.Disposable(() => {
    disposeLog();
  });
}

/**
 * The output channel as a `CoreLogger`.
 *
 * `src/core` cannot import `vscode`, so the host passes this adapter into the
 * gateway clients: every connection / protocol diagnostic then lands in the
 * same greppable "Wing" channel as the rest of the host.
 */
export function coreLogger(): CoreLogger {
  return {
    debug: (message, detail) => {
      log().debug(formatDetail(message, detail));
    },
    warn: (message, detail) => {
      log().warn(formatDetail(message, detail));
    },
    error: (message, detail) => {
      log().error(formatDetail(message, detail));
    },
  };
}

function formatDetail(message: string, detail: unknown): string {
  if (detail === undefined) {
    return message;
  }
  if (detail instanceof Error) {
    return `${message} — ${detail.message}`;
  }
  try {
    const serialized: unknown = JSON.stringify(detail);
    return `${message} — ${typeof serialized === 'string' ? serialized : describeFallback(detail)}`;
  } catch {
    return `${message} — ${describeFallback(detail)}`;
  }
}

/** Last resort for values `JSON.stringify` refuses (cycles, BigInt, …). */
function describeFallback(detail: unknown): string {
  if (typeof detail === 'string' || typeof detail === 'number' || typeof detail === 'boolean') {
    return `${detail}`;
  }
  return Object.prototype.toString.call(detail);
}
