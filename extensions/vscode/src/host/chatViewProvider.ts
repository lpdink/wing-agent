import * as vscode from 'vscode';

import { BRIDGE_PROTOCOL_VERSION, WEBVIEW_ROOT_ID } from '../shared';

import type { WebviewIntent } from './bridge';
import { HostBridge } from './bridge';
import { buildWebviewHtml, createNonce } from './html';
import { log } from './log';
import type { WingHost } from './wingHost';

/** Contributed view id — must match `package.json#contributes.views`. */
export const CHAT_VIEW_ID = 'wing.chatView';

/** Built webview assets (names are part of the `vite.config.mts` contract). */
const WEBVIEW_DIR = 'dist/webview';
const WEBVIEW_SCRIPT = 'main.js';
const WEBVIEW_STYLE = 'main.css';

/**
 * The chat sidebar view.
 *
 * Owns exactly two things: the webview document and the message channel. Every
 * session decision (what to render, which tab is active, what an intent means)
 * belongs to the host behind {@link WingHost}; this class is the plumbing that
 * connects `postMessage` to it, so a test can drive the whole product without a
 * webview.
 */
export class ChatViewProvider implements vscode.WebviewViewProvider {
  static readonly viewId = CHAT_VIEW_ID;

  private bridge: HostBridge | undefined;

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly host: WingHost,
  ) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    const { webview } = view;
    webview.options = {
      enableScripts: true,
      localResourceRoots: [this.extensionUri],
    };
    webview.html = this.buildHtml(webview);

    // Re-resolving the same view (e.g. after a container switch) must not leave
    // a dangling listener on the old webview, and session messages must go to
    // the new one.
    this.bridge?.dispose();
    this.host.detachSink();
    this.bridge = new HostBridge(webview, {
      onReady: (protocolVersion) => {
        if (protocolVersion !== BRIDGE_PROTOCOL_VERSION) {
          log().warn(
            `[chat-view] webview speaks bridge v${protocolVersion}, host speaks v${BRIDGE_PROTOCOL_VERSION}`,
          );
        }
        this.host.onReady(protocolVersion);
      },
      onResync: (message) => {
        log().warn(`[chat-view] resync requested (${message.reason}, lastSeq=${message.lastSeq})`);
        this.host.onResync(message.sessionId);
      },
      onPing: (id) => {
        this.bridge?.post({ type: 'pong', id, hostTimeMs: Date.now() });
      },
      onIntent: (intent: WebviewIntent) => {
        void this.host.onIntent(intent);
      },
    });
    this.host.attachSink({
      post: (message) => {
        this.bridge?.post(message);
      },
    });
    log().info('[chat-view] view resolved');
  }

  dispose(): void {
    this.host.detachSink();
    this.bridge?.dispose();
    this.bridge = undefined;
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
