/**
 * File references in transcript content.
 *
 * The renderer never guesses: a string is a file reference only when it looks
 * like one (a slash-separated path, or a bare name with a known code extension,
 * optionally suffixed with `:line`). Clicking one asks the *host* to open it —
 * webviews cannot touch the editor themselves.
 *
 * The host derives `display.subject` for tool calls, so the subject of a `Read`
 * call is typically already exactly this shape (`src/app/main.ts:42`).
 */

import type { ReactElement } from 'react';

import { postToHost } from '../bridge/channel';
import styles from '../styles/chat.module.css';

/** Extensions that make a bare filename (no slash) a plausible file reference. */
const FILE_EXTENSIONS = [
  'ts',
  'tsx',
  'js',
  'jsx',
  'mjs',
  'cjs',
  'json',
  'jsonc',
  'md',
  'markdown',
  'css',
  'scss',
  'less',
  'html',
  'vue',
  'svelte',
  'py',
  'pyi',
  'rs',
  'go',
  'rb',
  'java',
  'kt',
  'swift',
  'c',
  'h',
  'cc',
  'cpp',
  'hpp',
  'cs',
  'php',
  'sh',
  'bash',
  'zsh',
  'ps1',
  'toml',
  'yaml',
  'yml',
  'ini',
  'cfg',
  'conf',
  'sql',
  'lock',
  'txt',
  'csv',
  'env',
  'dockerfile',
  'makefile',
];

const REFERENCE = /^([^\s:]+)(?::(\d+))?$/;
const HAS_EXTENSION = new RegExp(`\\.(?:${FILE_EXTENSIONS.join('|')})$`, 'i');

export interface FileReferenceModel {
  readonly path: string;
  readonly line: number | null;
}

/** Parse `path` / `path:line` into a reference, or `null` when it is not one. */
export function parseFileReference(raw: string): FileReferenceModel | null {
  const value = raw.trim();
  const match = REFERENCE.exec(value);
  if (match === null) {
    return null;
  }
  const path = match[1] ?? '';
  if (path === '' || path.length > 512) {
    return null;
  }
  const looksLikePath = path.startsWith('/') || path.includes('/') || HAS_EXTENSION.test(path);
  if (!looksLikePath) {
    return null;
  }
  const line = match[2] === undefined ? null : Number.parseInt(match[2], 10);
  return { path, line: Number.isNaN(line ?? 0) ? null : line };
}

/** A clickable file path that asks the host to open the file in an editor tab. */
export function FileReference({ path, line }: FileReferenceModel): ReactElement {
  return (
    <button
      type="button"
      className={styles.fileReference}
      data-file-path={path}
      title={line === null ? path : `${path}:${line}`}
      onClick={() => {
        postToHost({ type: 'openFile', path, line });
      }}
    >
      {path}
      {line === null ? null : <span className={styles.fileReferenceLine}>{`:${line}`}</span>}
    </button>
  );
}

/** Render `subject` as a file link when it parses as one, else as plain text. */
export function FileReferenceText({ text }: { readonly text: string }): ReactElement {
  const reference = parseFileReference(text);
  return reference === null ? (
    <span>{text}</span>
  ) : (
    <FileReference path={reference.path} line={reference.line} />
  );
}
