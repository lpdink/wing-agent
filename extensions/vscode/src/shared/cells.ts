/**
 * Transcript cells — the chat view's content unit.
 *
 * Semantics mirror the TUI's `crates/wing/src/ui/chat_view/cell.rs`, with three
 * deliberate differences (agreed with the scheduler, see `design.md` D6):
 *
 * 1. The TUI's `UserMessage` / `PendingUserMessage` / `DiscardedUserMessage`
 *    variants collapse into one {@link UserCellModel} with a `state` field, so a
 *    state change is an `update` patch instead of a remove + insert.
 * 2. The TUI's `/model` picker cell is not a transcript cell here — it lives in
 *    `PanelsModel` (an overlay, not part of the conversation).
 * 3. Goal cells are out of scope (Goal orchestration is not implemented).
 *
 * Everything a renderer needs is pre-derived by the host: `display.subject` for a
 * tool row, windowed diff rows, normalized ask options. The webview never parses
 * partial JSON and never inspects raw protocol events.
 */

import type { CellId, EpochMs, RequestId, SessionId, ToolCallId } from './types';
import type { JsonValue } from './types';

/**
 * Discriminants of {@link CellModel}.
 *
 * Derive it from the union (single source of truth) instead of writing a second
 * list that can drift.
 */
export type CellKind = CellModel['kind'];

interface CellBase {
  /** Host-assigned, session-unique. Patches address cells through this id. */
  readonly id: CellId;
  /** Epoch ms the host created the cell (renderer ordering / diagnostics). */
  readonly createdAt: EpochMs;
}

// ── user ──────────────────────────────────────────────────────────────

/**
 * Delivery state of a user message.
 *
 * `pending` — sent to the gateway, not yet acknowledged by the agent
 * (`user_message_accepted`); `accepted` — fed into the model context;
 * `discarded` — dropped by an interrupt before reaching the model.
 */
export type UserMessageState = 'pending' | 'accepted' | 'discarded';

export interface UserCellModel extends CellBase {
  readonly kind: 'user';
  readonly text: string;
  readonly state: UserMessageState;
}

// ── assistant / thinking ──────────────────────────────────────────────

export interface AssistantCellModel extends CellBase {
  readonly kind: 'assistant';
  readonly text: string;
  /** True while the provider is still streaming into this cell. */
  readonly streaming: boolean;
}

export interface ThinkingCellModel extends CellBase {
  readonly kind: 'thinking';
  readonly text: string;
  readonly streaming: boolean;
  /** Wall-clock duration of the reasoning block, when the host knows it. */
  readonly durationMs: number | null;
}

// ── system ────────────────────────────────────────────────────────────

/**
 * System message severity.
 *
 * `info` — plain system message; `notice` — one-shot notice (retry, degradation)
 * rendered dimmer but never implying the turn ended; `warning` / `error` — the
 * gateway reported a problem.
 *
 * Mirrors the TUI: `Notice(level=warning|error)` → warning, `Error` → error,
 * anything else → info.
 */
export type SystemLevel = 'info' | 'notice' | 'warning' | 'error';

export interface SystemCellModel extends CellBase {
  readonly kind: 'system';
  readonly level: SystemLevel;
  readonly text: string;
}

// ── tool calls ────────────────────────────────────────────────────────

/**
 * Tool-call lifecycle.
 *
 * `streaming` — arguments still arriving from the model (partial JSON);
 * `pending` — arguments complete, execution not finished;
 * `success` / `failed` — terminal.
 */
export type ToolCallStatus = 'streaming' | 'pending' | 'success' | 'failed';

/** Host-derived presentation of a tool call's collapsed row. */
export interface ToolCallDisplayModel {
  /** Short label, e.g. `Bash`, `Read`, `TodoWrite` (falls back to the raw name). */
  readonly title: string;
  /** One-line subject, e.g. `pnpm test` or `src/host/extension.ts`; `''` when unknown. */
  readonly subject: string;
}

export interface ToolCallResultModel {
  readonly text: string;
  readonly isError: boolean;
  /** True when the host truncated `text` for transport. */
  readonly truncated: boolean;
}

export interface ToolCallCellModel extends CellBase {
  readonly kind: 'tool_call';
  readonly toolCallId: ToolCallId;
  /** Raw tool name as reported by the gateway (never localized). */
  readonly name: string;
  readonly status: ToolCallStatus;
  readonly display: ToolCallDisplayModel;
  /** Raw argument JSON text as streamed; may be partial while `status === 'streaming'`. */
  readonly argsText: string;
  /** Parsed arguments once complete — the host parses, the webview never does. */
  readonly args: JsonValue | null;
  readonly result: ToolCallResultModel | null;
  readonly startedAt: EpochMs | null;
  readonly finishedAt: EpochMs | null;
}

// ── diff ──────────────────────────────────────────────────────────────

/** One row of a file diff. */
export type DiffLineKind = 'context' | 'add' | 'del' | 'hunk';

