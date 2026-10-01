/**
 * Bridge vocabulary for the host ⇄ webview contract.
 *
 * This module holds magic strings that both sides of **this extension's** bridge must
 * agree on. Keep it free of runtime dependencies (no `vscode`, no DOM, no node) — the
 * layer guard enforces it.
 *
 * The session-model and command vocabulary lives in `@wing-agent/session`
 * (`TOOL_NAMES`, `LOCAL_COMMANDS`, `SESSION_TITLE_MAX_LENGTH`): those values describe
 * the gateway's sessions, not this channel, and every frontend needs them.
 */

/**
 * Bridge protocol version.
 *
 * Bump on **breaking** changes to {@link HostToWebviewMessage} /
 * {@link WebviewToHostMessage}. Additive changes (new message variants, new
 * nullable fields on the shared model) do not need a bump: both sides ship in the
 * same VSIX, so they are only ever one version apart at most — the number exists
 * so the host can refuse a stale cached webview instead of failing silently.
 */
export const BRIDGE_PROTOCOL_VERSION = 1;

/** CSS class applied to the webview root for the current theme kind. */
export const THEME_CLASSES = {
  light: 'vscode-light',
  dark: 'vscode-dark',
  highContrast: 'vscode-high-contrast',
  highContrastLight: 'vscode-high-contrast-light',
} as const;

/**
 * Element id the webview bundle mounts into.
 *
 * The host generates the document (`src/host/html.ts`) and the webview entry
 * looks the element up (`src/webview/main.tsx`) — a shared constant keeps the two
 * from drifting apart.
 */
export const WEBVIEW_ROOT_ID = 'root';

/**
 * Upper bound for a single patch payload's text chunk. The host slices larger
 * streamed text into several `append_text` ops so one postMessage never builds a
 * multi-megabyte string (VS Code serializes messages through the extension host).
 */
export const MAX_PATCH_TEXT_CHUNK = 64 * 1024;
