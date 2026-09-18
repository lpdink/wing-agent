import type {
  AskCellModel,
  CellModel,
  EpochMs,
  JsonValue,
  SystemLevel,
  TodoCellModel,
  ToolCallCellModel,
} from '../../shared';
import type {
  AskEvent,
  DiffContentEvent,
  KnownWingEvent,
  SessionMessage,
  SessionStateChangedEvent,
  SyncSessionEvent,
  WingEvent,
} from '../../core';
import { isKnownEvent } from '../../core';

import type { SessionRecord } from './model';
import {
  DIFF_MAX_ROWS,
  branchRow,
  buildDiffWindow,
  normalizeAsk,
  todoItemsFromArgs,
  toolDisplay,
  truncateChars,
} from './derive';
import { parsePartialJson } from './partial-json';

/**
 * The reduction lane — `WingEvent` in, model mutations out.
 *
 * **There is exactly one lane.** `applySync` (the `sync_session` replay) and
 * `applyLive` (the event stream) drive the same private handlers; the replay's
 * four material groups are assembled in the backend's documented order
 * (`messages → uncommitted → uncommitted_tools → events`) and the `events`
 * group is decoded into real `WingEvent`s by `src/core`, so a diff or an ask
 * that arrives through a replay takes the *same* code path as one that arrives
 * live. That property is what makes "replay vs active rendering diverge" bugs
 * structurally impossible instead of merely unlikely.
 *
 * Side effects a reduction may want (attention badges, toasts, scroll hints)
 * leave through {@link ReductionEffect}: the reducer never talks to the bridge
 * or to `vscode` itself, which is what keeps it a pure function of
 * (model, event).
 */

/** Anything a reduction wants the outside world to do. */
export type ReductionEffect =
  | { readonly kind: 'attention'; readonly level: 'result' | 'error' }
  | { readonly kind: 'toast'; readonly level: 'info' | 'warning' | 'error'; readonly message: string }
  | { readonly kind: 'scrollToBottom' };

/** Result preview cap for `turn_result` (display-only field). */
const TURN_RESULT_TEXT_MAX = 2_000;
/** Tool result transport cap (head + tail kept; the renderer shows a marker). */
const TOOL_RESULT_MAX_CHARS = 16_000;
const TOOL_RESULT_TAIL_CHARS = 4_000;

// ============================================================
// Live events
// ============================================================

/** Apply one live (or replayed) known event. */
export function applyLive(record: SessionRecord, event: WingEvent): ReductionEffect[] {
  if (!isKnownEvent(event)) {
    // Unknown type / malformed payload: the frame said something we cannot
    // represent. Never guess — the raw payload is already in the log.
    return [];
  }
  const effects: ReductionEffect[] = [];
  applyKnown(record, event, effects);
  return effects;
}

/**
 * Rebuild the whole model from a `sync_session` replay.
 *
 * Full replacement (`clear()` first), then the backend's assembly order. Effects
 * are dropped on purpose: restoring a session is not a notification event (a
 * replayed `error` must not pop a toast, a replayed `ask` must not steal focus).
 */
