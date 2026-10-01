/**
 * Workspace images for the transcript (`apps/web`'s side of step 03's endpoint).
 *
 * The renderer never turns a path into something `<img>` can load — it asks its host
 * and keeps the plain link until an answer arrives (`@wing-agent/ui`'s
 * `chat/markdown/image.ts`). In VS Code the host answers with `asWebviewUri`; in a
 * browser there is no such scheme, so this resolver answers with the gateway:
 *
 * 1. `policy.ts` pre-filters the source (only real image paths get a request);
 * 2. the bytes are fetched from `GET /api/workspace/image` — with the `Authorization`
 *    header, which is why this is a `fetch` and not an `<img src>`: a browser cannot
 *    put a header on an image request, and the gateway's HTTP auth accepts nothing
 *    else (`gateway/auth.py`). It also decides the three states *before* the renderer
 *    sees them, instead of leaving a broken icon to `onError`;
 * 3. the bytes become a `blob:` object URL, whose lifetime this module owns
 *    (LRU-bounded, revoked on eviction and on `clear()`).
 *
 * The three states match the VS Code host one-for-one: never asked (absent),
 * loadable (a uri), refused (`null` — the renderer keeps its link, and never asks
 * again).
 *
 * Refusals are *permanent* here, exactly like in the extension (a refused source is
 * cached as `null`). The one case that is deliberately left unanswered is "there is
 * no session to ask about": an empty answer leaves the source unknown, so the next
 * mount asks again — which is what a session switch produces.
 *
 * ## Epochs: answers that belong to a session nobody is looking at any more
 *
 * `clear()` is the session switch (see `src/app/useWebBridge.ts`), and a fetch that
 * was in flight when it happened must not come back: the renderer's own image cache
 * is document-level and keyed by the markdown source *alone*, so a late answer for
 * `assets/chart.png` would overwrite the new session's picture with the old
 * workspace's file — silently, and for as long as the switch lasts. A fetch cannot be
 * cancelled, so it is *fenced*: every `clear()` bumps a document epoch, an in-flight
 * load records the epoch it started under, and a result that comes back under an
 * older one is dropped — nothing is encoded, nothing is cached, nothing is handed to
 * the renderer (the object URL that was never created cannot leak either). The same
 * fence is what keeps a stale answer from being written back into the cache after it
 * was cleared.
 */

import type { ResolvedImageModel } from '@wing-agent/ui';

import { workspaceImagePath } from './policy';
import { workspaceImageUrl } from './url';

/** Bytes of one image, plus what the gateway said they are. */
export interface ImageBytes {
  /** A view whose backing store is a plain `ArrayBuffer` (what `fetch` hands out). */
  readonly bytes: Uint8Array<ArrayBuffer>;
  readonly contentType: string;
}

/**
 * The platform seam: everything that needs a browser, injected.
 *
 * `load` returns `null` for every "not renderable" outcome (HTTP error, network
 * failure, blocked certificate, …) — the resolver never needs to know which.
 */
export interface ImagePlatform {
  load(url: string, options: { readonly apiKey: string | null }): Promise<ImageBytes | null>;
  /** Turn bytes into something an `<img>` can load (`blob:` in the browser). */
  encode(image: ImageBytes): string;
  /** Free an encoded URI (a no-op where the platform has no lifetime). */
  release(uri: string): void;
}

/** Whose workspace the paths belong to, plus how to reach the gateway. */
export interface ImageTarget {
  readonly baseUrl: string;
  readonly apiKey: string | null;
  readonly sessionId: string;
}

export interface ImageResolverOptions {
  /** Read the current target (session switch / settings change) at request time. */
  readonly target: () => ImageTarget | null;
  readonly platform: ImagePlatform;
  /** How many resolved sources stay cached (LRU); the rest are released. */
  readonly maxEntries?: number;
}

export interface ImageResolver {
  /** One answer per source, in request order (sources that cannot be asked about are omitted). */
  resolve(srcs: readonly string[]): Promise<readonly ResolvedImageModel[]>;
  /** Drop every cached answer (object URLs released). */
  clear(): void;
}

