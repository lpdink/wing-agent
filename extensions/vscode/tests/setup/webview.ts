import '@testing-library/jest-dom/vitest';

import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

import { resetAppStore, resetCollapseOverrides, resetImageUris, setBridgeController } from '@wing-agent/ui';

/**
 * jsdom project setup for the extension's remaining webview-side test.
 *
 * The renderer now lives in `@wing-agent/ui` (and so does the same teardown, in
 * `packages/ui/tests/setup/webview.ts`): this file is only here because the preview
 * harness — the one webview-side artefact that stayed in the extension — mounts the
 * app under this project's jsdom. It keeps the document-level singletons from leaking
 * between cases exactly like the package's copy does.
 */
afterEach(() => {
  cleanup();
  setBridgeController(null);
  resetAppStore();
  resetCollapseOverrides();
  resetImageUris();
});
