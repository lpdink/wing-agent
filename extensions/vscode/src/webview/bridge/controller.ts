import type {
  HostToWebviewMessage,
  ResyncReason,
  WebviewToHostMessage,
  WebviewTransport,
} from '../../shared';
import { BRIDGE_PROTOCOL_VERSION, isHostToWebviewMessage, unhandledVariant } from '../../shared';

import type { AppStoreApi } from '../state/store';

/**
 * Bridge controller — the only place that turns host messages into store updates.
 *
 * Responsibilities:
 * - send `ready` on start;
 * - validate incoming payloads (the transport is the document's message channel,
 *   which also carries unrelated platform traffic);
 * - apply each message to the store, and answer `resync` whenever the mirror
 *   cannot follow (never render a half-applied model);
 * - measure ping round-trips.
 *
 * It is deliberately React-free: `mount.tsx` wires it up, tests drive it directly.
 */

export interface BridgeControllerOptions {
  readonly transport: WebviewTransport;
  readonly store: AppStoreApi;
  /**
   * Injectable clock (tests); `undefined` means the wall clock.
   *
   * Explicitly `| undefined` so callers can forward an optional value under
   * `exactOptionalPropertyTypes`.
   */
  readonly now?: (() => number) | undefined;
}

export interface BridgeController {
  /** Subscribe to the transport and announce `ready`. */
  start(): void;
  /** Post one message to the host. */
  post(message: WebviewToHostMessage): void;
  /** Round-trip probe (the `pong` handler records the RTT in the store). */
  ping(): void;
  dispose(): void;
}

export function createBridgeController(options: BridgeControllerOptions): BridgeController {
  const { transport, store } = options;
  const now = options.now ?? (() => Date.now());

  let unsubscribe: (() => void) | null = null;
  let pingSeq = 0;
  const pendingPings = new Map<string, number>();
  // Sessions we already asked to re-hydrate: prevents a resync loop when the host
  // keeps sending patches we cannot apply. Cleared by the matching `hydrate`.
  const resyncInFlight = new Set<string>();

  const post = (message: WebviewToHostMessage): void => {
    transport.post(message);
  };

  const requestResync = (sessionId: string, reason: ResyncReason): void => {
    if (resyncInFlight.has(sessionId)) {
      return;
    }
    resyncInFlight.add(sessionId);
    const session = store.getState().sessions[sessionId];
    post({ type: 'resync', sessionId, lastSeq: session?.seq ?? 0, reason });
  };

  const handle = (message: HostToWebviewMessage): void => {
    if (!isHostToWebviewMessage(message)) {
      // Unreadable payload (platform traffic, or a newer/older peer): resync the
      // active session instead of failing silently. With no session open there is
      // nothing to reconcile.
      const activeSessionId = store.getState().activeSessionId;
      if (activeSessionId !== null) {
        requestResync(activeSessionId, 'protocol');
      }
      return;
    }

    // Any well-formed message proves the channel works: `ready` is about the
    // transport, not about the session state.
    store.getState().markReady(BRIDGE_PROTOCOL_VERSION);

    switch (message.type) {
      case 'hydrate': {
        resyncInFlight.delete(message.session.sessionId);
        store.getState().hydrate(message.session);
        return;
      }
      case 'patch': {
        const outcome = store.getState().applyPatch(message.sessionId, message.seq, message.patches);
        if (!outcome.ok) {
          requestResync(message.sessionId, outcome.reason);
        }
        return;
      }
      case 'state': {
        const outcome = store.getState().applyState(message.state);
        if (!outcome.ok) {
          requestResync(message.state.sessionId, outcome.reason);
        }
        return;
      }
      case 'panels': {
        const outcome = store.getState().applyPanels(message.sessionId, message.panels);
        if (!outcome.ok) {
          requestResync(message.sessionId, outcome.reason);
        }
        return;
      }
      case 'tabs': {
        store.getState().applyTabs(message.tabs, message.activeSessionId);
        return;
      }
      case 'ui': {
        store.getState().applyUi(message.action);
        return;
      }
      case 'pong': {
        const sentAt = pendingPings.get(message.id);
        pendingPings.delete(message.id);
        if (sentAt !== undefined) {
          store.getState().notePong(message.id, Math.max(0, now() - sentAt));
        }
        return;
      }
      default:
        // Compile-time gate: `message` is `never` only while every variant is
        // handled above (adding one to the union breaks this line). At runtime it
        // warns instead of throwing — a channel must survive a newer peer.
        unhandledVariant(message, 'bridge controller');
        return;
    }
  };

  return {
    start: () => {
      unsubscribe ??= transport.subscribe(handle);
      post({ type: 'ready', protocolVersion: BRIDGE_PROTOCOL_VERSION });
    },

    post,

    ping: () => {
      pingSeq += 1;
      const id = `ping-${pingSeq}`;
      pendingPings.set(id, now());
      post({ type: 'ping', id });
    },

    dispose: () => {
      unsubscribe?.();
      unsubscribe = null;
      pendingPings.clear();
      resyncInFlight.clear();
    },
  };
}
