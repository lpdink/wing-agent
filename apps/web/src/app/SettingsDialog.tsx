/**
 * The gateway settings form (the minimal one — step 09 owns the full panel).
 *
 * Five fields, one preview and one honest note:
 *
 * - `scheme` / `host` / `port` / `apiKey` — the connection itself; an empty host
 *   means "this page's origin" (the dev proxy and the gateway-hosted build), which
 *   is why it is the default and the placeholder says so;
 * - `ignoreCertErrors` — stored for real and consumed by the Electron shell; a
 *   *browser* cannot bypass certificate validation, so the form says what to do
 *   instead of pretending otherwise (design.md D9);
 * - the preview shows the exact URLs the app will use, or the reason the address
 *   cannot be used yet — validated here so a typo never silently becomes the
 *   default address;
 * - when the browser refused persistent storage, the dialog says the settings will
 *   be forgotten instead of showing a saved state that is not there.
 */

import { useCallback, useEffect, useState, type FormEvent, type ReactElement } from 'react';

import type { ConnectionView } from '../connection/runtime';
import { type GatewayScheme, type GatewaySettings, normalizePort } from '../settings/settings';
import {
  GatewayAddressError,
  type PageLocation,
  gatewayEndpoints,
  normalizeGatewayHost,
} from '../settings/urls';

export interface SettingsDialogProps {
  readonly settings: GatewaySettings;
  readonly location: PageLocation;
  readonly view: ConnectionView;
  readonly persistent: boolean;
  readonly onSave: (settings: GatewaySettings) => void;
  readonly onClose: () => void;
}

interface Preview {
  readonly endpoints: string | null;
  readonly error: string | null;
}

function previewEndpoints(
  draft: {
    scheme: GatewayScheme;
    host: string;
    port: string;
    apiKey: string | null;
    ignoreCertErrors: boolean;
  },
  location: PageLocation,
): Preview {
  const port = normalizePort(draft.port);
  if (port === null) {
    return { endpoints: null, error: 'Enter a port between 1 and 65535.' };
  }
  try {
    const endpoints = gatewayEndpoints(
      {
        scheme: draft.scheme,
        host: draft.host,
        port,
        apiKey: null,
        ignoreCertErrors: draft.ignoreCertErrors,
      },
      location,
    );
    return { endpoints: `${endpoints.httpBaseUrl} · ${endpoints.wsUrl}`, error: null };
  } catch (error) {
    return {
      endpoints: null,
      error: error instanceof GatewayAddressError ? error.message : String(error),
    };
  }
}

