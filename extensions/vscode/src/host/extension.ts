import * as vscode from 'vscode';

import { ChatViewProvider } from './chatViewProvider';
import { log } from './log';

/**
 * Extension entry point.
 *
 * Activation is driven by the contributed view (`onView:wing.chatView` in
 * `package.json`), so the work here is exactly: create the provider, register it,
 * and put everything on the extension context's subscription list.
 *
 * Step 03 adds the session manager / gateway client to the same list — keep this
 * function a wiring list, not a place for logic.
 */
export function activate(context: vscode.ExtensionContext): void {
  const chatView = new ChatViewProvider(context.extensionUri);

  context.subscriptions.push(
    chatView,
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewId, chatView, {
      // Keep the DOM (and the composer draft) alive when the user switches away
      // from the sidebar. The bridge does not depend on it: a re-created webview
      // re-runs `ready` → `hydrate` (design.md D9).
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );

  log().info('Wing extension activated');
}

/** Nothing to flush yet; the subscription list disposes everything created above. */
export function deactivate(): void {}