export function applySync(record: SessionRecord, sync: SyncSessionEvent): void {
  record.clear();
  record.replaced = true;

  // Agent snapshot: skills/rules banner (TUI parity) + model/provider/workspace.
  if (sync.agent !== null) {
    const skills = sync.agent.skills.length;
    const rules = sync.agent.rules.length;
    if (skills + rules > 0) {
      pushSystem(record, 'info', `loaded ${skills} skills, ${rules} rules`);
    }
    record.meta = {
      ...record.meta,
      model: sync.agent.model_name,
      provider: sync.agent.provider_name ?? '',
      workspace: sync.agent.workspace ?? record.meta.workspace,
    };
    record.dirtyState = true;
  }
  record.explicitTitle = sync.name;
  record.draft = sync.draft;
  record.dirtyState = true;
  record.dirtyTabs = true;

  // Mid-turn restore: the uncommitted projection proves the agent is still
  // running; `turn_started_at` restores the real elapsed time.
  const midTurn = sync.uncommitted !== null || sync.uncommitted_tools.length > 0;
  if (midTurn) {
    record.turn = {
      active: true,
      startedAtMs: parseIsoMs(sync.turn_started_at) ?? record.now(),
      lastResult: record.turn.lastResult,
    };
    record.metricsEmittedForTurn = false;
    record.dirtyState = true;
  }

  // 1. committed Message projections.
  for (const message of sync.messages) {
    applyMessageProjection(record, message);
  }
  // 2. the uncommitted assistant projection — same handler, single message.
  if (sync.uncommitted !== null) {
    applyMessageProjection(record, sync.uncommitted);
  }
  // 3. unterminated tool calls (raw fragments) — the live streaming handler.
  for (const tool of sync.uncommitted_tools) {
    applyToolCallStream(record, {
      toolCallId: tool.tool_call_id,
      toolName: tool.tool_name,
      fragment: tool.args_fragment,
      isFinal: false,
    });
  }
  // 4. durable fact events — the live event lane.
  const dropped: ReductionEffect[] = [];
  for (const event of sync.events) {
    if (isKnownEvent(event)) {
      applyKnown(record, event, dropped);
    }
  }

  record.refreshTitle();
  record.refreshStatus();
  record.dirtyState = true;
  record.dirtyTabs = true;
}

// ============================================================
// Dispatch
// ============================================================

