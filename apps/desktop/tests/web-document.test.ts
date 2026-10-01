import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  WING_APP_INDEX_URL,
  WING_APP_ORIGIN,
  WING_APP_SCHEME,
  contentTypeFor,
  serveWebDocument,
} from '../src/web-document';

/**
 * The `wing-app://` document server is the shell's only file-serving surface, so
 * the guards (method gate, traversal, SPA fallback, cache headers) are pinned
 * here rather than discovered in a production window.
 */

let root: string;
const temps: string[] = [];

const INDEX_HTML = '<!doctype html><html><body>wing</body></html>';

beforeEach(() => {
  root = mkdtempSync(path.join(tmpdir(), 'wing-desktop-renderer-'));
  temps.push(root);
  writeFileSync(path.join(root, 'index.html'), INDEX_HTML);
  mkdirSync(path.join(root, 'assets'));
  writeFileSync(path.join(root, 'assets', 'index-C9s2kQ1d.js'), 'console.log(1);');
  writeFileSync(path.join(root, 'styles.css'), 'body{}');
});

afterEach(() => {
  while (temps.length > 0) {
    const directory = temps.pop();
    if (directory !== undefined) {
      rmSync(directory, { recursive: true, force: true });
    }
  }
});

function request(url: string, method = 'GET'): Request {
  return new Request(url, { method });
}

describe('the scheme contract', () => {
  it('is a standard, secure origin so the page has a real origin and a secure context', () => {
    expect(WING_APP_SCHEME).toBe('wing-app');
    expect(WING_APP_ORIGIN).toBe('wing-app://app');
    expect(WING_APP_INDEX_URL).toBe('wing-app://app/');
  });
});

describe('contentTypeFor', () => {
  it('maps the file types a web build ships', () => {
    expect(contentTypeFor('/x/index.html')).toBe('text/html; charset=utf-8');
    expect(contentTypeFor('/x/index.js')).toContain('text/javascript');
    expect(contentTypeFor('/x/a.WOFF2')).toBe('font/woff2');
    expect(contentTypeFor('/x/logo.svg')).toBe('image/svg+xml');
    expect(contentTypeFor('/x/blob.bin')).toBe('application/octet-stream');
  });
});

describe('serveWebDocument', () => {
  it('serves index.html for the root, without caching it', async () => {
    const response = await serveWebDocument(request(WING_APP_INDEX_URL), root);
    expect(response.status).toBe(200);
    expect(response.headers.get('content-type')).toBe('text/html; charset=utf-8');
    expect(response.headers.get('cache-control')).toBe('no-cache');
    await expect(response.text()).resolves.toBe(INDEX_HTML);
  });

  it('serves static assets with their content type', async () => {
    const css = await serveWebDocument(request(`${WING_APP_ORIGIN}/styles.css`), root);
    expect(css.status).toBe(200);
    expect(css.headers.get('content-type')).toBe('text/css; charset=utf-8');
    expect(css.headers.get('cache-control')).toBe('no-cache');
  });

  it('marks content-hashed assets as immutable', async () => {
    const js = await serveWebDocument(request(`${WING_APP_ORIGIN}/assets/index-C9s2kQ1d.js`), root);
    expect(js.status).toBe(200);
    expect(js.headers.get('cache-control')).toBe('public, max-age=31536000, immutable');
    expect(js.headers.get('content-type')).toContain('text/javascript');
  });

  it('falls back to index.html for extension-less deep links (SPA routing)', async () => {
    const response = await serveWebDocument(request(`${WING_APP_ORIGIN}/session/42`), root);
    expect(response.status).toBe(200);
    expect(response.headers.get('content-type')).toBe('text/html; charset=utf-8');
    await expect(response.text()).resolves.toBe(INDEX_HTML);
  });

  it('404s a missing asset instead of answering with index.html', async () => {
    const response = await serveWebDocument(request(`${WING_APP_ORIGIN}/assets/old-chunk.js`), root);
    expect(response.status).toBe(404);
  });

  it('refuses requests that escape the renderer root', async () => {
    // Percent-encoded slashes survive URL normalization: this is the shape a
    // traversal actually reaches the handler in.
    const response = await serveWebDocument(request(`${WING_APP_ORIGIN}/..%2f..%2f..%2fetc/passwd`), root);
    expect(response.status).toBe(403);
  });

  it('rejects undecodable paths with 400', async () => {
    const response = await serveWebDocument(request(`${WING_APP_ORIGIN}/%zz`), root);
    expect(response.status).toBe(400);
  });

  it('only allows GET and HEAD', async () => {
    for (const method of ['POST', 'PUT', 'DELETE']) {
      const response = await serveWebDocument(request(WING_APP_INDEX_URL, method), root);
      expect(response.status).toBe(405);
    }
  });

  it('answers HEAD with headers but no body', async () => {
    const response = await serveWebDocument(request(WING_APP_INDEX_URL, 'HEAD'), root);
    expect(response.status).toBe(200);
    expect(response.headers.get('content-type')).toBe('text/html; charset=utf-8');
    await expect(response.text()).resolves.toBe('');
  });

  it('404s everything when the renderer build is missing', async () => {
    const empty = mkdtempSync(path.join(tmpdir(), 'wing-desktop-empty-'));
    temps.push(empty);
    expect((await serveWebDocument(request(WING_APP_INDEX_URL), empty)).status).toBe(404);
    expect((await serveWebDocument(request(`${WING_APP_ORIGIN}/session/1`), empty)).status).toBe(404);
  });
});
