import * as vscode from 'vscode';

import type { WingHost } from './wingHost';

/**
 * Command palette surface for the host.
 *
 * The webview drives everything through the bridge; these commands exist for
 * the palette and keybindings (and for the "the view is not open yet" case).
 * Keep the set minimal — every entry is a promise about behaviour.
 */

export const COMMAND_IDS = {
  newSession: 'wing.newSession',
  reconnectGateway: 'wing.reconnectGateway',
} as const;

/** Register the host commands; the caller owns the returned disposables. */
export function registerCommands(host: WingHost): vscode.Disposable[] {
  return [
    vscode.commands.registerCommand(COMMAND_IDS.newSession, async () => {
      await host.newSession();
    }),
    vscode.commands.registerCommand(COMMAND_IDS.reconnectGateway, async () => {
      await host.reconnect();
    }),
  ];
}