function applyKnown(record: SessionRecord, event: KnownWingEvent, effects: ReductionEffect[]): void {
  switch (event.type) {
    // ── lifecycle ──────────────────────────────────────────────────
    case 'turn_started': {
      if (!record.turn.active) {
        record.turn = { ...record.turn, active: true, startedAtMs: record.now() };
      } else {
        record.turn = { ...record.turn, active: true };
      }
      record.metricsEmittedForTurn = false;
      record.clearLastError();
      record.dirtyState = true;
      record.refreshStatus();
      return;
    }
    case 'user_message_accepted': {
      const patch = record.promotePending(event.origin_request_id);
      if (patch === null) {
        // Another client's message (or a lost pending entry): nothing to render.
        return;
      }
      return;
    }
    case 'delivered':
      return;
    case 'done': {
      record.promoteAllPending();
      finishTurn(record, effects);
      return;
    }
    case 'interrupted': {
      record.discardAllPending();
      finishTurn(record, effects);
      effects.push({ kind: 'toast', level: 'info', message: 'Agent interrupted' });
      return;
    }
    case 'error': {
      finishTurn(record, effects);
      const text = event.message === '' ? 'Agent error' : event.message;
      pushSystem(record, 'error', text);
      record.setLastError(text);
      effects.push({ kind: 'attention', level: 'error' });
      return;
    }
    case 'notice': {
      // Informational only — deliberately does NOT finish the turn: retrying a
      // failed LLM call means the turn is still running.
      let text = event.message === '' ? 'notice' : event.message;
      if (event.attempt !== null && event.max_attempts !== null && event.retry_in_s !== null) {
        text = `${text} (attempt ${event.attempt}/${event.max_attempts}, retrying in ${Math.round(event.retry_in_s)}s)`;
      }
      const level: SystemLevel = event.level === 'warning' || event.level === 'error' ? 'warning' : 'notice';
      pushSystem(record, level, text);
      return;
    }

    // ── reasoning / text ───────────────────────────────────────────
    case 'reasoning': {
      const last = record.lastCommittedCell();
      if (last !== null && last.kind === 'thinking') {
        // "The last cell is a thinking block" is the whole rule (TUI parity) —
        // a replayed block adopts live deltas instead of growing a second cell.
        if (!last.streaming) {
          record.update({ ...last, streaming: true });
        }
        if (record.thinkingStartedAtMs === null) {
          record.thinkingStartedAtMs = record.now();
        }
        record.appendText(last.id, event.content);
        record.lastThinkingCellId = last.id;
        return;
      }
      closeStreamingText(record);
      const cell: CellModel = {
        kind: 'thinking',
        id: record.newCellId(),
        createdAt: record.now(),
        text: event.content,
        streaming: true,
        durationMs: null,
      };
      pushWithSeparator(record, cell);
      record.lastThinkingCellId = cell.id;
      record.thinkingStartedAtMs = record.now();
      return;
    }
    case 'text': {
      const last = record.lastCommittedCell();
      if (last !== null && last.kind === 'assistant') {
        // Same rule as reasoning: a replayed assistant cell (resume mid-turn)
        // takes live deltas — that is what keeps replay and live identical.
        if (!last.streaming) {
          record.update({ ...last, streaming: true });
        }
        record.appendText(last.id, event.content);
        record.lastAssistantCellId = last.id;
        return;
      }
      closeStreamingText(record);
      const cell: CellModel = {
        kind: 'assistant',
        id: record.newCellId(),
        createdAt: record.now(),
        text: event.content,
        streaming: true,
      };
      pushWithSeparator(record, cell);
      record.lastAssistantCellId = cell.id;
      return;
    }

    // ── tools ──────────────────────────────────────────────────────
    case 'tool_call_stream': {
      applyToolCallStream(record, {
        toolCallId: event.tool_call_id,
        toolName: event.tool_name,
        fragment: event.args_fragment,
        isFinal: event.is_final,
      });
      return;
    }
    case 'tool_call': {
      closeStreamingText(record);
      const existing = record.toolCells.get(event.tool_call_id);
      if (existing !== undefined) {
        const cell = record.cellById(existing);
        if (cell !== undefined && cell.kind === 'tool_call') {
          record.update({
            ...cell,
            name: event.tool_name,
            args: event.tool_args,
            argsText: '',
            display: toolDisplay(event.tool_name, event.tool_args),
            status: 'pending',
            startedAt: cell.startedAt ?? record.now(),
          });
          return;
        }
      }
      const cell: ToolCallCellModel = {
        kind: 'tool_call',
        id: record.newCellId(),
        createdAt: record.now(),
        toolCallId: event.tool_call_id,
        name: event.tool_name,
        status: 'pending',
        display: toolDisplay(event.tool_name, event.tool_args),
        argsText: '',
        args: event.tool_args,
        result: null,
        startedAt: record.now(),
        finishedAt: null,
      };
      record.pushCell(cell);
      record.toolCells.set(cell.toolCallId, cell.id);
      record.refreshTitle();
      return;
    }
    case 'tool_call_result': {
      closeStreamingText(record);
      // A tool call finishing proves that its ask (a Bash confirmation, an
      // AskUserQuestion) is over — including when another client answered it.
      const askCellId = record.resolveAsk(event.tool_call_id);
      if (askCellId !== null) {
        const ask = record.cellById(askCellId);
        if (ask !== undefined && ask.kind === 'ask' && ask.state === 'awaiting') {
          record.update({ ...ask, state: 'answered' });
        }
        record.refreshStatus();
      }

      const result = truncateToolResult(event.tool_result);
      const success = event.tool_success;
      const existing = record.toolCells.get(event.tool_call_id);
      const previous = existing === undefined ? undefined : record.cellById(existing);
      if (existing !== undefined && previous !== undefined && previous.kind === 'tool_call') {
        record.update({
          ...previous,
          status: success ? 'success' : 'failed',
          result: { text: result, isError: !success, truncated: result !== event.tool_result },
          finishedAt: record.now(),
        });
        if (success && event.tool_name === 'TodoWrite') {
          const todo = todoItemsFromArgs(event.tool_args);
          if (todo !== null) {
            const todoCell: TodoCellModel = {
              kind: 'todo',
              id: record.newCellId(),
              createdAt: record.now(),
              items: todo,
            };
            insertAfterToolCall(record, event.tool_call_id, todoCell);
          }
        }
        return;
      }
      // Orphan result (the tool-call cell is gone) — build the pair like the TUI.
      const cell: ToolCallCellModel = {
        kind: 'tool_call',
        id: record.newCellId(),
        createdAt: record.now(),
        toolCallId: event.tool_call_id,
        name: event.tool_name,
        status: success ? 'success' : 'failed',
        display: toolDisplay(event.tool_name, event.tool_args),
        argsText: '',
        args: event.tool_args,
        result: { text: result, isError: !success, truncated: result !== event.tool_result },
        startedAt: null,
        finishedAt: record.now(),
      };
      record.pushCell(cell);
      record.toolCells.set(cell.toolCallId, cell.id);
      if (success && event.tool_name === 'TodoWrite') {
        const todo = todoItemsFromArgs(event.tool_args);
        if (todo !== null) {
          record.pushCell({
            kind: 'todo',
            id: record.newCellId(),
            createdAt: record.now(),
            items: todo,
          });
        }
      }
      record.refreshTitle();
      return;
    }
    case 'diff_content': {
      applyDiff(record, event);
      return;
    }

    // ── metrics / state ────────────────────────────────────────────
    case 'llm_call_metrics': {
      record.totals = {
        promptTokens: record.totals.promptTokens + event.prompt_tokens,
        completionTokens: record.totals.completionTokens + event.completion_tokens,
        cachedTokens: record.totals.cachedTokens + event.cached_tokens,
      };
      record.recordTurnUsage(
        {
          promptTokens: event.prompt_tokens,
          completionTokens: event.completion_tokens,
          cachedTokens: event.cached_tokens,
          tokensPerSecond: event.tokens_per_sec,
          ttftMs: event.first_chunk_rt_ms,
        },
        event.model,
      );
      record.dirtyState = true;
      return;
    }
    case 'context_stats': {
      record.context = {
        usedTokens: event.total_tokens,
        // `0` means the gateway does not know a window size — keep the last one.
        windowTokens:
          event.context_window_tokens > 0 ? event.context_window_tokens : record.context.windowTokens,
        messageCount: event.message_count,
      };
      record.dirtyState = true;
      return;
    }
    case 'session_state_changed': {
      applyMetaChanges(record, event);
      return;
    }

    // ── ask ────────────────────────────────────────────────────────
    case 'ask': {
      applyAsk(record, event);
      effects.push({ kind: 'scrollToBottom' });
      return;
    }

    // ── turn result ────────────────────────────────────────────────
    case 'turn_result': {
      const usage = event.usage;
      const totalTokens =
        usage === null ? null : numberField(usage, 'input_tokens') + numberField(usage, 'output_tokens');
      record.turn = {
        ...record.turn,
        lastResult: {
          subtype: event.subtype,
          isError: event.is_error,
          durationMs: event.duration_ms,
          numTurns: event.num_turns,
          totalTokens,
          resultText: event.result === null ? null : truncateChars(event.result, TURN_RESULT_TEXT_MAX),
        },
      };
      record.dirtyState = true;
      effects.push({ kind: 'attention', level: event.is_error ? 'error' : 'result' });
      return;
    }

    // ── replay containers (handled by applySync / ignored here) ────
    case 'sync_session':
      // A sync arriving on the live lane (subscribe / rewind / fork) — full
      // replacement, exactly like the replay lane.
      applySync(record, event);
      return;

    // ── panel data ─────────────────────────────────────────────────
    case 'branch_targets': {
      // The gateway re-emits the candidate list after a rewind / fork; refresh
      // the picker when it is open (the rows themselves are not transcript).
      if (record.panels.branches !== null) {
        const rows = event.targets.map((target) => branchRow(target));
        record.panels = {
          ...record.panels,
          branches: {
            mode: record.panels.branches.mode,
            rows,
            activeIndex: rows.length > 0 ? 0 : null,
          },
        };
        record.dirtyPanels = true;
      }
      return;
    }

    // ── events with no cell renderer (forward tolerant, TUI parity) ─
    case 'compact_done':
    case 'assistant_turn':
    case 'tool_result_turn':
    case 'session_init':
      return;
  }
}

