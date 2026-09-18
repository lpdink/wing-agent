import type {
  HostToWebviewMessage,
  SessionViewModel,
  TabModel,
  WebviewToHostMessage,
  WebviewTransport,
} from '../shared';

import { makeFixtureSession, makeTab } from './fixtures';

/**
 * A scripted host for the webview: the preview harness and the component tests
 * drive the real controller through this transport.
 *
 * It mirrors the protocol parts of `src/host/*` that step 03 will implement for
 * real: answer `ready` with `tabs` + `hydrate`, answer `ping` with `pong`, and
 * answer `resync` with a fresh `hydrate`. Everything else is recorded so tests and
 * the preview toolbar can assert/inspect what the renderer asked for.
 */

export interface MockBridgeOptions {
  /** Sessions the scripted host knows about (first one is hydrated on `ready`). */
  readonly sessions?: readonly SessionViewModel[];
  /** Answer protocol messages automatically (default: true). */
  readonly autoHandshake?: boolean;
  /** Injectable clock for `pong` (and for controllers built with `mock.now`). */
  readonly now?: () => number;
}

export interface MockBridge {
  readonly transport: WebviewTransport;
  /** Every message the webview posted, in order. */
  readonly sent: readonly WebviewToHostMessage[];
  /** Push a message as if the extension host sent it. */
  push(message: HostToWebviewMessage): void;
  /** Messages of one type (typed narrowing for assertions). */
  sentOfType<T extends WebviewToHostMessage['type']>(
    type: T,
  ): readonly Extract<WebviewToHostMessage, { type: T }>[];
  /** Forget recorded messages (keeps subscriptions and sessions). */
  clearSent(): void;
}

export function createMockBridge(options: MockBridgeOptions = {}): MockBridge {
  const now = options.now ?? (() => Date.now());
  const autoHandshake = options.autoHandshake ?? true;
  const sessions = new Map<string, SessionViewModel>();
  for (const session of options.sessions ?? [makeFixtureSession()]) {
    sessions.set(session.sessionId, session);
  }

  const handlers = new Set<(message: HostToWebviewMessage) => void>();
  const sent: WebviewToHostMessage[] = [];

  const push = (message: HostToWebviewMessage): void => {
    for (const handler of [...handlers]) {
      handler(message);
    }
  };

  const tabsOf = (): readonly TabModel[] => [...sessions.values()].map(makeTab);

  const hydrateAll = (): void => {
    push({ type: 'tabs', tabs: tabsOf(), activeSessionId: [...sessions.keys()][0] ?? null });
    for (const session of sessions.values()) {
      push({ type: 'hydrate', session });
    }
  };

  const transport: WebviewTransport = {
    post: (message) => {
      sent.push(message);
      if (!autoHandshake) {
        return;
      }
      switch (message.type) {
        case 'ready':
          hydrateAll();
          return;
        case 'ping':
          push({ type: 'pong', id: message.id, hostTimeMs: now() });
          return;
        case 'resync': {
          const session = sessions.get(message.sessionId);
          if (session !== undefined) {
            push({ type: 'hydrate', session });
          }
          return;
        }
        default:
          return;
      }
    },
    subscribe: (handler) => {
      handlers.add(handler);
      return () => {
        handlers.delete(handler);
      };
    },
  };

  return {
    transport,
    get sent() {
      return sent;
    },
    push,
    sentOfType: (type) =>
      sent.filter(
        (message): message is Extract<WebviewToHostMessage, { type: typeof type }> => message.type === type,
      ),
    clearSent: () => {
      sent.length = 0;
    },
  };
}
