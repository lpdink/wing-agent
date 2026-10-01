/**
 * The host ⇄ webview bridge protocol.
 *
 * This is the *channel*: the message unions below, plus their transport rules. The
 * values they carry (transcript cells, session state, panels) are the session
 * package's model — `@wing-agent/session` — and are imported, never redeclared.
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
 * | `images` | global | Reply to `resolveImages`: markdown image sources resolved to webview-loadable URIs (or `null`). |
 *
 * ## Direction 2 — webview → host
 *
 * `ready` / `resync` / `ping` / `resolveImages` are protocol-level; everything
 * else is a user intent. The webview never mutates its own model in response to an
 * intent — it waits for the host's `state` / `patch`.
 *
 * ## Application rules (webview side)
 *
 * Applying a `patch` is pure bookkeeping: `seq` must be exactly `lastSeq + 1` and
 * every addressed cell must exist. Anything else is a bug or a lost message — the
 * webview must answer `resync` (never guess) and the host replies with `hydrate`.
 * `./patches` is the executable form of these rules (`applyCellPatches`), and the
 * host's own test mirror reuses it so a violation surfaces as a `resync`-worthy
 * failure instead of a silently different transcript.
 */

import type {
  AskAnswerModel,
  CellId,
  CellPatch,
  PanelsModel,
  RequestId,
  SessionId,
  SessionStateModel,
  SessionViewModel,
  TabModel,
} from '@wing-agent/session';

// ── the channel version ───────────────────────────────────────────────

/**
 * Bridge protocol version.
 *
 * Bump on **breaking** changes to {@link HostToWebviewMessage} /
 * {@link WebviewToHostMessage}. Additive changes (new message variants, new
 * nullable fields on the shared model) do not need a bump: both sides of one shell
 * ship together, so they are only ever one version apart at most — the number exists
 * so the host can refuse a stale cached document instead of failing silently.
 *
 * It lives here, next to the union it versions. It used to sit in the extension's
 * channel constants back when the protocol did; every consumer of the protocol
 * (the webview entry, the store, the host's document generator) reads it from this
 * module now.
 */
export const BRIDGE_PROTOCOL_VERSION = 1;

// ── host → webview ────────────────────────────────────────────────────

/** One-shot UI actions the host may push. Ephemeral: never part of the model. */
export type UiActionModel =
  | { readonly kind: 'toast'; readonly level: 'info' | 'warning' | 'error'; readonly message: string }
  /** Put the caret in the composer (new / activated tab). */
  | { readonly kind: 'focusComposer' }
  /** Pin the transcript to the newest cell. */
  | { readonly kind: 'scrollToBottom' }
  /** Close every overlay (Escape semantics). */
  | { readonly kind: 'closeOverlays' };

/**
 * One answer to a `resolveImages` request.
 *
 * `uri` is `null` when the host refuses the source (remote URL, path outside the
 * workspace, not an image, no workspace folder, …) *and* when it could not be
 * turned into a resource the webview is allowed to load. Both cases mean the same
 * thing to the renderer: keep the existing link rendering.
 */
export interface ResolvedImageModel {
  /** The markdown `src` exactly as asked, so the reply is self-describing. */
  readonly src: string;
  /** `webview.asWebviewUri` result, or `null` for "not renderable". */
  readonly uri: string | null;
}

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
  | { readonly type: 'pong'; readonly id: string; readonly hostTimeMs: number }
  /** One per `resolveImages` request, in request order. */
  | { readonly type: 'images'; readonly images: readonly ResolvedImageModel[] };

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
  /**
   * Ask the host to resolve local image paths into webview-loadable URIs.
   *
   * Protocol-level (the *view* answers it, like `pong` — no session involved): the
   * mapping only depends on the workspace root and this document's resource
   * scope, both of which are host-side knowledge. Batched on purpose — one paint
   * of a transcript produces one message, not one per image. Unknown/refused
   * sources come back as `uri: null` and keep the existing link rendering.
   */
  | { readonly type: 'resolveImages'; readonly srcs: readonly string[] }
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

/**
 * Caps for one `resolveImages` request.
 *
 * Every other message on this channel is only *dispatched*; this one is *walked* by
 * the host (`srcs.map(…)`), so the protocol — not just the view — has to publish a
 * size. `MAX_IMAGE_SRC_CHARS` is also the markdown source limit the path policy
 * refuses beyond (`host/images.ts`), so a source that fits the wire always reaches
 * the same verdict as one the view skipped.
 */
export const RESOLVE_IMAGES_MAX_SRCS = 64;
export const MAX_IMAGE_SRC_CHARS = 1024;

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
