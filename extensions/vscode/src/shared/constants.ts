/**
 * Vocabulary of **this extension's** webview channel.
 *
 * What is left here are the three magic values that describe the VS Code side of the
 * channel and nothing else — the theme class names the generated document applies,
 * the element id the bundle mounts into, and the patch-chunk cap the host slices
 * streamed text with. Keep it free of runtime dependencies (no `vscode`, no DOM, no
 * node) — the layer guard enforces it.
 *
 * The protocol itself — the message unions, their guards, the transport interface,
 * the protocol version and the image caps — is `@wing-agent/ui/protocol`
 * (`packages/ui/src/protocol/`), because the renderer and the host both speak it and
 * the renderer now lives in that package. The session-model and command vocabulary
 * (`TOOL_NAMES`, `LOCAL_COMMANDS`, `SESSION_TITLE_MAX_LENGTH`) is
 * `@wing-agent/session`.
 */

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
