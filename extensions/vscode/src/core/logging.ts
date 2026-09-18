/**
 * Logging seam for `src/core`.
 *
 * The layer has no `vscode` dependency, so it cannot use the extension's output
 * channel directly; instead every client takes an optional `CoreLogger` and 03
 * passes the extension logger through. The default logs to the console, which in
 * the extension host lands in the "Extension Host" output channel — a silent
 * default would hide the diagnostics that matter most (why did we disconnect?).
 */

export interface CoreLogger {
  debug(message: string, detail?: unknown): void;
  warn(message: string, detail?: unknown): void;
  error(message: string, detail?: unknown): void;
}

/** Logs through `console.debug` / `warn` / `error` (the levels ESLint allows). */
export const consoleLogger: CoreLogger = {
  debug: (message, detail) => {
    console.debug(`[wing/core] ${message}`, ...(detail === undefined ? [] : [detail]));
  },
  warn: (message, detail) => {
    console.warn(`[wing/core] ${message}`, ...(detail === undefined ? [] : [detail]));
  },
  error: (message, detail) => {
    console.error(`[wing/core] ${message}`, ...(detail === undefined ? [] : [detail]));
  },
};

/** Drops everything (tests that assert behaviour, not noise). */
export const silentLogger: CoreLogger = {
  debug: () => undefined,
  warn: () => undefined,
  error: () => undefined,
};
