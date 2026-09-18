/**
 * Session view model — what a single tab renders.
 *
 * The host owns this structure end to end (reduction from gateway events, title
 * derivation, status derivation). The webview receives it through `hydrate`,
 * then keeps it up to date through `patch` (cells) and `state` (everything else).
 *
 * `SessionStateModel` and `SessionViewModel` are deliberately split: cell content
 * is the only thing that changes at streaming rate, so only `patch` messages
 * carry cells — status/meta updates stay small and idempotent.
 */

import type { CellModel } from './cells';
import type { EpochMs, SessionId } from './types';

/**
 * What the session is doing right now.
 *
 * `waiting-for-input` means the agent asked a question (or needs a dangerous
 * command confirmation) and the turn is parked until the user answers.
 */
export type SessionStatus = 'idle' | 'working' | 'waiting-for-input';

/**
 * Transient badge shown on a tab that the user is not looking at.
 *
 * Cleared by the host when the tab becomes active. `result` means a turn finished
 * while the tab was in the background; `error` means it failed.
 */
export type SessionAttention = 'none' | 'result' | 'error';

/** Session-level knobs and identity, as reported by the gateway. */
export interface SessionMetaModel {
  /** Model name (`''` until the gateway reports it). */
  readonly model: string;
  /** Provider name (`''` until the gateway reports it, or on old gateways). */
  readonly provider: string;
  /** Thinking / reasoning enabled. */
  readonly thinking: boolean;
  /** Reasoning effort (`''` when the provider has no effort knob). */
  readonly reasoningEffort: string;
  /** YOLO mode: skip tool approvals. */
  readonly yolo: boolean;
  /** Agent template name (`''` when none). */
  readonly agent: string;
  /** Session working directory (`''` until known). */
  readonly workspace: string;
  /** Gateway session creation timestamp (ISO-8601 string); `''` when unknown. */
  readonly createdAt: string;
}

/** Context window accounting (`context_stats`). */
export interface ContextUsageModel {
  /** Tokens currently in the context window. */
  readonly usedTokens: number;
  /** Window size; `0` when the gateway does not report one. */
  readonly windowTokens: number;
  readonly messageCount: number;
}

/** Cumulative session token totals (all turns). */
export interface SessionTotalsModel {
  readonly promptTokens: number;
  readonly completionTokens: number;
  readonly cachedTokens: number;
}

/** Summary of a finished turn (`TurnResult` event, as the TUI shows it). */
export interface TurnResultModel {
  /** `success` / `error` / … (raw subtype from the gateway). */
  readonly subtype: string;
  readonly isError: boolean;
  readonly durationMs: number;
  readonly numTurns: number;
  /** Total tokens reported by the gateway; `null` when absent. */
  readonly totalTokens: number | null;
  /** Final assistant text, host-truncated for display; `null` when absent. */
  readonly resultText: string | null;
}

/** Live turn bookkeeping (drives the spinner / elapsed time in the UI). */
export interface TurnViewModel {
  readonly active: boolean;
  /** Epoch ms the current turn started; `0` when idle. */
  readonly startedAtMs: EpochMs;
  /** Result of the last finished turn; `null` before the first one. */
  readonly lastResult: TurnResultModel | null;
}

// ── overlays (panels) ─────────────────────────────────────────────────

/** One row of the `/model` picker. */
export interface ModelPickerRowModel {
  readonly provider: string;
  readonly model: string;
  /** True for the row matching the session's current provider + model. */
  readonly selected: boolean;
}

/**
 * `/model` picker data.
 *
 * Present in `PanelsModel` means "the picker is open" — the webview renders it
 * and reports the choice back with `setModel`. Keyboard-first: the host may
 * pre-select a row through `activeIndex`.
 */
export interface ModelPickerModel {
  readonly sessionId: SessionId;
  readonly rows: readonly ModelPickerRowModel[];
  /** Row index to highlight initially; `null` = the selected row. */
  readonly activeIndex: number | null;
}

/** App-level banner (gateway unreachable, reconnecting, …). */
export interface GlobalNoticeModel {
  readonly level: 'info' | 'warning' | 'error';
  readonly text: string;
}

/**
 * One prompt command (`GET /api/commands`).
 *
 * The catalog feeds the composer's `/`-triggered candidates; it is not a
 * transcript cell. `name` includes the leading `/` (e.g. `/init`).
 */
