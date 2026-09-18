import * as vscode from 'vscode';

import type { SessionViewModel, TabModel } from '../shared';
import { BRIDGE_PROTOCOL_VERSION, WEBVIEW_ROOT_ID } from '../shared';

import type { WebviewIntent } from './bridge';
import { HostBridge } from './bridge';
import { buildWebviewHtml, createNonce } from './html';
import { log } from './log';
import { PLACEHOLDER_TABS, createPlaceholderSession } from './scaffold/placeholderSession';

/** Contributed view id — must match `package.json#contributes.views`. */
export const CHAT_VIEW_ID = 'wing.chatView';

/** Built webview assets (names are part of the `vite.config.mts` contract). */
const WEBVIEW_DIR = 'dist/webview';
const WEBVIEW_SCRIPT = 'main.js';
const WEBVIEW_STYLE = 'main.css';

/**
 * The chat sidebar view.
 *
 * Step 01 scope: serve the document, own the bridge, and answer the handshake
 * with the scaffold fixture. Session orchestration (create / subscribe / reduce
 * gateway events) is step 03's job — it replaces the `SCAFFOLD(01)` members below
 * and keeps the bridge plumbing.
 */
export class ChatViewProvider implements vscode.WebviewViewProvider {
  static readonly viewId = CHAT_VIEW_ID;

  // SCAFFOLD(01): fixture session + tab, replaced by the real session layer in step 03.
  private readonly scaffoldSession: SessionViewModel = createPlaceholderSession();
  private readonly scaffoldTabs: readonly TabModel[] = PLACEHOLDER_TABS;

  private bridge: HostBridge | undefined;

  constructor(private readonly extensionUri: vscode.Uri) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    const { webview } = view;
    webview.options = {
      enableScripts: true,
      localResourceRoots: [this.extensionUri],
    };
    webview.html = this.buildHtml(webview);

    // Re-resolving the same view (e.g. after a container switch) must not leave a
    // dangling listener on the old webview.
    this.bridge?.dispose();
    this.bridge = new HostBridge(webview, {
      onReady: (protocolVersion) => {
        this.handleReady(protocolVersion);
      },
      onResync: (message) => {
        log().warn(`[chat-view] resync requested (${message.reason}, lastSeq=${message.lastSeq})`);
        this.postHydrate();
      },
      onPing: (id) => {
        this.bridge?.post({ type: 'pong', id, hostTimeMs: Date.now() });
      },
      onIntent: (intent) => {
        this.handleIntent(intent);
      },
    });
    log().info('[chat-view] view resolved');
  }

  dispose(): void {
    this.bridge?.dispose();
    this.bridge = undefined;
  }

  private handleReady(protocolVersion: number): void {
    if (protocolVersion !== BRIDGE_PROTOCOL_VERSION) {
      log().warn(
        `[chat-view] webview speaks bridge v${protocolVersion}, host speaks v${BRIDGE_PROTOCOL_VERSION}`,
      );
    }
    this.bridge?.post({
      type: 'tabs',
      tabs: this.scaffoldTabs,
      activeSessionId: this.scaffoldSession.sessionId,
    });
    this.postHydrate();
  }

  private postHydrate(): void {
    this.bridge?.post({ type: 'hydrate', session: this.scaffoldSession });
  }

  // SCAFFOLD(01): intents are logged only — the controls that produce them land in
  // steps 03 / 05. Keeping this branch means the scaffold never silently swallows
  // a message: it always says what it received in the output channel.
  private handleIntent(intent: WebviewIntent): void {
    log().info(`[chat-view] intent not implemented in scaffold: ${intent.type}`);
  }

  private buildHtml(webview: vscode.Webview): string {
    const nonce = createNonce();
    const scriptUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, WEBVIEW_DIR, WEBVIEW_SCRIPT),
    );
    const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, WEBVIEW_DIR, WEBVIEW_STYLE));

    return buildWebviewHtml({
      cspSource: webview.cspSource,
      nonce,
      scriptUri: scriptUri.toString(),
      styleUri: styleUri.toString(),
      bootstrap: { protocolVersion: BRIDGE_PROTOCOL_VERSION, assetUris: {} },
      title: 'Wing',
      rootId: WEBVIEW_ROOT_ID,
    });
  }
}
