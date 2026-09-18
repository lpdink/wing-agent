/**
 * `vscode` module stub — the smoke's platform seam.
 *
 * The host layer imports `vscode` in a few places (`settings.ts` reads
 * `workspace.getConfiguration`, `editorActions.ts` opens editors, `log.ts` owns
 * the output channel). The smoke provides its own settings object and its own
 * `EditorActions` implementation, so none of those functions is ever called —
 * this module exists so the bundle resolves and a future accidental call fails
 * loudly instead of silently doing nothing.
 *
 * It is wired in `esbuild.smoke.mjs` as an alias, which is also why it must stay
 * dependency-free.
 */

function notAvailable(): never {
  throw new Error('the vscode API is not available in the smoke (tools/smoke/vscode-stub.ts)');
}

/** Every named import the host layer uses, all poisoned. */
export const workspace = {
  getConfiguration: notAvailable,
};

export const window = {
  createOutputChannel: notAvailable,
  showErrorMessage: notAvailable,
};

export const commands = {
  registerCommand: notAvailable,
  executeCommand: notAvailable,
};

export const Uri = {
  file: notAvailable,
};

export class Disposable {
  constructor(_callback: () => void) {
    notAvailable();
  }

  dispose(): void {
    notAvailable();
  }
}

export const env = {
  openExternal: notAvailable,
};

export const languages = {};

export const ViewColumn = {};

export const ExtensionMode = {};