export interface PromptCommandModel {
  readonly name: string;
  readonly aliases: readonly string[];
  readonly description: string;
  /** Parameter hint (`''` when the command takes none). */
  readonly params: string;
}

/** Gateway-side runtime status of a session in the history list. */
export type SessionHistoryStatus = 'idle' | 'working' | 'waiting-for-input' | 'inactive';

/** One row of the session-history picker (`GET /api/session/list`). */
export interface SessionHistoryRowModel {
  readonly sessionId: SessionId;
  /** Explicit name or first user message (`first_user_message` rule). */
  readonly title: string;
  readonly workspace: string | null;
  /** ISO-8601 string; `null` when the gateway does not report one. */
  readonly lastInteraction: string | null;
  /** `inactive` = not loaded into the gateway process (resume to use it). */
  readonly status: SessionHistoryStatus;
}

/** Session-history picker (`/ss`). Present in `PanelsModel` means "open". */
export interface SessionPanelModel {
  readonly rows: readonly SessionHistoryRowModel[];
  /** Row index to highlight initially; `null` = the row matching this session. */
  readonly activeIndex: number | null;
}

/** One rewind / fork candidate (`GET /api/session/branches`). */
export interface BranchTargetRowModel {
  readonly uuid: string;
  /** `user` for a message, `compact` for a compaction marker. */
  readonly role: string;
  /** Host-truncated preview of the node's content. */
  readonly preview: string;
  /** True for the backend's `current` sentinel (the newest state; a no-op target). */
  readonly current: boolean;
}

/** Rewind / fork picker. Present in `PanelsModel` means "open". */
export interface BranchPanelModel {
  readonly mode: 'rewind' | 'fork';
  readonly rows: readonly BranchTargetRowModel[];
  readonly activeIndex: number | null;
}

/** Overlay data for the active session. `null` members mean "not shown". */
export interface PanelsModel {
  readonly modelPicker: ModelPickerModel | null;
  readonly globalNotice: GlobalNoticeModel | null;
  /** Prompt-command catalog; `null` until the host has fetched it. */
  readonly commands: readonly PromptCommandModel[] | null;
  /** Session-history picker; `null` = closed. */
  readonly sessions: SessionPanelModel | null;
  /** Rewind / fork picker; `null` = closed. */
  readonly branches: BranchPanelModel | null;
}

/** The `panels` value with nothing open. */
export const EMPTY_PANELS: PanelsModel = {
  modelPicker: null,
  globalNotice: null,
  commands: null,
  sessions: null,
  branches: null,
};

// ── session state / view ──────────────────────────────────────────────

/**
 * Everything about a session **except** its transcript.
 *
 * Sent whole (idempotent full replacement) by the `state` message and included in
 * `hydrate`.
 */
export interface SessionStateModel {
  readonly sessionId: SessionId;
  /** Host-derived title: explicit session name first, else the first user message. */
  readonly title: string;
  readonly status: SessionStatus;
  readonly attention: SessionAttention;
  readonly meta: SessionMetaModel;
  readonly context: ContextUsageModel;
  readonly totals: SessionTotalsModel;
  readonly turn: TurnViewModel;
  /** Last error surfaced for this session (host-truncated); `null` when none. */
  readonly lastError: string | null;
  /**
   * Composer text the backend handed back (resume / rewind / fork / sync draft).
   *
   * `null` when there is nothing to restore. The webview adopts it only when it
   * is non-null **and** differs from the last value it adopted — a `state`
   * message carrying `null` must never clear what the user is typing.
   */
  readonly draft: string | null;
  readonly panels: PanelsModel;
  /**
   * Sequence number of the newest content in this snapshot.
   *
   * `state` updates leave it untouched; `patch` messages increment it. See
   * `CellPatch` for how the webview detects a gap.
   */
  readonly seq: number;
}

/** A session snapshot including its transcript — the `hydrate` payload. */
export interface SessionViewModel extends SessionStateModel {
  readonly cells: readonly CellModel[];
}

/** One entry of the webview's tab bar. */
export interface TabModel {
  readonly sessionId: SessionId;
  readonly title: string;
  readonly status: SessionStatus;
  readonly attention: SessionAttention;
}
