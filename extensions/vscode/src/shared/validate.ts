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

/**
 * Known tags, typed as `Record<…['type'], true>` on purpose: a `Set<string>` only
 * checks the *element* type, so a variant missing from the list compiles fine and
 * is then silently dropped at runtime (the guard rejects it). A record makes both
 * a missing and an extra key a compile error, which is what keeps these tables in
 * lockstep with the unions in `./bridge`.
 */
const HOST_TO_WEBVIEW_TYPES: Record<HostToWebviewMessage['type'], true> = {
  hydrate: true,
  patch: true,
  state: true,
  panels: true,
  tabs: true,
  ui: true,
  pong: true,
};

const WEBVIEW_TO_HOST_TYPES: Record<WebviewToHostMessage['type'], true> = {
  ready: true,
  resync: true,
  ping: true,
  sendMessage: true,
  interrupt: true,
  answerAsk: true,
  approveTool: true,
  newSession: true,
  closeSession: true,
  activateSession: true,
  compact: true,
  setModel: true,
  setThinking: true,
  setEffort: true,
  setYolo: true,
  runPromptCommand: true,
  openModelPicker: true,
  closeOverlays: true,
  openLink: true,
  openFile: true,
  openDiff: true,
  copyText: true,
};

function hasType(value: unknown, known: Record<string, true>): boolean {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const type: unknown = (value as { type?: unknown }).type;
  return typeof type === 'string' && Object.hasOwn(known, type);
}

/** True when `value` is a host → webview message this build understands. */
export function isHostToWebviewMessage(value: unknown): value is HostToWebviewMessage {
  return hasType(value, HOST_TO_WEBVIEW_TYPES);
}

/** True when `value` is a webview → host message this build understands. */
export function isWebviewToHostMessage(value: unknown): value is WebviewToHostMessage {
  return hasType(value, WEBVIEW_TO_HOST_TYPES);
}
