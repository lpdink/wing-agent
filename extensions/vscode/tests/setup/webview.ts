import '@testing-library/jest-dom/vitest';

import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

import { setBridgeController } from '../../src/webview/bridge/channel';
import { resetAppStore } from '../../src/webview/state/appStore';

/**
 * jsdom project setup.
 *
 * The webview keeps one app-wide store and one mounted controller (that is what a
 * webview document is), so tests must tear both down between cases — otherwise
 * state leaks from one assertion to the next.
 */
afterEach(() => {
  cleanup();
  setBridgeController(null);
  resetAppStore();
});