export interface DiffLineModel {
  readonly kind: DiffLineKind;
  readonly text: string;
  /** 1-based line number in the old revision (`null` for added rows / hunk headers). */
  readonly oldLine: number | null;
  /** 1-based line number in the new revision (`null` for deleted rows / hunk headers). */
  readonly newLine: number | null;
}

export interface DiffCellModel extends CellBase {
  readonly kind: 'diff';
  readonly path: string;
  /**
   * 1-based absolute line number of the first windowed row, mirroring the
   * gateway's `old_start_line` / `new_start_line`. `0` when the payload carried
   * no window information (treated as "starts at line 1" by renderers).
   */
  readonly oldStartLine: number;
  readonly newStartLine: number;
  readonly lines: readonly DiffLineModel[];
  readonly added: number;
  readonly removed: number;
  /** True when the host windowed the payload (renderer shows a "…" affordance). */
  readonly truncated: boolean;
  /** Anchoring tool call (Write / Edit / BetterEdit), when known. */
  readonly toolCallId: ToolCallId | null;
}

// ── todo ──────────────────────────────────────────────────────────────

export type TodoItemStatus = 'pending' | 'in_progress' | 'completed';

export interface TodoItemModel {
  readonly content: string;
  readonly status: TodoItemStatus;
}

export interface TodoCellModel extends CellBase {
  readonly kind: 'todo';
  readonly items: readonly TodoItemModel[];
}

// ── ask ───────────────────────────────────────────────────────────────

export interface AskOptionModel {
  readonly label: string;
  readonly description: string;
}

export interface AskQuestionModel {
  readonly id: string;
  readonly question: string;
  /** Very short tab label (renderers fall back to `id` when empty). */
  readonly header: string;
  /** True when several options may be toggled. */
  readonly multiSelect: boolean;
  /** Selectable options; empty means free-form only. */
  readonly options: readonly AskOptionModel[];
  /** True when the user must pick from `options` (no free-form answer). */
  readonly required: boolean;
}

/** One question's answer, as submitted by the webview and echoed back by the host. */
export interface AskAnswerModel {
  readonly questionId: string;
  /** Chosen option labels (empty for free-form answers). */
  readonly selected: readonly string[];
  /** Free-form text (empty when the user only picked options). */
  readonly text: string;
}

/** Lifecycle of an ask request. */
export type AskState = 'awaiting' | 'answered' | 'cancelled';

export interface AskCellModel extends CellBase {
  readonly kind: 'ask';
  /** Correlation id echoed back with the answer (`tool_call_id` on the wire). */
  readonly requestId: RequestId;
  /** Session the ask belongs to (answers travel with the session id). */
  readonly sessionId: SessionId;
  readonly questions: readonly AskQuestionModel[];
  readonly state: AskState;
  /** Answers chosen so far; empty while `state === 'awaiting'`. */
  readonly answers: readonly AskAnswerModel[];
  /**
   * True for the Bash dangerous-command confirmation shape ("approve / deny").
   *
   * The TUI normalizes this legacy single-question event into a required
   * question; the host does the same, and flags it so renderers may use a more
   * compact layout.
   */
  readonly approval: boolean;
}

// ── metrics / separator ───────────────────────────────────────────────

/** Token + latency accounting for one LLM call or one whole turn. */
export interface UsageMetricsModel {
  readonly promptTokens: number;
  readonly completionTokens: number;
  readonly cachedTokens: number;
  /** Tokens per second of the streaming phase; `0` when unknown. */
  readonly tokensPerSecond: number;
  /** Time to first token in ms; `0` when unknown. */
  readonly ttftMs: number;
}

/**
 * Aggregated metrics for a finished turn (the TUI keeps the same numbers in its
 * status bar; here they become a transcript cell so they scroll with the turn).
 */
export interface MetricsCellModel extends CellBase {
  readonly kind: 'metrics';
  readonly usage: UsageMetricsModel;
  readonly durationMs: number | null;
  readonly model: string;
}

export interface SeparatorCellModel extends CellBase {
  readonly kind: 'separator';
  /** Optional label, e.g. `Turn 3`; `''` renders a bare rule. */
  readonly label: string;
}

// ── union ─────────────────────────────────────────────────────────────

/** One cell in the transcript. */
export type CellModel =
  | UserCellModel
  | AssistantCellModel
  | ThinkingCellModel
  | SystemCellModel
  | ToolCallCellModel
  | DiffCellModel
  | TodoCellModel
  | AskCellModel
  | MetricsCellModel
  | SeparatorCellModel;

/**
 * Exhaustiveness helper for `switch` statements over discriminated unions.
 *
 * Call it in the `default` branch: the argument is `never` only while every
 * variant is handled, so adding a variant turns every unhandled switch into a
 * compile error instead of a silent fallthrough.
 */
export function assertNever(value: never, context: string): never {
  throw new Error(`${context}: unhandled variant ${JSON.stringify(value)}`);
}