// ============================================================
// Material handlers
// ============================================================

/** Replay one history `Message` projection (committed or uncommitted). */
function applyMessageProjection(record: SessionRecord, message: SessionMessage): void {
  switch (message.role) {
    case 'user': {
      if (message.content === '') {
        return;
      }
      const cell: CellModel = {
        kind: 'user',
        id: record.newCellId(),
        createdAt: record.now(),
        text: message.content,
        state: 'accepted',
      };
      record.pushCell(cell);
      record.refreshTitle();
      return;
    }
    case 'assistant': {
      if (message.reasoning_content !== null && message.reasoning_content !== '') {
        record.pushCell({
          kind: 'thinking',
          id: record.newCellId(),
          createdAt: record.now(),
          text: message.reasoning_content,
          streaming: false,
          durationMs: null,
        });
      }
      for (const call of message.tool_calls) {
        const cell: ToolCallCellModel = {
          kind: 'tool_call',
          id: record.newCellId(),
          createdAt: record.now(),
          toolCallId: call.id,
          name: call.name,
          status: 'pending',
          display: toolDisplay(call.name, call.arguments),
          argsText: '',
          args: call.arguments,
          result: null,
          startedAt: null,
          finishedAt: null,
        };
        record.pushCell(cell);
        record.toolCells.set(cell.toolCallId, cell.id);
      }
      if (message.content !== '') {
        record.pushCell({
          kind: 'assistant',
          id: record.newCellId(),
          createdAt: record.now(),
          text: message.content,
          streaming: false,
        });
      }
      return;
    }
    case 'tool': {
      const toolCallId = message.tool_call_id;
      if (toolCallId === null || toolCallId === '') {
        return;
      }
      // Results are persisted in completion order (concurrent tool execution),
      // so pairing is strictly by id.
      const cellId = record.toolCells.get(toolCallId);
      const cell = cellId === undefined ? undefined : record.cellById(cellId);
      if (cell === undefined || cell.kind !== 'tool_call') {
        pushSystem(record, 'info', `Tool result (orphan): ${truncateToolResult(message.content)}`);
        return;
      }
      record.update({
        ...cell,
        status: 'success',
        result: { text: truncateToolResult(message.content), isError: false, truncated: false },
        finishedAt: record.now(),
      });
      if (cell.name === 'TodoWrite') {
        const todo = todoItemsFromArgs(cell.args);
        if (todo !== null) {
          insertAfterToolCall(record, toolCallId, {
            kind: 'todo',
            id: record.newCellId(),
            createdAt: record.now(),
            items: todo,
          });
        }
      }
      return;
    }
    default:
      return;
  }
}

