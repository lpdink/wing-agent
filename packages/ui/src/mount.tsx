import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import type { WebviewTransport } from './protocol';

import { App } from './app/App';
import { setBridgeController } from './bridge/channel';
import { createBridgeController } from './bridge/controller';
import { appStore, resetAppStore } from './state/appStore';
import './styles/tokens.css';

/**
 * Mounts the app against a transport.
 *
 * This is the package's **host-injection seam**: the caller supplies a
 * `WebviewTransport` (the VS Code webview channel in the extension's `main.tsx`, a
 * scripted host in the preview harness and in tests, whatever the web/Electron
 * shells bring next) — so every consumer runs literally the same app.
 */

export interface MountOptions {
  readonly transport: WebviewTransport;
  /** Clear the mirror before mounting (default: true). */
  readonly reset?: boolean;
  /**
   * Clock used by the bridge (ping round-trips). Injected so tests can assert
   * latency deterministically instead of racing the wall clock.
   */
  readonly now?: () => number;
}

export interface MountedApp {
  dispose(): void;
}

export function mountApp(rootElement: HTMLElement, options: MountOptions): MountedApp {
  if (options.reset ?? true) {
    resetAppStore();
  }

  const controller = createBridgeController({
    transport: options.transport,
    store: appStore,
    now: options.now,
  });
  setBridgeController(controller);

  const root = createRoot(rootElement);
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  );

  // Last: the transport is live before the host hears `ready`.
  controller.start();

  return {
    dispose: () => {
      controller.dispose();
      setBridgeController(null);
      root.unmount();
    },
  };
}
