/**
 * The browser implementation of {@link ImagePlatform}.
 *
 * Kept apart from the resolver so the resolver stays a node-testable module with an
 * injected seam (no `fetch`, no `Blob`, no `URL.createObjectURL` in its own body).
 *
 * `credentials: 'omit'` is deliberate: the gateway's auth is an API key the settings
 * hold, never a cookie — a cross-origin deployment must not start sending the page's
 * cookies to the gateway just because an image asked.
 */

import type { ImageBytes, ImagePlatform } from './resolver';

export function browserImagePlatform(): ImagePlatform {
  return {
    async load(url: string, options: { readonly apiKey: string | null }): Promise<ImageBytes | null> {
      try {
        const response = await fetch(url, {
          method: 'GET',
          credentials: 'omit',
          headers:
            options.apiKey === null || options.apiKey === ''
              ? {}
              : { Authorization: `Bearer ${options.apiKey}` },
        });
        if (!response.ok) {
          return null;
        }
        const buffer = await response.arrayBuffer();
        return {
          bytes: new Uint8Array(buffer),
          contentType: response.headers.get('content-type') ?? 'application/octet-stream',
        };
      } catch {
        // Network failure, CORS, a certificate the browser would not accept: all of
        // them are "not renderable", which the renderer already knows how to show.
        return null;
      }
    },

    encode(image: ImageBytes): string {
      return URL.createObjectURL(new Blob([image.bytes], { type: image.contentType }));
    },

    release(uri: string): void {
      URL.revokeObjectURL(uri);
    },
  };
}
