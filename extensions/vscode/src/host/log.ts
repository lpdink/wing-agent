import * as vscode from 'vscode';

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
