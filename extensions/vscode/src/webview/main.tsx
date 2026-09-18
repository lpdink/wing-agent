import { WEBVIEW_ROOT_ID } from '../shared';

import { readBootstrap } from './bootstrap';
import { createVsCodeTransport } from './bridge/vsCodeTransport';
import { mountApp } from './mount';

/**
 * Production webview entry (the bundle `vite.config.mts` builds).
 *
 * Exactly three steps: read the injected bootstrap, connect to the host channel,
 * mount. Everything else lives in the shared app so the preview harness can drive
 * it without VS Code.
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
