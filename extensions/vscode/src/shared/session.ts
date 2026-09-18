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

// ── panel catalogs (data the host fetches, the webview renders) ───────

/**
 * One slash command the host can route.
 *
 * Mirrors the gateway's `CommandInfo` (`libs/core/wing/event/base.py:90`), whose
 * `name` carries **no** leading slash. The webview normalizes both spellings with
 * `normalizeCommandName` (see `src/shared/commands.ts`) before comparing.
 */
export interface CommandInfoModel {
  readonly name: string;
  readonly aliases: readonly string[];
  readonly description: string;
  /** Parameter hint shown next to the command (`'[focus]'`, `'on|off'`, …). */
  readonly params: string;
}

/**
 * Slash-command catalog (`GET /api/commands` plus the host's own commands).
 *
 * `null` on {@link PanelsModel} means "not fetched yet" — not "no commands": the
 * composer falls back to its own `FRONTEND_COMMANDS` table in that case.
 */
export interface CommandCatalogModel {
  readonly commands: readonly CommandInfoModel[];
}

/**
 * Session list status.
 *
 * The gateway's vocabulary (`inactive|idle|working|waiting`), mapped by the host
 * onto the UI's three states plus `inactive` for sessions that are not loaded:
 * `waiting` is the user's `waiting-for-input`, and `inactive` means "on disk, not
 * in memory" (so resuming it costs a load).
 */
export type SessionListStatus = 'inactive' | 'idle' | 'working' | 'waiting-for-input';

/** One row of the `/ss` (session) picker. Mirrors `SessionInfo` (`event/base.py:57`). */
export interface SessionCandidateModel {
  readonly sessionId: string;
  /** Host-derived title (never empty — falls back to the session id). */
  readonly title: string;
  /** Session working directory; `''` when unknown. */
  readonly workspace: string;
  readonly status: SessionListStatus;
  /** True for the session this catalog was fetched for (rendered as "current"). */
  readonly current: boolean;
}

/**
 * Session catalog (`GET /api/session/list`).
 *
 * `null` means "not fetched yet"; the session panel then falls back to the open
 * tabs, which the host streams on every change anyway.
 */
export interface SessionCatalogModel {
  readonly sessions: readonly SessionCandidateModel[];
}

/**
 * One rewind / fork target.
 *
 * Mirrors `BranchTargetInfo` (`libs/core/wing/event/query_response.py:27`). The
 * gateway appends a final `{ uuid: 'current', content: '(current)' }` entry that
 * represents the newest state (`context_manager.py:925`) — the webview marks it as
 * the current point instead of a target.
 */
export interface BranchTargetModel {
  readonly uuid: string;
  /** Display label (the gateway truncates user messages to 100 chars). */
  readonly content: string;
}

/** Rewind / fork targets for one session (`GET /api/session/branches`). */
export interface BranchCatalogModel {
  /** The session these targets belong to — the panel only renders it for that session. */
  readonly sessionId: SessionId;
  readonly targets: readonly BranchTargetModel[];
}

/**
 * Overlay data for the active session.
 *
 * Two kinds of member live here, and the difference matters:
 *
 * - **host-opened overlays** (`modelPicker`, `globalNotice`): non-`null` means
 *   "the host wants this on screen";
 * - **catalogs** (`commandCatalog`, `sessionCatalog`, `branchCatalog`): non-`null`
 *   means "the host has this data"; whether the panel is *open* is webview-local
 *   state (the overlays are driven by the composer, whose draft the host cannot
 *   see). The webview never derives catalog contents.
 */
export interface PanelsModel {
  readonly modelPicker: ModelPickerModel | null;
  readonly globalNotice: GlobalNoticeModel | null;
  readonly commandCatalog: CommandCatalogModel | null;
  readonly sessionCatalog: SessionCatalogModel | null;
  readonly branchCatalog: BranchCatalogModel | null;
}

/** The `panels` value with nothing open and nothing fetched. */
export const EMPTY_PANELS: PanelsModel = {
  modelPicker: null,
  globalNotice: null,
  commandCatalog: null,
  sessionCatalog: null,
  branchCatalog: null,
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
