import type { WebviewTransport } from '../../shared';
import { isHostToWebviewMessage } from '../../shared';

/**
 * The production transport: VS Code's `postMessage` channel.
 *
 * The document also receives unrelated platform messages, so the transport — the
 * outermost boundary — filters on the bridge's discriminant tags before handing
 * anything to the renderer.
 */

/** The API object `acquireVsCodeApi()` returns (declared structurally for tests). */
export interface VsCodeApiLike {
  postMessage(message: unknown): void;
  getState(): unknown;
  setState(state: unknown): void;
}

/** Event target subset we need (keeps this testable without a full DOM). */
export interface MessageTarget {
  addEventListener(type: 'message', listener: (event: { data: unknown }) => void): void;
  removeEventListener(type: 'message', listener: (event: { data: unknown }) => void): void;
}

export function createVsCodeTransport(api: VsCodeApiLike, target: MessageTarget = window): WebviewTransport {
  return {
    post: (message) => {
      api.postMessage(message);
    },
    subscribe: (handler) => {
      const listener = (event: { data: unknown }): void => {
        if (isHostToWebviewMessage(event.data)) {
          handler(event.data);
        }
      };
      target.addEventListener('message', listener);
      return () => {
        target.removeEventListener('message', listener);
      };
    },
  };
}
