/**
 * Wires the shared renderer to this app: one bridge controller, one image resolver,
 * for the lifetime of the mounted shell.
 *
 * Three things happen here, in this order of importance:
 *
 * 1. **The controller goes in.** `setBridgeController` is the package's documented
 *    injection point (`postToHost` is a no-op until something is mounted), and the
 *    renderer's intents are routed by `src/bridge/webBridge.ts` — ask answers to the
 *    runtime, images to the gateway, the browser-only intents to the browser.
 * 2. **Image answers are per session.** The renderer's image cache is document-level
 *    and keyed by the markdown source alone (`chat/markdown/image.ts`), which is
 *    exactly right inside one VS Code window (one workspace) and not right here: two
 *    sessions can live in different working directories, so `assets/chart.png` is two
 *    different files. On a session switch both caches are dropped — the resolver's own
 *    (object URLs revoked) and the renderer's — and `SessionPane` remounts the
 *    transcript (`key={sessionId}`) so every image asks again, against the new session.
 *    The reset runs from the runtime's own notification, i.e. strictly before React
 *    renders the new session's cells.
 * 3. **Everything is released on unmount** (a root that goes away must not leave a
 *    controller or blob URLs behind).
 */

import { useEffect } from 'react';

import { consoleLogger } from '@wing-agent/client';
import { resetImageUris, setBridgeController } from '@wing-agent/ui';

import { createWebBridge } from '../bridge/webBridge';
import type { GatewayRuntime } from '../connection/runtime';
import { browserImagePlatform } from '../images/browser-platform';
import { createImageResolver } from '../images/resolver';
import { imageTarget } from '../images/target';

export function useWebBridge(runtime: GatewayRuntime): void {
  useEffect(() => {
    const images = createImageResolver({
      target: () => {
        const snapshot = runtime.getSnapshot();
        return imageTarget({
          settings: snapshot.settings,
          location: runtime.location,
          sessionId: snapshot.activeSessionId,
        });
      },
      platform: browserImagePlatform(),
    });

    const controller = createWebBridge({
      images,
      logger: consoleLogger,
      host: {
        answerAsk: (requestId, answers) => {
          runtime.answerAsk(requestId, answers);
        },
        approveTool: (requestId, decision) => {
          runtime.approveTool(requestId, decision);
        },
        notify: (level, text) => {
          runtime.pushUserNotice(level, text);
        },
      },
    });
    setBridgeController(controller);

    let sessionId = runtime.getSnapshot().activeSessionId;
    const unsubscribe = runtime.subscribe(() => {
      const next = runtime.getSnapshot().activeSessionId;
      if (next === sessionId) {
        return;
      }
      sessionId = next;
      images.clear();
      // The renderer's cache is the package's, and its entries are keyed by source:
      // dropping it is the only way a new session re-asks (see the file comment).
      resetImageUris();
    });

    return () => {
      unsubscribe();
      setBridgeController(null);
      images.clear();
    };
  }, [runtime]);
}
