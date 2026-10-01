/**
 * Which markdown image sources the gateway may be asked about.
 *
 * A pure function, and a *pre-filter* only: the gateway owns the decision (it knows
 * the session's workspace, resolves behind it, and answers 403/404/413). What this
 * module removes is the traffic that cannot possibly succeed and would only produce
 * noise in the log: remote URLs and data URIs (a link, not a file), sources longer
 * than the bridge's own limit, control characters, and extensions an `<img>` cannot
 * display.
 *
 * The rules are the same ones `extensions/vscode/src/host/images.ts` applies to the
 * same problem, minus the workspace containment — a browser has no workspace to
 * compare against (`path.relative` on the host is what makes that check possible),
 * and the gateway already refuses anything outside the session's working directory.
 */

import { MAX_IMAGE_SRC_CHARS } from '@wing-agent/ui/protocol';

/** Extensions an `<img>` can display — the gateway's whitelist, same list. */
const IMAGE_EXTENSIONS: ReadonlySet<string> = new Set([
  '.png',
  '.jpg',
  '.jpeg',
  '.gif',
  '.webp',
  '.svg',
  '.bmp',
  '.ico',
  '.avif',
  '.apng',
]);

/** `scheme:` — `https:`, `data:`, `blob:`, … never a workspace path. */
const SCHEME = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;

/**
 * A Windows drive letter looks exactly like a scheme (`C:\…`), and markdown-it
 * percent-encodes the backslashes of one written as `![a](C:\tmp\x.png)`, so both
 * spellings have to be recognized as paths (same rule as the VS Code host).
 */
const WINDOWS_DRIVE = /^[a-zA-Z]:(?:[\\/]|%5c)/i;

/**
 * The workspace-relative path to ask the gateway for, or `null` when this source is
 * not an image path at all.
 *
 * The returned value is the *decoded* source: markdown-it normalizes destinations,
 * so `![a](<my image.png>)` reaches the renderer as `my%20image.png`, and the decoded
 * form is what names the file.
 */
export function workspaceImagePath(src: string): string | null {
  const source = decodeSource(src.trim());
  if (source === '' || source.length > MAX_IMAGE_SRC_CHARS || hasControlCharacter(source)) {
    return null;
  }
  // A remote URL is a link, not an image (the transcript renders it as one).
  if (SCHEME.test(source) && !WINDOWS_DRIVE.test(source)) {
    return null;
  }
  if (!IMAGE_EXTENSIONS.has(extensionOf(source))) {
    return null;
  }
  return source;
}

/**
 * The lower-cased extension of a path, or `''` when it has none.
 *
 * `node:path.extname` semantics for the cases that matter here: the basename decides
 * (`.hidden` has no extension, `a.PNG` → `.png`), and both separators are accepted
 * because a Windows path is still a path.
 */
function extensionOf(path: string): string {
  const base = path.slice(Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\')) + 1);
  const dot = base.lastIndexOf('.');
  if (dot <= 0) {
    return '';
  }
  return base.slice(dot).toLowerCase();
}

/** Percent-decoding, once; a malformed escape is used verbatim (it resolves to nothing). */
function decodeSource(source: string): string {
  try {
    return decodeURIComponent(source);
  } catch {
    return source;
  }
}

/** Control characters never belong in a path (and would confuse a URL round trip). */
function hasControlCharacter(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 0x20 || code === 0x7f) {
      return true;
    }
  }
  return false;
}
