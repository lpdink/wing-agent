/**
 * The host ⇄ webview bridge protocol.
 *
 * ## Direction 1 — host → webview (the host is the only authority)
 *
 * | message | scope | meaning |
 * |---|---|---|
 * | `hydrate` | session | Full snapshot (state + cells). Sent after `ready`, after a `resync`, and whenever a tab is (re)opened. Resets the webview's `seq` cursor. |
 * | `patch` | session | Ordered cell ops with a `seq`. The only high-frequency message. |
 * | `state` | session | Full replacement of everything except cells (status, meta, turn, panels, …). Never bumps `seq`. |
 * | `panels` | session/global | Overlay data only (`PanelsModel`). |
 * | `tabs` | global | Tab bar contents + active tab. |
 * | `ui` | global | One-shot UI actions (toast, focus, …). Never part of the model. |
 * | `pong` | global | Reply to `ping` (channel diagnostics). |
 *
 * ## Direction 2 — webview → host
 *
 * `ready` / `resync` / `ping` are protocol-level; everything else is a user
 * intent. The webview never mutates its own model in response to an intent — it
 * waits for the host's `state` / `patch`.
 *
 * ## Application rules (webview side)
 *
 * Applying a `patch` is pure bookkeeping: `seq` must be exactly `lastSeq + 1` and
 * every addressed cell must exist. Anything else is a bug or a lost message — the
 * webview must answer `resync` (never guess) and the host replies with `hydrate`.
 */

import type { AskAnswerModel, CellModel } from './cells';
import type { PanelsModel, SessionStateModel, SessionViewModel, TabModel } from './session';
import type { CellId, RequestId, SessionId } from './types';
import { BRIDGE_PROTOCOL_VERSION } from './constants';

// ── host → webview ────────────────────────────────────────────────────

/**
 * One ordered mutation of a session's transcript.
 *
 * Ordering matters: `insert_after` addresses the cell that must end up directly
 * before the new one, which is how the host keeps concurrent tool calls in
 * arrival order without re-sending the whole transcript.
 */
export type CellPatch =
  /** Append a new cell at the end of the transcript. Fails if the id already exists. */
  | { readonly op: 'append'; readonly cell: CellModel }
  /** Insert directly after `afterCellId`. Fails if that cell does not exist. */
  | { readonly op: 'insert_after'; readonly afterCellId: CellId; readonly cell: CellModel }
  /** Replace the cell with the same id. Fails if it does not exist. */
  | { readonly op: 'update'; readonly cell: CellModel }
  /**
   * Append streamed text to a text-bearing cell (user / assistant / thinking /
   * system). Fails on other kinds — callers use `update` there.
   */
  | { readonly op: 'append_text'; readonly cellId: CellId; readonly text: string }
  /** Drop a cell (e.g. a cancelled tool call). Fails if it does not exist. */
  | { readonly op: 'remove'; readonly cellId: CellId }
  /** Replace the whole transcript (compaction, rewind, session reload). */
  | { readonly op: 'replace_all'; readonly cells: readonly CellModel[] };

/** Kinds of patches, derived from the union. */
export type CellPatchOp = CellPatch['op'];

/** One-shot UI actions the host may push. Ephemeral: never part of the model. */
export type UiActionModel =
  | { readonly kind: 'toast'; readonly level: 'info' | 'warning' | 'error'; readonly message: string }
  /** Put the caret in the composer (new / activated tab). */
  | { readonly kind: 'focusComposer' }
  /** Pin the transcript to the newest cell. */
  | { readonly kind: 'scrollToBottom' }
  /** Close every overlay (Escape semantics). */
  | { readonly kind: 'closeOverlays' };

/** Messages the host posts into the webview. */
export type HostToWebviewMessage =
  | { readonly type: 'hydrate'; readonly session: SessionViewModel }
  | {
      readonly type: 'patch';
      readonly sessionId: SessionId;
      /** `lastSeq + 1` — see the module doc. */
      readonly seq: number;
      readonly patches: readonly CellPatch[];
    }
  | { readonly type: 'state'; readonly state: SessionStateModel }
  | { readonly type: 'panels'; readonly sessionId: SessionId; readonly panels: PanelsModel }
  | { readonly type: 'tabs'; readonly tabs: readonly TabModel[]; readonly activeSessionId: SessionId | null }
  | { readonly type: 'ui'; readonly action: UiActionModel }
  | { readonly type: 'pong'; readonly id: string; readonly hostTimeMs: number };

/** Discriminants of {@link HostToWebviewMessage}. */
export type HostToWebviewMessageType = HostToWebviewMessage['type'];

// ── webview → host ────────────────────────────────────────────────────

/** Approve / deny a dangerous command confirmation. */
export type ToolApprovalDecision = 'approve' | 'deny';

/**
 * Why the webview could not apply a patch stream.
 *
 * `seq-gap` — a patch was lost (reload race, dropped message);
 * `unknown-cell` — an op addressed a cell this webview never saw;
 * `duplicate-cell` — `append` for an id that already exists;
 * `unsupported-op` — the op does not fit the addressed cell kind;
 * `protocol` — an unrecognized/invalid message.
 */