/** `tool_call_stream` — the live streaming handler (also used by replay). */
function applyToolCallStream(
  record: SessionRecord,
  input: {
    readonly toolCallId: string;
    readonly toolName: string;
    readonly fragment: string;
    readonly isFinal: boolean;
  },
): void {
  closeStreamingText(record);
  const existing = record.toolCells.get(input.toolCallId);
  const previous = existing === undefined ? undefined : record.cellById(existing);
  if (previous !== undefined && previous.kind === 'tool_call') {
    const argsText = previous.argsText + input.fragment;
    record.update({
      ...previous,
      argsText,
      display: toolDisplay(previous.name, parsePartialJson(argsText)),
      status: input.isFinal ? 'pending' : previous.status,
    });
    return;
  }
  const argsText = input.fragment;
  const cell: ToolCallCellModel = {
    kind: 'tool_call',
    id: record.newCellId(),
    createdAt: record.now(),
    toolCallId: input.toolCallId,
    name: input.toolName,
    status: input.isFinal ? 'pending' : 'streaming',
    display: toolDisplay(input.toolName, parsePartialJson(argsText)),
    argsText,
    args: null,
    result: null,
    startedAt: null,
    finishedAt: null,
  };
  record.pushCell(cell);
  record.toolCells.set(cell.toolCallId, cell.id);
  record.refreshTitle();
}

