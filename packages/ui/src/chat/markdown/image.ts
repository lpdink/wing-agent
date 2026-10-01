/**
 * Image source resolution (webview side).
 *
 * The renderer cannot turn a file path into something an `<img>` may load:
 * `asWebviewUri` exists only in the extension host, which also owns the *policy*
 * (which paths are allowed at all, inside which roots). So every `image` node asks
 * the host once per source and keeps the existing link rendering until — and
 * unless — a URI comes back. A refusal and "no answer yet" are the same thing to
 * the renderer: the behaviour the transcript always had.
 *
 * One module-level cache per webview document, three states per source:
 * absent = not asked yet, `string` = loadable, `null` = the host refused (so it is
 * never asked again). Requests are batched: a paint that shows twenty images
 * produces one message, not twenty.
 */

import type { ResolvedImageModel } from '../../protocol';
import { RESOLVE_IMAGES_MAX_SRCS } from '../../protocol';

import { postToHost } from '../../bridge/channel';

const resolved = new Map<string, string | null>();
const listeners = new Set<() => void>();
let pending: string[] = [];
let scheduled = false;

/** The webview-loadable URI for `src`, or `null` while unknown/unavailable. */
export function imageUri(src: string): string | null {
  return resolved.get(src) ?? null;
}

/**
 * Ask the host about `src` (idempotent — a known or already-queued source is a
 * no-op). Call from an effect: it posts a message and schedules a batch.
 */
export function requestImage(src: string): void {
  if (src === '' || resolved.has(src) || pending.includes(src)) {
    return;
  }
  pending.push(src);
  if (!scheduled) {
    scheduled = true;
    // One message per paint, not per image (a transcript can show many).
    queueMicrotask(flush);
  }
}

/** Record the host's answers and wake every mounted image component. */
export function acceptImageUris(images: readonly ResolvedImageModel[]): void {
  let changed = false;
  for (const image of images) {
    if (resolved.has(image.src) && resolved.get(image.src) === image.uri) {
      continue;
    }
    resolved.set(image.src, image.uri);
    changed = true;
  }
  if (changed) {
    for (const listener of [...listeners]) {
      listener();
    }
  }
}

/** Subscribe to cache updates; returns the unsubscribe function. */
export function subscribeImages(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Test hook: the cache is document state, exactly like the app store. */
export function resetImageUris(): void {
  resolved.clear();
  listeners.clear();
  pending = [];
  scheduled = false;
}

function flush(): void {
  scheduled = false;
  const srcs = pending;
  pending = [];
  // One request carries at most `RESOLVE_IMAGES_MAX_SRCS` sources (the host walks the
  // list, so the protocol caps it): a transcript with more images sends follow-up
  // batches instead of losing the tail.
  for (let start = 0; start < srcs.length; start += RESOLVE_IMAGES_MAX_SRCS) {
    postToHost({ type: 'resolveImages', srcs: srcs.slice(start, start + RESOLVE_IMAGES_MAX_SRCS) });
  }
}
