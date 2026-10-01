/**
 * The connection indicator: the shared three-state control, fed by the shell's state machine.
 *
 * 07 put an ad-hoc cluster in the top bar (a dot, a status word and a Reconnect
 * button) and a banner above the transcript for everything that needs explaining.
 * 08b swaps the cluster for the ported `ConnectionIndicator` — disconnected /
 * connecting / recovered, click to retry — and keeps the banner (it carries what the
 * indicator does not: the unauthorized message, the first-connect guide with the
 * self-signed-certificate hint, the list-refresh failure).
 *
 * The mapping, as the borrow card specifies:
 *
 * - `offline` **after** a connection existed → `disconnected` (something broke);
 *   before the first successful connect there is nothing to say that the banner is
 *   not already saying, so the indicator stays hidden;
 * - `connecting` / `reconnecting` → `connecting`, with the retry countdown folded
 *   into the label ("Reconnecting in 4s…" — the word the old status text showed);
 * - `connected` after an outage → `recovered` for {@link RECOVERED_MS}, then silent;
 *   steady-state connected is silent (the address and the transcript are the UI);
 * - an unauthorized gateway hides it: retrying is pointless and the banner is the
 *   way forward.
 */

import { useEffect, useRef, useState, type ReactElement } from 'react';

import { ConnectionIndicator, type ConnectionIndicatorState } from '@wing-agent/ui';

import type { ConnectionView } from '../connection/runtime';

/** How long the "Reconnected" confirmation stays on screen, in ms. */
export const RECOVERED_MS = 4_000;

export interface ConnectionStatusProps {
  readonly view: ConnectionView;
  readonly onReconnect: () => void;
}

/**
 * The indicator, mounted unconditionally.
 *
 * It is the *component* that decides nothing is worth showing (and that keeps a
 * just-dismissed state on screen for its 150ms exit transition). An early `null`
 * here would unmount it on every quiet phase and turn that transition off.
 */
export function ConnectionStatus({ view, onReconnect }: ConnectionStatusProps): ReactElement {
  const state = useIndicatorState(view);
  return (
    <ConnectionIndicator
      state={state}
      disconnectedLabel="Disconnected"
      connectingLabel={connectingLabel(view)}
      recoveredLabel="Reconnected"
      reconnectActionLabel="Reconnect now"
      restartActionLabel="Restart the connection attempt"
      onReconnect={onReconnect}
    />
  );
}

/** `Connecting…` on the first dial, the countdown while a lost link is retried. */
function connectingLabel(view: ConnectionView): string {
  if (!view.everConnected) {
    return 'Connecting';
  }
  if (view.reconnectInMs === null) {
    return 'Reconnecting';
  }
  return `Reconnecting in ${Math.max(1, Math.ceil(view.reconnectInMs / 1_000))}s`;
}

/**
 * The indicator's state, derived (with the one piece of memory the raw phase lacks:
 * "we were disconnected a moment ago").
 */
export function useIndicatorState(view: ConnectionView): ConnectionIndicatorState | undefined {
  const [recovered, setRecovered] = useState(false);
  // Set when the link is lost, consumed when it comes back: the *first* successful
  // connect is not a recovery, and neither is a manual reconnect from a stopped app.
  const outage = useRef(false);

  useEffect(() => {
    if (view.phase === 'offline' || view.phase === 'reconnecting') {
      outage.current = outage.current || view.everConnected;
      setRecovered(false);
      return;
    }
    if (view.phase === 'connecting') {
      setRecovered(false);
      return;
    }
    if (!outage.current) {
      return;
    }
    outage.current = false;
    setRecovered(true);
    const timer = window.setTimeout(() => {
      setRecovered(false);
    }, RECOVERED_MS);
    return () => {
      window.clearTimeout(timer);
    };
  }, [view.phase, view.everConnected]);

  if (view.unauthorized) {
    return undefined;
  }
  switch (view.phase) {
    case 'offline':
      return view.everConnected ? 'disconnected' : undefined;
    case 'connecting':
    case 'reconnecting':
      return 'connecting';
    default:
      return recovered ? 'recovered' : undefined;
  }
}
