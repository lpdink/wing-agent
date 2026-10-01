import { mountApp, readBootstrap } from '@wing-agent/ui';

import { WEBVIEW_ROOT_ID } from '../shared';

import { createVsCodeTransport } from './bridge/vsCodeTransport';

/**
 * Production webview entry (the bundle `vite.config.mts` builds).
 *
 * This file is the extension's whole side of the renderer: it injects the **host
 * bridge** — a `WebviewTransport` implementation over VS Code's `postMessage`
 * channel — into the package's `mountApp`. Everything else (the app, the mirror
 * store, the protocol) lives in `@wing-agent/ui`; there is no protocol logic left
 * here.
 *
 * Three steps: read the injected bootstrap, connect to the host channel, mount.
 */

const bootstrap = readBootstrap();
const rootElement = document.getElementById(WEBVIEW_ROOT_ID);

if (rootElement === null) {
  console.error(`[wing] missing #${WEBVIEW_ROOT_ID} mount point`);
} else if (typeof acquireVsCodeApi !== 'function') {
  // Only reachable if the bundle is loaded outside VS Code (e.g. opened directly
  // in a browser). Say so instead of throwing into a blank page.
  console.error('[wing] acquireVsCodeApi is unavailable — load this bundle through the extension view');
} else {
  mountApp(rootElement, {
    transport: createVsCodeTransport(acquireVsCodeApi()),
  });
  console.debug(`[wing] webview mounted (assets: ${Object.keys(bootstrap.assetUris).length})`);
}
