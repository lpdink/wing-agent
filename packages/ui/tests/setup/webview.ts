import '@testing-library/jest-dom/vitest';

import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

import { setBridgeController } from '../../src/bridge/channel';
import { resetCollapseOverrides } from '../../src/chat/interaction';
import { resetImageUris } from '../../src/chat/markdown/image';
import { resetAppStore } from '../../src/state/appStore';

/**
 * jsdom project setup.
 *
 * The webview keeps one app-wide store and one mounted controller (that is what a
 * webview document is), so tests must tear both down between cases — otherwise
 * state leaks from one assertion to the next. The renderer's interaction state
 * (expand/collapse choices) and the image URI cache are module-level for the same
 * reason and are reset with them.
 */
afterEach(() => {
  cleanup();
  setBridgeController(null);
  resetAppStore();
  resetCollapseOverrides();
  resetImageUris();
});
