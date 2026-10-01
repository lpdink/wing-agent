/**
 * The connection banner — "what is wrong and what to do about it".
 *
 * Four states worth a banner, in priority order: a rejected API key (nothing
 * works until it is fixed), a first connect that never succeeded (the guide:
 * reason + the settings form — never a blank page), a lost connection with a live
 * retry countdown, and a session list that failed to refresh while live events
 * still flow. Everything else stays quiet: a healthy shell shows no banner.
 */

import type { ReactElement } from 'react';

import type { ConnectionView } from '../connection/runtime';

export interface ConnectionBannerProps {
  readonly view: ConnectionView;
  readonly listError: string | null;
  readonly onOpenSettings: () => void;
  readonly onReconnect: () => void;
}

export function ConnectionBanner({
  view,
  listError,
  onOpenSettings,
  onReconnect,
}: ConnectionBannerProps): ReactElement | null {
  if (view.unauthorized) {
    return (
      <div className="banner banner--error" role="alert">
        <div className="banner__text">
          <strong>The gateway rejected the API key.</strong> Check the key in the gateway settings (and that
          the role allows this client).
        </div>
        <button type="button" className="button" onClick={onOpenSettings}>
          Gateway settings
        </button>
      </div>
    );
  }

  if (!view.everConnected && view.lastError !== null) {
    const custom = !view.address.includes('(this page)');
    return (
      <div className="banner banner--warning" role="alert">
        <div className="banner__text">
          <strong>Cannot reach the gateway at {view.address}.</strong>
          <span className="banner__detail">{view.lastError}</span>
          {custom ? (
            <span className="banner__detail">
              A gateway on another origin has to allow this page — add the address under{' '}
              <code>gateway.cors_origins</code> in <code>~/.wing/config.yaml</code> and restart it. For a
              self-signed HTTPS gateway, open <code>{view.address}</code> in a tab once and accept the
              certificate.
            </span>
          ) : null}
        </div>
        <button type="button" className="button" onClick={onOpenSettings}>
          Set the gateway address
        </button>
        <button type="button" className="button button--ghost" onClick={onReconnect}>
          Retry now
        </button>
      </div>
    );
  }

  if (view.phase === 'reconnecting') {
    return (
      <div className="banner banner--warning" role="status">
        <div className="banner__text">
          <strong>Connection lost.</strong> Reconnecting — the open session is restored from the gateway when
          the link is back.
          {view.lastError === null ? null : <span className="banner__detail">{view.lastError}</span>}
        </div>
        <button type="button" className="button button--ghost" onClick={onReconnect}>
          Reconnect now
        </button>
      </div>
    );
  }

  if (view.phase === 'offline' && view.everConnected) {
    return (
      <div className="banner banner--warning" role="status">
        <div className="banner__text">
          <strong>Disconnected.</strong>
          {view.lastError === null ? null : <span className="banner__detail">{view.lastError}</span>}
        </div>
        <button type="button" className="button" onClick={onReconnect}>
          Reconnect
        </button>
      </div>
    );
  }

  if (view.phase === 'connected' && listError !== null) {
    return (
      <div className="banner banner--warning" role="status">
        <div className="banner__text">
          <strong>Could not refresh the session list.</strong>
          <span className="banner__detail">{listError}</span>
        </div>
      </div>
    );
  }

  return null;
}
