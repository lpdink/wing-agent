import type * as vscode from 'vscode';

import type { HostToWebviewMessage, WebviewToHostMessage } from '../shared';
import { isWebviewToHostMessage } from '../shared';

import { log } from './log';

/** A user intent (every webview message that is not protocol bookkeeping). */
export type WebviewIntent = Exclude<WebviewToHostMessage, { type: 'ready' | 'resync' | 'ping' }>;

export interface HostBridgeHandlers {
  /** The webview mounted and wants its initial state. */
  onReady(protocolVersion: number): void;
  /** The webview lost continuity and needs a full `hydrate`. */
  onResync(message: Extract<WebviewToHostMessage, { type: 'resync' }>): void;
  /** Channel diagnostics — answer with `pong`. */
  onPing(id: string): void;
  /** Everything else the user asked for. */
  onIntent(intent: WebviewIntent): void;
}

/**
 * Host side of the webview message channel.
 *
 * Owns exactly two things: serializing outgoing messages, and turning raw
 * `onDidReceiveMessage` payloads into typed intents (unknown payloads are logged
 * and dropped — never guessed at). Session semantics live above this class.
 */
export class HostBridge implements vscode.Disposable {
  private readonly subscription: vscode.Disposable;

  constructor(
    private readonly webview: Pick<vscode.Webview, 'postMessage' | 'onDidReceiveMessage'>,
    private readonly handlers: HostBridgeHandlers,
  ) {
    this.subscription = this.webview.onDidReceiveMessage((raw: unknown) => {
      this.dispatch(raw);
    });
  }

  /** Post one message into the webview. Serialization errors are logged, not thrown. */
  post(message: HostToWebviewMessage): void {
    void this.webview.postMessage(message).then(
      (delivered) => {
        if (delivered === false) {
          log().debug(`[bridge] message dropped (webview not ready): ${message.type}`);
        }
      },
      (error: unknown) => {
        log().error(`[bridge] failed to post ${message.type}: ${String(error)}`);
      },
    );
  }

  dispose(): void {
    this.subscription.dispose();
  }

  private dispatch(raw: unknown): void {
    if (!isWebviewToHostMessage(raw)) {
      log().warn(`[bridge] ignoring unrecognized webview message: ${safeDescribe(raw)}`);
      return;
    }

    switch (raw.type) {
      case 'ready':
        this.handlers.onReady(raw.protocolVersion);
        return;
      case 'resync':
        this.handlers.onResync(raw);
        return;
      case 'ping':
        this.handlers.onPing(raw.id);
        return;
      default:
        this.handlers.onIntent(raw);
    }
  }
}

/** Short, log-safe description of an unknown payload (no full dumps of big objects). */
function safeDescribe(value: unknown): string {
  if (typeof value === 'object' && value !== null) {
    const record = value as Record<string, unknown>;
    const type = typeof record['type'] === 'string' ? record['type'] : '<no type>';
    return `{ type: ${type}, keys: ${Object.keys(record).slice(0, 8).join(',')} }`;
  }
  return typeof value;
}
