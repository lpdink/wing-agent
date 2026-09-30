import path from 'node:path';

import { MAX_IMAGE_SRC_CHARS } from '../shared';

/**
 * Image path policy for the transcript.
 *
 * The model writes an image source the way it would write a file path (relative to
 * the session's workspace, or absolute) — but a webview can only load resources
 * the *host* has turned into a webview URI, and only from the roots it was given.
 * This module owns the decision, as a pure function: same inputs → same answer,
 * no editor API, no filesystem, no I/O at all.
 *
 * Not touching the disk is deliberate. `asWebviewUri` returns a URI whether or not
 * the file exists, and a missing (or mislabelled) file fails at `<img>` load time,
 * where the renderer already falls back to the link it used to show. Statting here
 * would buy the same visible outcome for a per-image file-system round trip — and
 * it would have to be asynchronous, which the render path is not.
 */

/** Extensions a webview `<img>` can actually display. Everything else stays a link. */
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

/** `scheme:` — `https:`, `data:`, `vscode-*:`, … never a local file. */
const SCHEME = /^[a-zA-Z][a-zA-Z0-9+.-]*:/;

/**
 * A Windows drive letter looks exactly like a scheme (`C:\…`), and markdown-it
 * percent-encodes the backslashes of one written as `![a](C:\tmp\x.png)`, so both
 * spellings have to be recognized as paths.
 */
const WINDOWS_DRIVE = /^[a-zA-Z]:(?:[\\/]|%5c)/i;

/**
 * The absolute path of a markdown image source, or `null` when it must stay a link.
 *
 * - `root` is the workspace folder (`null` when no folder is open — then nothing is
 *   inside the workspace, so every relative source is refused);
 * - remote URLs, data URLs, non-image extensions, paths escaping the workspace and
 *   malformed sources are all refused;
 * - refusals are silent by design: the renderer keeps the existing link, which is
 *   exactly what the user saw before images were supported.
 */
export function resolveWorkspaceImage(root: string | null, src: string): string | null {
  const source = decodeSource(src.trim());
  // The same length the protocol allows (`shared/bridge.ts`): a source that is too
  // long to travel is too long to be a path a model wrote on purpose.
  if (source === '' || source.length > MAX_IMAGE_SRC_CHARS || hasControlCharacter(source)) {
    return null;
  }
  // A remote URL is a link, not an image — the CSP forbids the former anyway.
  if (SCHEME.test(source) && !WINDOWS_DRIVE.test(source)) {
    return null;
  }
  // `#fragment` / `?query` suffixes land here too: a markdown image source with one
  // is a URL, not a file name.
  if (!IMAGE_EXTENSIONS.has(path.extname(source).toLowerCase())) {
    return null;
  }
  if (root === null) {
    return null;
  }

  const absolute = path.isAbsolute(source) ? path.normalize(source) : resolveWorkspacePath(root, source);
  const relative = path.relative(path.resolve(root), absolute);
  // `''` is the folder itself, `..`-prefixed or absolute is outside it (on Windows a
  // different drive also comes back absolute).
  if (relative === '' || relative.startsWith('..') || path.isAbsolute(relative)) {
    return null;
  }
  return absolute;
}

/**
 * Resolve a path the model wrote against the workspace, the way
 * `VsCodeEditorActions.openFile` has always done it (one shared rule, not two).
 *
 * Not exported as a general-purpose "safe join": `openFile` wants the workspace
 * *interpretation* of a path and will happily open something outside it (the user
 * asked for that file), while an image must additionally stay inside
 * ({@link resolveWorkspaceImage}).
 */
export function resolveWorkspacePath(root: string | null, candidate: string): string {
  if (path.isAbsolute(candidate)) {
    return candidate;
  }
  return root === null ? candidate : path.join(root, candidate);
}

/**
 * Percent-decoding, once.
 *
 * markdown-it normalizes destinations, so a source written as `![a](<my image.png>)`
 * reaches the webview as `my%20image.png` (and a Windows path as `C:%5Ctmp%5Cx.png`)
 * — the decoded form is what names the file. A malformed escape (`50%.png`) cannot
 * be decoded and is used verbatim: it simply will not resolve to anything.
 */
function decodeSource(source: string): string {
  try {
    return decodeURIComponent(source);
  } catch {
    return source;
  }
}

/** Control characters never belong in a path (and would confuse a URI round trip). */
function hasControlCharacter(value: string): boolean {
  for (let index = 0; index < value.length; index += 1) {
    const code = value.charCodeAt(index);
    if (code < 0x20 || code === 0x7f) {
      return true;
    }
  }
  return false;
}