export const DEFAULT_MAX_IMAGE_ENTRIES = 64;

/**
 * "This answer belongs to a cleared document" — a sentinel, so it can never be
 * confused with a real answer (`null` is the *refusal*, which the renderer caches as
 * final).
 */
const STALE = Symbol('stale-image-answer');

export function createImageResolver(options: ImageResolverOptions): ImageResolver {
  const maxEntries = Math.max(1, options.maxEntries ?? DEFAULT_MAX_IMAGE_ENTRIES);
  /** `session + src` → object URL or `null` (refused). Insertion order = LRU order. */
  const cache = new Map<string, string | null>();
  /** `session + src` → in-flight load, so two asks for one source fetch once. */
  const inFlight = new Map<string, Promise<string | null | typeof STALE>>();
  /** Bumped by `clear()`; a load that started under another epoch is dropped. */
  let epoch = 0;

  const remember = (key: string, uri: string | null): void => {
    cache.delete(key);
    cache.set(key, uri);
    while (cache.size > maxEntries) {
      const oldest = cache.keys().next();
      if (oldest.done === true) {
        return;
      }
      const evicted = cache.get(oldest.value) ?? null;
      cache.delete(oldest.value);
      if (evicted !== null) {
        options.platform.release(evicted);
      }
    }
  };

  const resolveOne = async (target: ImageTarget, src: string): Promise<string | null | typeof STALE> => {
    const path = workspaceImagePath(src);
    if (path === null) {
      return null;
    }
    const key = `${target.sessionId}\u0000${src}`;
    if (cache.has(key)) {
      return cache.get(key) ?? null;
    }
    const existing = inFlight.get(key);
    if (existing !== undefined) {
      return existing;
    }
    const startedEpoch = epoch;
    const pending: Promise<string | null | typeof STALE> = options.platform
      .load(workspaceImageUrl(target.baseUrl, target.sessionId, path), { apiKey: target.apiKey })
      // A failed load is a *refusal* (`null`, final for this document) — unless the
      // document moved on, which the fence right after decides.
      .catch((): null => null)
      .then((image): string | null | typeof STALE => {
        // The document moved on while this was in flight — see the module doc. The
        // result is dropped *before* it becomes an object URL or a cache entry, so a
        // stale answer can neither be cached (N4) nor reach the renderer (S1) — and
        // there is no object URL to leak, not even for a stale failure.
        if (epoch !== startedEpoch) {
          return STALE;
        }
        return image === null ? null : options.platform.encode(image);
      })
      .then((uri) => {
        // Only drop the in-flight entry if it is still *this* load: a `clear()` plus a
        // new ask for the same key may have registered a newer one in the meantime.
        if (inFlight.get(key) === pending) {
          inFlight.delete(key);
        }
        if (uri === STALE) {
          return STALE;
        }
        remember(key, uri);
        return uri;
      });
    inFlight.set(key, pending);
    return pending;
  };

  return {
    async resolve(srcs: readonly string[]): Promise<readonly ResolvedImageModel[]> {
      const target = options.target();
      if (target === null || srcs.length === 0) {
        // No session (or an address the settings cannot produce): answer nothing.
        // The renderer keeps the link and asks again on the next mount.
        return [];
      }
      const answers = await Promise.all(
        srcs.map(async (src) => ({ src, uri: await resolveOne(target, src) })),
      );
      // A fenced answer is simply absent: the renderer keeps the link (it has no
      // answer) and the source is *not* cached as refused, so a later mount may ask
      // again — which is exactly what the session switch's remount does.
      return answers.filter((answer): answer is ResolvedImageModel => answer.uri !== STALE);
    },

    clear(): void {
      // Fence off everything already in flight (module doc) before dropping the caches.
      epoch += 1;
      for (const uri of cache.values()) {
        if (uri !== null) {
          options.platform.release(uri);
        }
      }
      cache.clear();
      inFlight.clear();
    },
  };
}