export type ResyncReason = 'seq-gap' | 'unknown-cell' | 'duplicate-cell' | 'unsupported-op' | 'protocol';

/** Messages the webview posts to the host. */
export type WebviewToHostMessage =
  /** First message after mount: the host must answer with `tabs` + `hydrate`. */
  | { readonly type: 'ready'; readonly protocolVersion: number }
  /** The webview's model is out of sync — the host answers with `hydrate`. */
  | {
      readonly type: 'resync';
      readonly sessionId: SessionId;
      readonly lastSeq: number;
      readonly reason: ResyncReason;
    }
  /** Channel diagnostics; the host answers with `pong`. */
  | { readonly type: 'ping'; readonly id: string }
  /** Send a user message (host assigns the request id + creates the pending cell). */
  | { readonly type: 'sendMessage'; readonly sessionId: SessionId; readonly text: string }
  /** Interrupt the running turn. */
  | { readonly type: 'interrupt'; readonly sessionId: SessionId }
  /** Answer an ask cell. */
  | {
      readonly type: 'answerAsk';
      readonly sessionId: SessionId;
      readonly requestId: RequestId;
      readonly answers: readonly AskAnswerModel[];
    }
  /** Approve / deny a Bash dangerous-command confirmation. */
  | {
      readonly type: 'approveTool';
      readonly sessionId: SessionId;
      readonly requestId: RequestId;
      readonly decision: ToolApprovalDecision;
    }
  /** New session in a new tab (workspace folder = first folder of the window). */
  | { readonly type: 'newSession' }
  /** Close a tab (unsubscribe + forget the session view). */
  | { readonly type: 'closeSession'; readonly sessionId: SessionId }
  /** Focus a tab. */
  | { readonly type: 'activateSession'; readonly sessionId: SessionId }
  /** `/compact`. */
  | { readonly type: 'compact'; readonly sessionId: SessionId }
  /** Apply a model selection (also closes the picker). */
  | {
      readonly type: 'setModel';
      readonly sessionId: SessionId;
      readonly provider: string;
      readonly model: string;
    }
  /** Toggle thinking. */
  | { readonly type: 'setThinking'; readonly sessionId: SessionId; readonly enabled: boolean }
  /** Set reasoning effort (provider-specific label, e.g. `high`). */
  | { readonly type: 'setEffort'; readonly sessionId: SessionId; readonly effort: string }
  /** Toggle YOLO mode. */
  | { readonly type: 'setYolo'; readonly sessionId: SessionId; readonly enabled: boolean }
  /** Run a prompt command (`/init`, `/ss`, …) — the host resolves it against the gateway. */
  | {
      readonly type: 'runPromptCommand';
      readonly sessionId: SessionId;
      readonly name: string;
      readonly argsText: string;
    }
  /** Open the `/model` picker for a session. */
  | { readonly type: 'openModelPicker'; readonly sessionId: SessionId }
  /** Close every overlay. */
  | { readonly type: 'closeOverlays' }
  /** Open an http(s) link in the OS browser. */
  | { readonly type: 'openLink'; readonly href: string }
  /** Open a file (optionally at a line) in an editor tab. */
  | { readonly type: 'openFile'; readonly path: string; readonly line: number | null }
  /** Open the native diff editor for a diff cell. */
  | { readonly type: 'openDiff'; readonly sessionId: SessionId; readonly cellId: CellId }
  /** Copy text to the clipboard (webviews cannot do it reliably themselves). */
  | { readonly type: 'copyText'; readonly text: string };

/** Discriminants of {@link WebviewToHostMessage}. */
export type WebviewToHostMessageType = WebviewToHostMessage['type'];

// ── transport ─────────────────────────────────────────────────────────

/**
 * The webview's view of the bridge.
 *
 * Production uses the VS Code `postMessage` channel
 * (`acquireVsCodeApi()`); tests and the preview harness inject a mock. Keeping
 * this an interface is what makes the renderer testable without a VS Code host.
 */
export interface WebviewTransport {
  /** Post one message to the host. Must never throw. */
  post(message: WebviewToHostMessage): void;
  /** Subscribe to host messages; returns an unsubscribe function. */
  subscribe(handler: (message: HostToWebviewMessage) => void): () => void;
}

// ── bootstrap ─────────────────────────────────────────────────────────

/**
 * Values the host injects into the webview document before the bundle runs
 * (`window.__WING_BOOTSTRAP__`).
 *
 * Static data only: it must be available at first paint, so it cannot travel over
 * the async message channel.
 */
export interface BootstrapModel {
  readonly protocolVersion: number;
  /** Logical asset name → webview-safe URI (icons, images, …). */
  readonly assetUris: Readonly<Record<string, string>>;
}

/** The bootstrap value a webview falls back to when the host injected nothing. */
export const FALLBACK_BOOTSTRAP: BootstrapModel = {
  protocolVersion: BRIDGE_PROTOCOL_VERSION,
  assetUris: {},
};
