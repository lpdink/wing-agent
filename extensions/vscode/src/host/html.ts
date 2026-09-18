import { randomUUID } from 'node:crypto';

import type { BootstrapModel } from '../shared';

/**
 * Webview document generation.
 *
 * Pure functions (no `vscode` import) so the CSP/nonce/bootstrap contract is unit
 * testable — a white-screen webview is the classic silent failure of this layer.
 *
 * The CSP shape follows VS Code's webview guidance: `default-src 'none'` plus
 * exactly the sources we need. VS Code augments the `script-src` / `style-src`
 * directives with its own nonces when it serves the document, so the values below
 * only have to cover *our* script, stylesheet and images.
 *
 * The webview ships one IIFE bundle and one stylesheet (see `vite.config.mts`); no
 * third-party origin, no runtime `fetch` — hence `connect-src` stays unset
 * (`default-src 'none'` blocks it).
 */

export interface WebviewHtmlOptions {
  /** `webview.cspSource` — the origin of `asWebviewUri` resources. */
  readonly cspSource: string;
  /** Single-use script/style nonce. */
  readonly nonce: string;
  /** `asWebviewUri` of the built webview bundle. */
  readonly scriptUri: string;
  /** `asWebviewUri` of the built stylesheet; `null` when the build emitted none. */
  readonly styleUri: string | null;
  /** Values injected into `window.__WING_BOOTSTRAP__` before the bundle runs. */
  readonly bootstrap: BootstrapModel;
  /** Document title (visible in the webview's a11y tree). */
  readonly title: string;
  /** Element id the bundle mounts into. */
  readonly rootId: string;
}

/** Fresh, unguessable nonce for one document load. */
export function createNonce(): string {
  return randomUUID().replaceAll('-', '');
}

/**
 * JSON for an inline `<script>` body.
 *
 * Escapes the characters that could terminate the script element or open an HTML
 * comment — `JSON.stringify` alone does not, which is the standard way an inline
 * bootstrap becomes an injection point.
 */
export function serializeInlineJson(value: unknown): string {
  return JSON.stringify(value)
    .replaceAll('<', '\\u003c')
    .replaceAll('>', '\\u003e')
    .replaceAll('&', '\\u0026')
    .replaceAll('\u2028', '\\u2028')
    .replaceAll('\u2029', '\\u2029');
}

/** Content-Security-Policy for the webview document. */
export function buildContentSecurityPolicy(options: Pick<WebviewHtmlOptions, 'cspSource' | 'nonce'>): string {
  const { cspSource, nonce } = options;
  return [
    "default-src 'none'",
    `img-src ${cspSource} data:`,
    `font-src ${cspSource}`,
    // 'unsafe-inline' covers style *attributes* (React sets a few) and VS Code's
    // own theme variable injection; scripts stay nonce-only.
    `style-src ${cspSource} 'unsafe-inline'`,
    `script-src 'nonce-${nonce}'`,
  ].join('; ');
}

/** The full webview document. */
export function buildWebviewHtml(options: WebviewHtmlOptions): string {
  const { cspSource, nonce, scriptUri, styleUri, bootstrap, title, rootId } = options;
  const csp = buildContentSecurityPolicy({ cspSource, nonce });
  const styleTag = styleUri === null ? '' : `\n    <link rel="stylesheet" href="${styleUri}">`;

  return `<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <meta http-equiv="Content-Security-Policy" content="${csp}">${styleTag}
    <title>${title}</title>
  </head>
  <body>
    <div id="${rootId}"></div>
    <script nonce="${nonce}">
      window.__WING_BOOTSTRAP__ = ${serializeInlineJson(bootstrap)};
    </script>
    <script nonce="${nonce}" src="${scriptUri}"></script>
  </body>
</html>
`;
}