/** `diff_content` — anchor the diff after the tool call that produced it. */
function applyDiff(record: SessionRecord, event: DiffContentEvent): void {
  closeStreamingText(record);
  const window = buildDiffWindow({
    oldText: event.old_text,
    newText: event.new_text,
    oldStartLine: event.old_start_line,
    newStartLine: event.new_start_line,
    maxRows: DIFF_MAX_ROWS,
  });
  const cell: CellModel = {
    kind: 'diff',
    id: record.newCellId(),
    createdAt: record.now(),
    path: event.path,
    oldStartLine: event.old_start_line,
    newStartLine: event.new_start_line,
    lines: window.lines,
    added: window.added,
    removed: window.removed,
    truncated: window.truncated,
    toolCallId: event.tool_call_id === '' ? null : event.tool_call_id,
  };
  insertAfterToolCall(record, event.tool_call_id, cell);
}

/** `ask` — normalize both wire shapes through the one entry. */
function applyAsk(record: SessionRecord, event: AskEvent): void {
  closeStreamingText(record);
  const normalized = normalizeAsk(event);
  const questions = normalized.questions;
  const awaitingId = record.awaitingAsks.get(event.tool_call_id);
  const awaitingCell = awaitingId === undefined ? undefined : record.cellById(awaitingId);
  if (awaitingCell !== undefined && awaitingCell.kind === 'ask' && awaitingCell.state === 'awaiting') {
    // Same pending ask re-delivered (e.g. replayed twice) — keep one cell.
    record.update({
      ...awaitingCell,
      questions,
      approval: normalized.approval,
      answers: [],
    });
    return;
  }
  const cell: AskCellModel = {
    kind: 'ask',
    id: record.newCellId(),
    createdAt: record.now(),
    requestId: event.tool_call_id,
    sessionId: record.sessionId,
    questions,
    state: normalized.answerable ? 'awaiting' : 'cancelled',
    answers: [],
    approval: normalized.approval,
  };
  record.pushCell(cell);
  if (normalized.answerable) {
    record.registerAsk(cell.requestId, cell.id);
  }
  record.refreshStatus();
}

/** `session_state_changed` — merge the non-null fields into the meta model. */
function applyMetaChanges(record: SessionRecord, event: SessionStateChangedEvent): void {
  const meta = { ...record.meta };
  if (event.model !== null) {
    meta.model = event.model;
  }
  if (event.thinking !== null) {
    meta.thinking = event.thinking;
  }
  if (event.reasoning_effort !== null) {
    meta.reasoningEffort = event.reasoning_effort;
  }
  if (event.yolo !== null) {
    meta.yolo = event.yolo;
  }
  if (event.agent !== null) {
    meta.agent = event.agent;
  }
  if (event.title !== null) {
    record.explicitTitle = event.title;
    record.refreshTitle();
  }
  record.meta = meta;
  record.dirtyState = true;
  record.dirtyTabs = true;
}

// ============================================================
// Shared helpers
// ============================================================

/** Turn end (done / interrupted / error): cancel asks, flush metrics, go idle. */
function finishTurn(record: SessionRecord, _effects: ReductionEffect[]): void {
  record.cancelAwaitingAsks();
  closeStreamingText(record);
  const wasActive = record.turn.active;
  record.turn = { active: false, startedAtMs: 0, lastResult: record.turn.lastResult };
  record.dirtyState = true;

  const metrics = record.takeMetricsCell();
  if (metrics !== null) {
    record.pushCell({
      kind: 'metrics',
      id: record.newCellId(),
      createdAt: record.now(),
      usage: metrics.usage,
      durationMs: metrics.durationMs,
      model: metrics.model,
    });
  }
  record.refreshStatus();
  void wasActive;
}

/**
 * Push a thinking/assistant cell with the ReAct separator rule (TUI `push`):
 * a new text block that follows a tool call gets a bare separator first.
 */
