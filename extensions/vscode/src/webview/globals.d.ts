/**
 * Globals injected by VS Code into every webview document.
 *
 * Declared per-project (webview tsconfig only) so host code cannot accidentally
 * rely on them, and the webview bundle cannot accidentally rely on node globals.
 */

/** Handle returned by {@link acquireVsCodeApi}. */
interface VsCodeApi {
  postMessage(message: unknown): void;
  getState(): unknown;
  setState(state: unknown): void;
}

/** Present only inside a VS Code webview. */
declare function acquireVsCodeApi(): VsCodeApi;

interface Window {
  /** Injected by the host document (see `src/host/html.ts`); absent in the preview harness. */
  __WING_BOOTSTRAP__?: unknown;
}
