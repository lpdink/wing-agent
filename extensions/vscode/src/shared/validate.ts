/**
 * Runtime guards for the bridge.
 *
 * They check the **discriminant tag only**: a message is trusted to be
 * well-formed once its `type` is recognized. That is deliberate — both sides
 * ship in the same VSIX, so field-level re-validation would only add code that
 * can drift; what these guards really catch is garbage (a platform message, a
 * future message from a newer webview, a malformed `postMessage`) that must not
 * crash a reducer.
 *
 * Anything rejected here must be treated as "ignore + log", never as "guess".
 */

import type { HostToWebviewMessage, WebviewToHostMessage } from './bridge';

const HOST_TO_WEBVIEW_TYPES: ReadonlySet<string> = new Set<HostToWebviewMessage['type']>([
  'hydrate',
  'patch',
  'state',
  'panels',
  'tabs',
  'ui',
  'pong',
]);

const WEBVIEW_TO_HOST_TYPES: ReadonlySet<string> = new Set<WebviewToHostMessage['type']>([
  'ready',
  'resync',
  'ping',
  'sendMessage',
  'interrupt',
  'answerAsk',
  'approveTool',
  'newSession',
  'closeSession',
  'activateSession',
  'compact',
  'setModel',
  'setThinking',
  'setEffort',
  'setYolo',
  'runPromptCommand',
  'openModelPicker',
  'closeOverlays',
  'openLink',
  'openFile',
  'openDiff',
  'copyText',
]);

function hasType(value: unknown, known: ReadonlySet<string>): boolean {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const type: unknown = (value as { type?: unknown }).type;
  return typeof type === 'string' && known.has(type);
}

/** True when `value` is a host → webview message this build understands. */
export function isHostToWebviewMessage(value: unknown): value is HostToWebviewMessage {
  return hasType(value, HOST_TO_WEBVIEW_TYPES);
}

/** True when `value` is a webview → host message this build understands. */
export function isWebviewToHostMessage(value: unknown): value is WebviewToHostMessage {
  return hasType(value, WEBVIEW_TO_HOST_TYPES);
}