function pushWithSeparator(record: SessionRecord, cell: CellModel): void {
  let last: CellModel | null = null;
  for (let position = record.cells.length - 1; position >= 0; position -= 1) {
    const candidate = record.cells[position];
    if (candidate === undefined || candidate.kind === 'separator') {
      continue;
    }
    last = candidate;
    break;
  }
  if (last !== null && last.kind === 'tool_call') {
    record.pushCell({ kind: 'separator', id: record.newCellId(), createdAt: record.now(), label: '' });
  }
  record.pushCell(cell);
}

/** Finalize the current streaming text anchors (streaming=false, thinking duration). */
function closeStreamingText(record: SessionRecord): void {
  const assistantId = record.lastAssistantCellId;
  if (assistantId !== null) {
    const cell = record.cellById(assistantId);
    if (cell !== undefined && cell.kind === 'assistant' && cell.streaming) {
      record.update({ ...cell, streaming: false });
    }
    record.lastAssistantCellId = null;
  }
  const thinkingId = record.lastThinkingCellId;
  if (thinkingId !== null) {
    const cell = record.cellById(thinkingId);
    if (cell !== undefined && cell.kind === 'thinking') {
      const startedAt = record.thinkingStartedAtMs;
      const durationMs =
        cell.streaming && startedAt !== null ? Math.max(0, record.now() - startedAt) : cell.durationMs;
      if (cell.streaming || durationMs !== cell.durationMs) {
        record.update({ ...cell, streaming: false, durationMs });
      }
    }
    record.lastThinkingCellId = null;
    record.thinkingStartedAtMs = null;
  }
}

/**
 * Anchor a derived cell (diff / todo) directly after its ToolCall, skipping
 * already-anchored siblings so multiple emissions keep their order (TUI
 * `insert_after_tool_call`). Unknown anchor → append.
 */
function insertAfterToolCall(record: SessionRecord, toolCallId: string, cell: CellModel): void {
  const toolCellId = record.toolCells.get(toolCallId);
  if (toolCellId === undefined) {
    record.pushCell(cell);
    return;
  }
  const cells = record.cells;
  let position = -1;
  for (let index = 0; index < cells.length; index += 1) {
    const candidate = cells[index];
    if (candidate !== undefined && candidate.id === toolCellId) {
      position = index;
      break;
    }
  }
  if (position === -1) {
    record.pushCell(cell);
    return;
  }
  while (true) {
    const next = cells[position + 1];
    if (next === undefined || (next.kind !== 'diff' && next.kind !== 'todo')) {
      break;
    }
    position += 1;
  }
  const anchor = cells[position];
  if (anchor === undefined) {
    record.pushCell(cell);
    return;
  }
  record.insertAfter(anchor.id, cell);
}

/** Push a system cell (the notice / error lane). */
function pushSystem(record: SessionRecord, level: SystemLevel, text: string): void {
  record.pushCell({
    kind: 'system',
    id: record.newCellId(),
    createdAt: record.now(),
    level,
    text,
  });
}

/** Tool results are capped for transport (head + tail, with a marker in between). */
export function truncateToolResult(text: string): string {
  if (text.length <= TOOL_RESULT_MAX_CHARS) {
    return text;
  }
  const omitted = text.length - TOOL_RESULT_MAX_CHARS;
  const head = text.slice(0, TOOL_RESULT_MAX_CHARS - TOOL_RESULT_TAIL_CHARS);
  const tail = text.slice(text.length - TOOL_RESULT_TAIL_CHARS);
  return `${head}\n… (${omitted} chars truncated) …\n${tail}`;
}

function numberField(value: JsonValue, key: string): number {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    return 0;
  }
  const field = (value as Record<string, JsonValue>)[key];
  return typeof field === 'number' && Number.isFinite(field) ? field : 0;
}

/** Parse the gateway's UTC ISO-8601 timestamps (`2026-09-18T08:00:00.123456`). */
export function parseIsoMs(value: string | null): EpochMs | null {
  if (value === null || value === '') {
    return null;
  }
  const normalized = /Z$|[+-]\d{2}:?\d{2}$/.test(value) ? value : `${value}Z`;
  const parsed = Date.parse(normalized);
  return Number.isNaN(parsed) ? null : parsed;
}
