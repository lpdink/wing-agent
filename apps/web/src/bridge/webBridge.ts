/**
 * The web shell's bridge controller: `@wing-agent/ui` components → this app.
 *
 * The package's renderer talks to its host through one narrow channel
 * (`postToHost`, `packages/ui/src/bridge/channel.ts`): it *asks* for things and never
 * touches the app. In VS Code that host is the extension; here it is `GatewayRuntime`
 * plus a couple of browser APIs. This module is the adapter, and it is deliberately
 * the **narrow version of the package's own controller**: `createBridgeController`
 * exists to feed the package's mirror store (`hydrate` / `patch` / `state` / …) and
 * its `App`, which this shell does not use — the web app reduces the gateway's events
 * itself (`src/connection/runtime.ts`, step 07) and renders only the transcript.
 * Implementing the same `BridgeController` interface keeps the seam identical
 * (`setBridgeController` is the documented injection point).
 *
 * Only the intents the transcript can produce are wired (see the table in the step's
 * `design.md`, D5). Everything else (send, interrupt, panels, …) is step 09's: those
 * messages cannot be produced yet, so they are logged and dropped rather than guessed
 * into a half-behaviour.
 */

import {
  acceptImageUris,
  unhandledVariant,
  type BridgeController,
  type ToolApprovalDecision,
  type WebviewToHostMessage,
} from '@wing-agent/ui';
import { consoleLogger, type CoreLogger } from '@wing-agent/client';
import type { AskAnswerModel } from '@wing-agent/session';

import type { ImageResolver } from '../images/resolver';

/** What the bridge needs from the app (implemented by `GatewayRuntime`, D6). */
export interface WebBridgeHost {
  /** Answer an awaiting question form. */
  answerAsk(requestId: string, answers: readonly AskAnswerModel[]): void;
  /** Approve / deny a dangerous-command confirmation. */
  approveTool(requestId: string, decision: ToolApprovalDecision): void;
  /** User-visible message (the runtime's notice stack). */
  notify(level: 'info' | 'warning' | 'error', text: string): void;
}

export interface WebBridgeOptions {
  readonly host: WebBridgeHost;
  readonly images: ImageResolver;
  /** Open an http(s) link (defaults to a new tab). */
  readonly openLink?: (href: string) => void;
  /** Copy text to the clipboard (defaults to `navigator.clipboard`). */
  readonly copyText?: (text: string) => void;
  readonly logger?: CoreLogger;
}

/** http(s) only: a transcript link can also be a local image path (the link fallback). */
function isExternalLink(href: string): boolean {
  return /^https?:\/\//i.test(href);
}

export function createWebBridge(options: WebBridgeOptions): BridgeController {
  const logger = options.logger ?? consoleLogger;
  const openLink = options.openLink ?? ((href: string) => void globalThis.open(href, '_blank', 'noopener'));
  const copyText =
    options.copyText ??
    ((text: string) => {
      void globalThis.navigator?.clipboard?.writeText(text).catch((error: unknown) => {
        logger.warn('could not write to the clipboard', error);
      });
    });

  return {
    // Nothing to subscribe to (the runtime pushes through React) and nothing to
    // hand-shake: `ready` exists for a host that has to hydrate a fresh document,
    // and `ping` probes a channel this app does not use.
    start: () => undefined,
    ping: () => undefined,
    dispose: () => undefined,

    post: (message: WebviewToHostMessage): void => {
      switch (message.type) {
        case 'resolveImages': {
          void options.images
            .resolve(message.srcs)
            .then(acceptImageUris)
            .catch((error: unknown) => {
              logger.warn('image resolution failed', error);
            });
          return;
        }

        case 'answerAsk': {
          options.host.answerAsk(message.requestId, message.answers);
          return;
        }

        case 'approveTool': {
          options.host.approveTool(message.requestId, message.decision);
          return;
        }

        case 'openLink': {
          // The image fallback renders the source path as a link; a path cannot be
          // "opened" in a browser tab, so it stays an inert click (it is already
          // readable as the link text).
          if (isExternalLink(message.href)) {
            openLink(message.href);
          } else {
            logger.debug(`ignoring a non-http link "${message.href}"`);
          }
          return;
        }

        case 'copyText': {
          copyText(message.text);
          return;
        }

        case 'openFile': {
          // No editor in a browser (design.md D7-1): say what the user would open
          // instead of leaving a dead control.
          const line = message.line === null ? '' : `:${message.line}`;
          options.host.notify('info', `Open in your editor: ${message.path}${line}`);
          return;
        }

        case 'openDiff': {
          options.host.notify(
            'info',
            'The diff is shown inline here — the editor’s diff view is not available in the browser.',
          );
          return;
        }

        // ── Not wired in this step — nothing can emit them yet (09 does) ──────
        case 'ready':
        case 'sendMessage':
        case 'interrupt':
        case 'newSession':
        case 'activateSession':
        case 'closeSession':
        case 'setModel':
        case 'setThinking':
        case 'setEffort':
        case 'setYolo':
        case 'runPromptCommand':
        case 'openModelPicker':
        case 'compact':
        case 'closeOverlays':
        case 'resync':
        case 'ping': {
          logger.debug(`web bridge: "${message.type}" belongs to a later step; ignored`);
          return;
        }

        default:
          // Compile-time gate: `message` is `never` here only while every variant is
          // handled above. At runtime a newer renderer must not break the shell.
          unhandledVariant(message, 'web bridge');
          return;
      }
    },
  };
}
