import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';

import type { WebviewTransport } from '../shared';

import { App } from './app/App';
import { setBridgeController } from './bridge/channel';
import { createBridgeController } from './bridge/controller';
import { appStore, resetAppStore } from './state/appStore';

/**
 * Mounts the app against a transport.
 *
 * Shared by the production entry (`main.tsx`, VS Code channel) and the preview
 * harness (`preview/main.tsx`, mock channel) — so what the preview shows is
 * literally the same code that runs in the sidebar.
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
