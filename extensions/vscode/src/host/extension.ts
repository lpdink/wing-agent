import * as vscode from 'vscode';
import path from 'node:path';

import { ChatViewProvider } from './chatViewProvider';
import { registerCommands } from './commands';
import { VsCodeEditorActions } from './editorActions';
import { GatewayLauncher } from './gateway/launcher';
import { coreLogger, log, logDisposable } from './log';
import { readGatewaySettings } from './settings';
import { WingHost, createGatewayClients } from './wingHost';

/**
 * Extension entry point.
 *
 * Activation is driven by the contributed view (`onView:wing.chatView` in
 * `package.json`), so this function is exactly: build the host, register the
 * view + commands, put everything on the subscription list, and start the
 * gateway lifecycle. Keep it a wiring list, not a place for logic.
 */
export function activate(context: vscode.ExtensionContext): void {
  const logger = coreLogger();
  const editor = new VsCodeEditorActions({
    // Tool rows carry paths as the model wrote them (often relative to the
    // session's workspace); the editor needs an absolute file URI.
    resolvePath: (candidate) => {
      if (path.isAbsolute(candidate)) {
        return candidate;
      }
      const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      return root === undefined ? candidate : path.join(root, candidate);
    },
  });
  const host = new WingHost({
    sink: () => null, // replaced when the chat view resolves
    settings: readGatewaySettings,
    gatewayFactory: {
      create: (settings) => createGatewayClients(settings, { logger }),
    },
    launcher: new GatewayLauncher({ logger }),
    workspaceFolder: () => vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null,
    editor,
    reportError: (message) => {
      void vscode.window.showErrorMessage(message);
    },
    logger,
  });

  const chatView = new ChatViewProvider(context.extensionUri, host);

  context.subscriptions.push(
    chatView,
    host,
    editor,
    logDisposable(),
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewId, chatView, {
      // Keep the DOM (and the composer draft) alive when the user switches away
      // from the sidebar. The bridge does not depend on it: a re-created webview
      // re-runs `ready` → `hydrate` (design.md D9 in step 01).
      webviewOptions: { retainContextWhenHidden: true },
    }),
    ...registerCommands(host),
  );

  log().info('Wing extension activated');
  void host.start();
}

/** Everything the extension created is disposed through `context.subscriptions`. */
export function deactivate(): void {}
