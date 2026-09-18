import type { ReactElement } from 'react';

import { BRIDGE_PROTOCOL_VERSION } from '../../shared';
import { pingHost } from '../bridge/channel';
import { useAppStore } from '../state/appStore';
import { selectActiveSession } from '../state/store';
import styles from '../styles/app.module.css';

import { ScaffoldCell } from './ScaffoldCells';

/**
 * SCAFFOLD(01): the placeholder shell.
 *
 * Proves the whole pipeline end to end — activate → view → document (CSP/nonce)
 * → bundle → `ready` → `hydrate` → cells → `ping`/`pong` — with fixture data and
 * no gateway. Steps 04/05 replace the body with the real chat shell (tab bar,
 * transcript, composer, panels); the bridge contract it renders from does not
 * change.
 */
export function App(): ReactElement {
  const session = useAppStore(selectActiveSession);
  const bridge = useAppStore((state) => state.bridge);
  const tabCount = useAppStore((state) => state.tabs.length);
  const toasts = useAppStore((state) => state.toasts);

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <div className={styles.titleRow}>
          <span className={styles.title} data-testid="session-title">
            {session === null ? 'Wing' : session.title}
          </span>
          <span className={styles.status} data-testid="session-status">
            {session === null ? 'no session' : session.status}
          </span>
        </div>
        <div className={styles.meta}>
          {session === null ? null : (
            <>
              <span data-testid="session-model">
                {session.meta.model}
                {session.meta.provider === '' ? '' : ` · ${session.meta.provider}`}
              </span>
              <span data-testid="session-context">
                {`${session.context.usedTokens} / ${session.context.windowTokens} tokens`}
              </span>
              {tabCount > 1 ? <span>{`${tabCount} tabs`}</span> : null}
            </>
          )}
        </div>
      </header>

      <div className={styles.banner}>
        Scaffold preview — fixture data only. The gateway client (step 02) and the session host (step 03) plug
        into this same bridge.
      </div>

      <main className={styles.transcript} data-testid="transcript">
        {session === null ? (
          <p className={styles.empty} data-testid="empty-state">
            The extension host has not hydrated a session yet.
          </p>
        ) : (
          session.cells.map((cell) => <ScaffoldCell key={cell.id} cell={cell} />)
        )}
      </main>

      {toasts.length === 0 ? null : (
        <div className={styles.toasts} data-testid="toasts">
          {toasts.map((toast) => (
            <div key={toast.id} className={styles.toast} data-toast-level={toast.level}>
              {toast.message}
            </div>
          ))}
        </div>
      )}

      <footer className={styles.footer}>
        <span data-testid="bridge-status">
          {bridge.connection === 'ready' ? 'bridge: ready' : 'bridge: connecting'}
        </span>
        <span data-testid="protocol-version">{`protocol v${BRIDGE_PROTOCOL_VERSION}`}</span>
        {bridge.lastPong === null ? null : (
          <span data-testid="ping-rtt">{`pong in ${bridge.lastPong.rttMs}ms`}</span>
        )}
        <span className={styles.footerSpacer} />
        <button type="button" className={styles.button} data-testid="ping-button" onClick={() => pingHost()}>
          Ping host
        </button>
      </footer>
    </div>
  );
}
