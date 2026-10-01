import { readFile } from 'node:fs/promises';
import { extname, join, resolve, sep } from 'node:path';

/**
 * The shell's application origin: `wing-app://` and the static document server
 * behind it.
 *
 * Why a custom protocol instead of `file://` (the mistake of the July branch):
 *
 * - `file://` pages have an opaque origin, so `fetch()` to the gateway sends
 *   `Origin: null` and the renderer is not a secure context;
 * - with a *standard, secure* scheme the page gets a stable origin
 *   (`wing-app://app`), `window.isSecureContext === true` and working `fetch` /
 *   service-worker semantics.
 *
 * The cost is one line of configuration elsewhere: the gateway's CORS allow-list
 * must include `wing-app://app` **and** the dev server origin
 * (`http://localhost:5173`) — see `docs/dev/web-desktop.md`.
 *
 * `serveWebDocument` is plain Node (`node:fs/promises` + `node:path`), so
 * `tests/web-document.test.ts` exercises the traversal guard, the SPA fallback
 * and the cache headers without an Electron runtime.
 */

export const WING_APP_SCHEME = 'wing-app';

/** The page origin the shell loads in production (`host` segment is fixed: `app`). */
export const WING_APP_ORIGIN = `${WING_APP_SCHEME}://app`;

export const WING_APP_INDEX_URL = `${WING_APP_ORIGIN}/`;

export const WING_APP_INDEX_FILE = 'index.html';

/**
 * Privileges for `protocol.registerSchemesAsPrivileged` (must run before
 * `app.ready`): `standard` + `secure` give the stable, secure origin, `stream`
 * keeps large assets streaming, `codeCache` lets V8 cache the web bundle.
 */
export const WING_APP_SCHEME_PRIVILEGES = {
  standard: true,
  secure: true,
  supportFetchAPI: true,
  corsEnabled: true,
  stream: true,
  codeCache: true,
} as const;

/** Extension → content type for everything a web build can contain. */
const MIME_TYPES: Readonly<Record<string, string>> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.webmanifest': 'application/manifest+json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.otf': 'font/otf',
  '.wasm': 'application/wasm',
  '.txt': 'text/plain; charset=utf-8',
};

/** Vite-style content-hashed asset (`/assets/index-C9s2kQ1d.js`) → immutable. */
const HASHED_ASSET = /^\/assets\/.+-[A-Za-z0-9_-]{8,}\.[a-z0-9]+$/u;

export function contentTypeFor(file: string): string {
  return MIME_TYPES[extname(file).toLowerCase()] ?? 'application/octet-stream';
}

function cacheControlFor(urlPath: string): string {
  if (HASHED_ASSET.test(urlPath)) {
    return 'public, max-age=31536000, immutable';
  }
  return 'no-cache';
}

function response(body: BodyInit | null, status: number, urlPath: string, file: string): Response {
  return new Response(body, {
    status,
    headers: {
      'content-type': contentTypeFor(file),
      'cache-control': cacheControlFor(urlPath),
    },
  });
}

/** Read a file, mapping "not there / not a file" to `null` (404) instead of throwing. */
async function readIfFile(file: string): Promise<Uint8Array<ArrayBuffer> | null> {
  try {
    // Copy into a fresh `ArrayBuffer`-backed view: `Buffer` is
    // `Uint8Array<ArrayBufferLike>`, which `BodyInit` does not accept.
    return new Uint8Array(await readFile(file));
  } catch {
    return null;
  }
}

/**
 * Serve one request from `root` (the packaged `renderer/` directory).
 *
 * - only `GET` / `HEAD` (405 otherwise), undecodable paths are 400;
 * - the resolved target must stay inside `root` (403 otherwise — the guard
 *   Harness' `web-document.ts` uses);
 * - a missing file with an extension is a 404, a missing extension-less path
 *   falls back to `index.html` (SPA routing);
 * - hashed `/assets/…` files are immutable, everything else is `no-cache`.
 */
export async function serveWebDocument(request: Request, root: string): Promise<Response> {
  if (request.method !== 'GET' && request.method !== 'HEAD') {
    return new Response(null, { status: 405 });
  }

  let urlPath: string;
  try {
    urlPath = decodeURIComponent(new URL(request.url).pathname);
  } catch {
    return new Response(null, { status: 400 });
  }

  const directory = resolve(root);
  const relative = urlPath === '/' || urlPath === '' ? `/${WING_APP_INDEX_FILE}` : urlPath;
  const target = resolve(directory, `.${relative}`);
  if (target !== directory && !target.startsWith(directory + sep)) {
    return new Response(null, { status: 403 });
  }

  const body = await readIfFile(target);
  if (body !== null) {
    return response(request.method === 'HEAD' ? null : body, 200, urlPath, target);
  }
  if (extname(urlPath) !== '') {
    return new Response(null, { status: 404 });
  }

  const indexFile = join(directory, WING_APP_INDEX_FILE);
  const index = await readIfFile(indexFile);
  if (index === null) {
    return new Response(null, { status: 404 });
  }
  return response(request.method === 'HEAD' ? null : index, 200, `/${WING_APP_INDEX_FILE}`, indexFile);
}