export function SettingsDialog({
  settings,
  location,
  view,
  persistent,
  onSave,
  onClose,
}: SettingsDialogProps): ReactElement {
  const [scheme, setScheme] = useState<GatewayScheme>(settings.scheme);
  const [host, setHost] = useState(settings.host);
  const [port, setPort] = useState(String(settings.port));
  const [apiKey, setApiKey] = useState(settings.apiKey ?? '');
  const [ignoreCertErrors, setIgnoreCertErrors] = useState(settings.ignoreCertErrors);
  const [error, setError] = useState<string | null>(null);

  // Esc key handling.
  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        onClose();
      }
    };
    document.addEventListener('keydown', handleKeyDown);
    return () => {
      document.removeEventListener('keydown', handleKeyDown);
    };
  }, [onClose]);

  // Overlay click closes the dialog (click on the overlay background only).
  const handleOverlayClick = useCallback(
    (event: React.MouseEvent): void => {
      if (event.target === event.currentTarget) {
        onClose();
      }
    },
    [onClose],
  );

  const preview = previewEndpoints({ scheme, host, port, apiKey: null, ignoreCertErrors }, location);
  // With an empty host the app uses the page's own origin: the scheme and the port
  // are then not "settings", they are whatever served this page (design.md D3).
  const sameOrigin = host.trim() === '';

  const submit = (event: FormEvent): void => {
    event.preventDefault();
    const normalizedPort = normalizePort(port);
    if (normalizedPort === null) {
      setError('Enter a port between 1 and 65535.');
      return;
    }
    try {
      normalizeGatewayHost(host);
    } catch (hostError) {
      setError(hostError instanceof GatewayAddressError ? hostError.message : String(hostError));
      return;
    }
    setError(null);
    onSave({
      scheme,
      host: host.trim(),
      port: normalizedPort,
      apiKey: apiKey.trim() === '' ? null : apiKey.trim(),
      ignoreCertErrors,
    });
  };

  return (
    <div className="overlay" role="dialog" aria-modal="true" aria-label="Gateway settings" onClick={handleOverlayClick}>
      <form className="dialog" onSubmit={submit} onClick={(event) => { event.stopPropagation(); }}>
        <h2 className="dialog__title">Gateway connection</h2>

        <p className="dialog__state">
          {view.phase === 'connected' ? 'Connected to ' : 'Not connected — target '}
          <code>{preview.endpoints ?? view.address}</code>
          {view.lastError === null ? null : <span className="dialog__reason">{view.lastError}</span>}
        </p>

        <div className="field">
          <label className="field__label" htmlFor="settings-scheme">
            Scheme
          </label>
          <select
            id="settings-scheme"
            className="field__control"
            value={scheme}
            disabled={sameOrigin}
            onChange={(event) => {
              setScheme(event.target.value === 'https' ? 'https' : 'http');
            }}
          >
            <option value="http">http</option>
            <option value="https">https</option>
          </select>
        </div>

        <div className="field">
          <label className="field__label" htmlFor="settings-host">
            Host
          </label>
          <input
            id="settings-host"
            className="field__control"
            value={host}
            placeholder="(this page's origin)"
            onChange={(event) => {
              setHost(event.target.value);
            }}
          />
          <span className="field__hint">
            Leave empty to use the origin this page was served from (the gateway's own hosting, or the dev
            server's proxy).
          </span>
        </div>

        <div className="field">
          <label className="field__label" htmlFor="settings-port">
            Port
          </label>
          <input
            id="settings-port"
            className="field__control"
            value={port}
            disabled={sameOrigin}
            inputMode="numeric"
            onChange={(event) => {
              setPort(event.target.value);
            }}
          />
          {sameOrigin ? (
            <span className="field__hint">
              Not used while the host is empty — the page's own origin already carries the port.
            </span>
          ) : null}
        </div>

        <div className="field">
          <label className="field__label" htmlFor="settings-api-key">
            API key
          </label>
          <input
            id="settings-api-key"
            className="field__control"
            type="password"
            value={apiKey}
            placeholder="(no auth)"
            onChange={(event) => {
              setApiKey(event.target.value);
            }}
          />
          <span className="field__hint">
            Sent as an <code>Authorization</code> header for HTTP; the browser cannot set WebSocket headers,
            so the socket carries it as a query parameter instead.
          </span>
        </div>

        <div className="field field--check">
          <input
            id="settings-ignore-cert"
            type="checkbox"
            checked={ignoreCertErrors}
            onChange={(event) => {
              setIgnoreCertErrors(event.target.checked);
            }}
          />
          <label className="field__label" htmlFor="settings-ignore-cert">
            Ignore certificate errors
          </label>
        </div>
        {ignoreCertErrors ? (
          <p className="dialog__hint">
            A browser cannot skip its own certificate validation. Open <code>{preview.endpoints ?? ''}</code>{' '}
            in a tab once and accept the certificate — after that this page can connect (the desktop app
            honours this switch without that step).
          </p>
        ) : null}

        <p className={preview.error === null ? 'dialog__preview' : 'dialog__preview dialog__preview--error'}>
          {preview.error ?? `Gateway: ${preview.endpoints ?? ''}`}
        </p>

        {!persistent ? (
          <p className="dialog__warn">
            This browser is not letting the page store settings, so they last until the page is closed.
          </p>
        ) : null}

        {error === null ? null : (
          <p className="dialog__error" role="alert">
            {error}
          </p>
        )}

        <div className="dialog__actions">
          <button type="submit" className="button button--primary" disabled={preview.error !== null}>
            Save &amp; reconnect
          </button>
          <button type="button" className="button button--ghost" onClick={onClose}>
            Cancel
          </button>
        </div>
      </form>
    </div>
  );
}
