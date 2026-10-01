/**
 * Globals injected by VS Code into every webview document.
 *
 * Declared per-project (webview tsconfig only) so host code cannot accidentally
 * rely on them, and the webview bundle cannot accidentally rely on node globals.
 *
 * Only the VS Code-specific part lives here: the bridge's own window property
 * (`__WING_BOOTSTRAP__`, read by `@wing-agent/ui`'s `readBootstrap`) is declared by
 * the package — the renderer owns the value it reads.
 */

/** Handle returned by {@link acquireVsCodeApi}. */
interface VsCodeApi {
  postMessage(message: unknown): void;
  getState(): unknown;
  setState(state: unknown): void;
}

/** Present only inside a VS Code webview. */
declare function acquireVsCodeApi(): VsCodeApi;
