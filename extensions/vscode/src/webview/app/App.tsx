import type { ReactElement } from 'react';

import { BRIDGE_PROTOCOL_VERSION } from '../../shared';
import { pingHost } from '../bridge/channel';
import { useAppStore } from '../state/appStore';
import { selectActiveSession } from '../state/store';
import { TranscriptView } from './TranscriptView';
import appStyles from '../styles/app.module.css';

/**
 * The webview root.
 *
 * Step 04 owns the transcript; the shell around it is deliberately thin:
 *
 * - {@link AppHeader} is the step 01 placeholder strip (title / status / model /
 *   context). Step 05 replaces it with the real tab bar and status area — it is
 *   kept here so its behaviour (host-driven title and status) stays under test.
 * - {@link BridgeFooter} is a channel diagnostic (ready state, protocol version,
 *   ping round-trip). Also step 05's to replace; it is the only visible proof
 *   that the bridge is alive while the rest of the shell does not exist yet.
 */
export function App(): ReactElement {
  const session = useAppStore(selectActiveSession);
  const toasts = useAppStore((state) => state.toasts);

  return (
    <div className={appStyles.root}>
      <AppHeader />
      <TranscriptView session={session} />
      {toasts.length === 0 ? null : <Toasts />}
      <BridgeFooter />
    </div>
  );
}

function AppHeader(): ReactElement {
  const session = useAppStore(selectActiveSession);
  const tabCount = useAppStore((state) => state.tabs.length);

  return (
    <header className={appStyles.header}>
      <div className={appStyles.titleRow}>
        <span className={appStyles.title} data-testid="session-title">
          {session === null ? 'Wing' : session.title}
        </span>
        {/* Announced to assistive tech when the turn state changes — VS Code's chat
         * loading overlay is a `role="status" aria-live="polite"` region too
         * (chatEditor.ts:199-202). */}
        <span className={appStyles.status} data-testid="session-status" role="status" aria-live="polite">
          {session === null ? 'no session' : session.status}
        </span>
      </div>
      <div className={appStyles.meta}>
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
  );
}

function Toasts(): ReactElement {
  const toasts = useAppStore((state) => state.toasts);
  return (
    <div className={appStyles.toasts} data-testid="toasts">
      {toasts.map((toast) => (
        <div key={toast.id} className={appStyles.toast} data-toast-level={toast.level}>
          {toast.message}
        </div>
      ))}
    </div>
  );
}

function BridgeFooter(): ReactElement {
  const bridge = useAppStore((state) => state.bridge);

  return (
    <footer className={appStyles.footer}>
      <span data-testid="bridge-status">
        {bridge.connection === 'ready' ? 'bridge: ready' : 'bridge: connecting'}
      </span>
      <span data-testid="protocol-version">{`protocol v${BRIDGE_PROTOCOL_VERSION}`}</span>
      {bridge.lastPong === null ? null : (
        <span data-testid="ping-rtt">{`pong in ${bridge.lastPong.rttMs}ms`}</span>
      )}
      <span className={appStyles.footerSpacer} />
      <button type="button" className={appStyles.button} data-testid="ping-button" onClick={() => pingHost()}>
        Ping host
      </button>
    </footer>
  );
}
