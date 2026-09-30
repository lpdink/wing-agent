import * as vscode from 'vscode';

import type { HostToWebviewMessage } from '../shared';
import { BRIDGE_PROTOCOL_VERSION, WEBVIEW_ROOT_ID } from '../shared';

import type { WebviewIntent } from './bridge';
import { HostBridge } from './bridge';
import { buildWebviewHtml, createNonce } from './html';
import { resolveWorkspaceImage } from './images';
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
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri;
    webview.options = {
      enableScripts: true,
      // The transcript can show images from the workspace, and only from there: the
      // one folder the host already treats as "the workspace" (`resolvePath` in
      // `extension.ts`). Deliberately not the whole disk, and not every folder of a
      // multi-root window — `host/images.ts` refuses anything outside this root.
      localResourceRoots:
        workspaceRoot === undefined ? [this.extensionUri] : [this.extensionUri, workspaceRoot],
    };
    webview.html = this.buildHtml(webview);
    const root = workspaceRoot?.fsPath ?? null;

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
      onResolveImages: (message) => {
        this.bridge?.post(resolveImages(webview, root, message.srcs));
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

/**
 * Answer one `resolveImages` request.
 *
 * Every source that is not a loadable workspace image comes back as `uri: null`,
 * which the renderer renders as the link it has always shown — so this function
 * never has to explain *why*. A debug line records the refused sources (a handful,
 * truncated): "my image is a link" is otherwise indistinguishable from a bug.
 *
 * `asWebviewUri` is the only way to build a URI the webview may load, and VS Code
 * owns its shape (it differs between desktop, remote and virtual workspaces) — we
 * never assemble one ourselves.
 */
function resolveImages(
  webview: Pick<vscode.Webview, 'asWebviewUri'>,
  workspaceRoot: string | null,
  srcs: readonly string[],
): Extract<HostToWebviewMessage, { type: 'images' }> {
  const refused: string[] = [];
  const images = srcs.map((src) => {
    const file = resolveWorkspaceImage(workspaceRoot, src);
    if (file === null) {
      refused.push(src.length > 80 ? `${src.slice(0, 80)}…` : src);
      return { src, uri: null };
    }
    try {
      return { src, uri: webview.asWebviewUri(vscode.Uri.file(file)).toString() };
    } catch (error) {
      // `asWebviewUri` is documented to warn about resources outside the roots; a
      // throw here would take the whole batch down, so treat it as a refusal.
      log().warn(`[chat-view] could not build a webview URI for ${src}: ${String(error)}`);
      return { src, uri: null };
    }
  });

  if (refused.length > 0) {
    log().debug(
      `[chat-view] ${refused.length}/${srcs.length} image sources kept as links: ${refused.join(', ')}`,
    );
  }
  return { type: 'images', images };
}
