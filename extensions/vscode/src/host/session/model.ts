import type {
  CellId,
  CellModel,
  CellPatch,
  ContextUsageModel,
  EpochMs,
  PanelsModel,
  RequestId,
  SessionAttention,
  SessionMetaModel,
  SessionStateModel,
  SessionStatus,
  SessionTotalsModel,
  SessionViewModel,
  ToolCallId,
  TurnViewModel,
  UsageMetricsModel,
  UserCellModel,
} from '../../shared';
import { EMPTY_PANELS, SESSION_TITLE_MAX_LENGTH } from '../../shared';

import { deriveTitle, truncateChars } from './derive';

/**
 * `SessionRecord` — the host's authority for one session's UI model.
 *
 * Two invariants make the whole bridge safe:
 *
 * 1. **Cells only change through the mutation helpers.** Each helper mutates the
 *    array and journals the equivalent {@link CellPatch}, so the transcript the
 *    webview rebuilds from patches can never drift from the host's copy.
 * 2. **Every patch-visible mutation is recorded in `journal`** in the exact
 *    order it happened; the manager ships the journal as one `patch` message
 *    (`seq + 1`) per reduction step, or throws it away and ships `hydrate` when
 *    the model was rebuilt from a replay (`replaced`).
 *
 * The record also carries the lookup maps the reducer needs (`toolCallId` →
 * cell, pending request → cell, awaiting ask → cell) — the TUI recomputes these
 * by scanning; an index is the same thing without the O(n) per event.
 */

/** Per-turn LLM usage (the numbers a metrics cell carries). */
export interface TurnUsage {
  readonly promptTokens: number;
  readonly completionTokens: number;
  readonly cachedTokens: number;
  readonly tokensPerSecond: number;
  readonly ttftMs: number;
}

/**
 * Streaming-arguments state of one in-flight tool call (host-only).
 *
 * `text` is every fragment received so far; the two render anchors let the
 * reducer decide whether a fragment is worth a cell update (and therefore a
 * bridge message) — see `TOOL_ARGS_RENDER_*` in `reducer.ts`.
 */
export interface ToolArgsStreamState {
  /** Accumulated raw argument text (the partial-JSON parse input). */
  readonly text: string;
  /** `text.length` at the last rendered update. */
  readonly renderedLength: number;
  /** `now()` of the last rendered update. */
  readonly renderedAtMs: EpochMs;
  /** Fragments received since the last rendered update. */
  readonly fragmentsSinceRender: number;
}

export interface SessionRecordOptions {
  readonly sessionId: string;
  readonly now: () => number;
  readonly workspace: string | null;
  readonly createdAt: string;
}

export class SessionRecord {
  readonly sessionId: string;
  readonly createdAt: string;

  private readonly nowFn: () => number;
  private readonly cellsInternal: CellModel[] = [];
  private readonly index = new Map<CellId, number>();
  /** Cells that are user messages still waiting for acceptance (kept at the tail). */
  private readonly pendingIds = new Set<CellId>();
  private cellSeq = 0;

  /** Patch ops produced since the last `takeJournal()`. */
  private journal: CellPatch[] = [];
  /** The model was rebuilt from a replay → the manager sends `hydrate`. */
  replaced = false;
  dirtyState = false;
  dirtyTabs = false;
  dirtyPanels = false;

  // ── identity / state ────────────────────────────────────────────────
  explicitTitle: string | null = null;
  title: string;
  status: SessionStatus = 'idle';
  attention: SessionAttention = 'none';
  meta: SessionMetaModel;
  context: ContextUsageModel = { usedTokens: 0, windowTokens: 0, messageCount: 0 };
  totals: SessionTotalsModel = { promptTokens: 0, completionTokens: 0, cachedTokens: 0 };
  turn: TurnViewModel = { active: false, startedAtMs: 0, lastResult: null };
  lastError: string | null = null;
  /** Backend-restored composer draft, cleared once the webview saw it. */
  draft: string | null = null;
  /**
   * One-shot token for {@link draft}: bumped every time the host *installs* a
   * draft (`setDraft`), never by consumption. The webview adopts a draft only
   * when the token it sees is newer than the last one it adopted, which is what
   * lets the *same* text be restored twice (two failed sends) while a re-sent
   * `state` (or an out-of-order older one) can never clobber typing.
   */
  draftSeq = 0;
  panels: PanelsModel = EMPTY_PANELS;
  /** Patch cursor; the manager bumps it exactly when it posts. */
  seq = 0;

  // ── reduction anchors ───────────────────────────────────────────────
  /** Tool call id → the ToolCall cell id (anchoring diffs / todos). */
  readonly toolCells = new Map<ToolCallId, CellId>();
  /** Request id → the pending user cell id. */
  readonly pendingRequests = new Map<RequestId, CellId>();
  /** Ask request id → the ask cell id (only while awaiting an answer). */
  readonly awaitingAsks = new Map<RequestId, CellId>();
  /** Streaming text targets (`append_text` goes here, not to "the last cell"). */
  lastAssistantCellId: CellId | null = null;
  lastThinkingCellId: CellId | null = null;
  thinkingStartedAtMs: EpochMs | null = null;
  /** Usage of the last LLM call in the current turn (metrics cell payload). */
  turnUsage: TurnUsage | null = null;
  turnUsageModel = '';
  metricsEmittedForTurn = false;

  /**
   * Streaming tool-argument buffers, keyed by tool call id.
   *
   * Host-only state (never serialised, never in a patch): the accumulated text
   * is the input of the partial-JSON parse, and the render anchors are what turn
   * "one event per provider chunk" into "one cell update per budget" — see
   * `reducer.applyToolCallStream`. The *cell* only ever carries a snapshot of
   * this buffer, which is what keeps `mirror.cells == record.cells` (a skipped
   * fragment is skipped in the model too, not just on the wire).
   */
  private readonly toolArgsStream = new Map<ToolCallId, ToolArgsStreamState>();

  /** Host clock (injectable: tests pin every timestamp). */
  now(): EpochMs {
    return this.nowFn();
  }

  /** A fresh, session-unique cell id. */
  newCellId(): CellId {
    this.cellSeq += 1;
    return `c${this.cellSeq}`;
  }

  constructor(options: SessionRecordOptions) {
    this.sessionId = options.sessionId;
    this.nowFn = options.now;
    this.createdAt = options.createdAt;
    this.meta = {
      model: '',
      provider: '',
      thinking: false,
      reasoningEffort: '',
      yolo: false,
      agent: '',
      workspace: options.workspace ?? '',
      createdAt: options.createdAt,
    };
    this.title = this.computeTitle();
  }

  // ── reads ───────────────────────────────────────────────────────────

  get cells(): readonly CellModel[] {
    return this.cellsInternal;
  }

  cellById(id: CellId): CellModel | undefined {
    const position = this.index.get(id);
    return position === undefined ? undefined : this.cellsInternal[position];
  }

  /** The last cell that is not a still-pending user message (TUI's "last cell"). */
  lastCommittedCell(): CellModel | null {
    for (let position = this.cellsInternal.length - 1; position >= 0; position -= 1) {
      const cell = this.cellsInternal[position];
      if (cell !== undefined && !this.pendingIds.has(cell.id)) {
        return cell;
      }
    }
    return null;
  }

  hasAwaitingAsk(): boolean {
    return this.awaitingAsks.size > 0;
  }

  /** First user message text (any state) — the title fallback source. */
  firstUserText(): string | null {
    for (const cell of this.cellsInternal) {
      if (cell.kind === 'user' && cell.text.trim() !== '') {
        return cell.text;
      }
    }
    return null;
  }

  /** Recompute the derived title; returns whether it changed. */
  refreshTitle(): boolean {
    const next = this.computeTitle();
    if (next === this.title) {
      return false;
    }
    this.title = next;
    return true;
  }

  private computeTitle(): string {
    return deriveTitle({
      explicit: this.explicitTitle,
      firstUserText: this.firstUserText(),
      workspace: this.meta.workspace === '' ? null : this.meta.workspace,
      maxLength: SESSION_TITLE_MAX_LENGTH,
      fallback: 'New session',
    });
  }

  // ── cell mutations (the only way cells change) ──────────────────────

  /** Push a cell at the end of the committed transcript (before pending messages). */
  pushCell(cell: CellModel): CellPatch {
    const anchor = this.lastCommittedCell();
    if (anchor !== null && this.pendingIds.size > 0) {
      return this.insertAfter(anchor.id, cell);
    }
    if (anchor === null && this.pendingIds.size > 0) {
      // Only pending messages exist, so "before them" has no anchor to insert
      // after — express the new order as a replacement (rare: a message queued
      // while the transcript itself is still empty).
      return this.replaceAll([cell, ...this.cellsInternal]);
    }
    this.insertAt(this.cellsInternal.length, cell);
    const patch: CellPatch = { op: 'append', cell };
    this.journal.push(patch);
    return patch;
  }

  /** Insert directly after `afterCellId`. Throws when the anchor is gone (a bug). */
  insertAfter(afterCellId: CellId, cell: CellModel): CellPatch {
    const position = this.index.get(afterCellId);
    if (position === undefined) {
      throw new Error(`insertAfter: unknown anchor cell ${afterCellId}`);
    }
    this.insertAt(position + 1, cell);
    const patch: CellPatch = { op: 'insert_after', afterCellId, cell };
    this.journal.push(patch);
    return patch;
  }

  /** Replace the cell with the same id (the cell object is stored as-is). */
  update(cell: CellModel): CellPatch {
    const position = this.index.get(cell.id);
    if (position === undefined) {
      throw new Error(`update: unknown cell ${cell.id}`);
    }
    this.cellsInternal[position] = cell;
    const patch: CellPatch = { op: 'update', cell };
    this.journal.push(patch);
    return patch;
  }

  /**
   * Append streamed text to a text-bearing cell.
   *
   * The patch carries only the delta; the manager slices deltas larger than
   * `MAX_PATCH_TEXT_CHUNK` so one `postMessage` never builds a multi-megabyte
   * string (shared constant, both sides know it).
   */
  appendText(cellId: CellId, text: string): CellPatch | null {
    if (text === '') {
      return null;
    }
    const position = this.index.get(cellId);
    if (position === undefined) {
      throw new Error(`appendText: unknown cell ${cellId}`);
    }
    const cell = this.cellsInternal[position];
    if (cell === undefined) {
      throw new Error(`appendText: dangling index for ${cellId}`);
    }
    switch (cell.kind) {
      case 'user':
      case 'assistant':
      case 'thinking':
      case 'system':
        this.cellsInternal[position] = { ...cell, text: cell.text + text };
        break;
      default:
        throw new Error(`appendText: cell ${cellId} (${cell.kind}) cannot receive text`);
    }
    const patch: CellPatch = { op: 'append_text', cellId, text };
    this.journal.push(patch);
    return patch;
  }

  /** Drop a cell (cancel paths). */
  remove(cellId: CellId): CellPatch {
    const position = this.index.get(cellId);
    if (position === undefined) {
      throw new Error(`remove: unknown cell ${cellId}`);
    }
    const cell = this.cellsInternal[position];
    this.cellsInternal.splice(position, 1);
    if (cell !== undefined) {
      this.forgetCell(cell);
    }
    this.reindexFrom(position);
    const patch: CellPatch = { op: 'remove', cellId };
    this.journal.push(patch);
    return patch;
  }

  /**
   * Replace the whole transcript (replay assembly does this through `clear()` +
   * pushes; retained for callers that already have a cell list).
   */
  replaceAll(cells: readonly CellModel[]): CellPatch {
    this.cellsInternal.splice(0, this.cellsInternal.length, ...cells);
    this.rebuildIndex();
    const patch: CellPatch = { op: 'replace_all', cells };
    this.journal.push(patch);
    return patch;
  }

  /** Drop every cell and reduction anchor (replay rebuild). */
  clear(): void {
    this.cellsInternal.length = 0;
    this.index.clear();
    this.pendingIds.clear();
    this.toolCells.clear();
    this.pendingRequests.clear();
    this.awaitingAsks.clear();
    this.toolArgsStream.clear();
    this.lastAssistantCellId = null;
    this.lastThinkingCellId = null;
    this.thinkingStartedAtMs = null;
    this.turnUsage = null;
    this.turnUsageModel = '';
    this.metricsEmittedForTurn = false;
    // `draftSeq` deliberately survives `clear()`: it is a monotone one-shot
    // token, and a replay that re-installs a draft must still look "newer" to a
    // webview that adopted an earlier one (see `SessionStateModel.draftSeq`).
  }

  // ── pending user messages ───────────────────────────────────────────

  /** Register a sent-but-not-accepted user message (kept at the tail). */
  addPendingUser(requestId: RequestId, text: string): { cell: UserCellModel; patch: CellPatch } {
    const cell: UserCellModel = {
      kind: 'user',
      id: this.newCellId(),
      createdAt: this.now(),
      text,
      state: 'pending',
    };
    const patch = this.pushCell(cell);
    this.pendingIds.add(cell.id);
    this.pendingRequests.set(requestId, cell.id);
    return { cell, patch };
  }

  /** Promote a pending message to `accepted` (TUI `promote_pending`). */
  promotePending(requestId: RequestId): CellPatch | null {
    const cellId = this.pendingRequests.get(requestId);
    if (cellId === undefined) {
      return null;
    }
    this.pendingRequests.delete(requestId);
    this.pendingIds.delete(cellId);
    const cell = this.cellById(cellId);
    if (cell === undefined || cell.kind !== 'user') {
      return null;
    }
    const patch = this.update({ ...cell, state: 'accepted' });
    this.dirtyState = true;
    return patch;
  }

  /** Promote every pending message (turn-end safety net). */
  promoteAllPending(): CellPatch[] {
    const patches: CellPatch[] = [];
    for (const requestId of [...this.pendingRequests.keys()]) {
      const patch = this.promotePending(requestId);
      if (patch !== null) {
        patches.push(patch);
      }
    }
    return patches;
  }

  /** Commit every pending message as `discarded` (interrupt semantics). */
  discardAllPending(): CellPatch[] {
    const patches: CellPatch[] = [];
    for (const [requestId, cellId] of [...this.pendingRequests.entries()]) {
      this.pendingRequests.delete(requestId);
      this.pendingIds.delete(cellId);
      const cell = this.cellById(cellId);
      if (cell === undefined || cell.kind !== 'user') {
        continue;
      }
      patches.push(this.update({ ...cell, state: 'discarded' }));
    }
    if (patches.length > 0) {
      this.dirtyState = true;
    }
    return patches;
  }

  /** Drop a pending message without committing it (send failed / never left). */
  removePending(requestId: RequestId): CellPatch | null {
    const cellId = this.pendingRequests.get(requestId);
    if (cellId === undefined) {
      return null;
    }
    this.pendingRequests.delete(requestId);
    this.pendingIds.delete(cellId);
    try {
      return this.remove(cellId);
    } catch {
      return null;
    }
  }

  // ── asks ────────────────────────────────────────────────────────────

  /** Register an answerable ask (the manager replies through this map). */
  registerAsk(requestId: RequestId, cellId: CellId): void {
    this.awaitingAsks.set(requestId, cellId);
  }

  /** Resolve an ask (answered locally or proven finished by an event). */
  resolveAsk(requestId: RequestId): CellId | null {
    const cellId = this.awaitingAsks.get(requestId) ?? null;
    this.awaitingAsks.delete(requestId);
    return cellId;
  }

  /** Mark every awaiting ask `cancelled` (turn end / interrupt). */
  cancelAwaitingAsks(): CellPatch[] {
    const patches: CellPatch[] = [];
    for (const cellId of [...this.awaitingAsks.values()]) {
      const cell = this.cellById(cellId);
      if (cell === undefined || cell.kind !== 'ask' || cell.state !== 'awaiting') {
        continue;
      }
      patches.push(this.update({ ...cell, state: 'cancelled' }));
    }
    this.awaitingAsks.clear();
    return patches;
  }

  // ── state derivation ────────────────────────────────────────────────

  /** Re-derive `status` from the model; returns whether it changed. */
  refreshStatus(): boolean {
    const next: SessionStatus = this.hasAwaitingAsk()
      ? 'waiting-for-input'
      : this.turn.active
        ? 'working'
        : 'idle';
    if (next === this.status) {
      return false;
    }
    this.status = next;
    this.dirtyState = true;
    this.dirtyTabs = true;
    return true;
  }

  /** Record one LLM call's usage so the reducer can emit a metrics cell at turn end. */
  recordTurnUsage(usage: TurnUsage, model: string): void {
    this.turnUsage = usage;
    this.turnUsageModel = model;
  }

  /** Forget the previous turn's usage (a new turn starts empty). */
  resetTurnUsage(): void {
    this.turnUsage = null;
    this.turnUsageModel = '';
  }

  /** The metrics cell for the finished turn, if there is anything to report. */
  takeMetricsCell(): { usage: UsageMetricsModel; durationMs: number | null; model: string } | null {
    const usage = this.turnUsage;
    if (usage === null || this.metricsEmittedForTurn) {
      return null;
    }
    this.metricsEmittedForTurn = true;
    const startedAt = this.turn.startedAtMs;
    const durationMs = startedAt > 0 ? Math.max(0, this.now() - startedAt) : null;
    return {
      usage: {
        promptTokens: usage.promptTokens,
        completionTokens: usage.completionTokens,
        cachedTokens: usage.cachedTokens,
        tokensPerSecond: usage.tokensPerSecond,
        ttftMs: usage.ttftMs,
      },
      durationMs,
      model: this.turnUsageModel,
    };
  }

  /** Truncate + store the last user-visible error. */
  setLastError(message: string): void {
    this.lastError = truncateChars(message, 400);
    this.dirtyState = true;
  }

  /**
   * Install a composer draft (resume / rewind / fork / sync, and the "this was
   * never sent" path) and stamp it with a fresh token.
   *
   * `null` only ever *clears* the local copy (the webview already adopted the
   * text); it does not consume a token, so the next install still counts as new.
   */
  setDraft(text: string | null): void {
    if (text !== null) {
      this.draftSeq += 1;
    }
    this.draft = text;
    this.dirtyState = true;
  }

  /** Forget the local copy without burning a token (the webview has the text). */
  consumeDraft(): void {
    this.draft = null;
  }

  clearLastError(): void {
    if (this.lastError !== null) {
      this.lastError = null;
      this.dirtyState = true;
    }
  }

  setAttention(attention: SessionAttention): void {
    if (this.attention !== attention) {
      this.attention = attention;
      this.dirtyState = true;
      this.dirtyTabs = true;
    }
  }

  // ── streaming tool arguments (host-only) ────────────────────────────

  /** The streaming-args buffer of one in-flight tool call, when there is one. */
  toolArgsStreamFor(toolCallId: ToolCallId): ToolArgsStreamState | undefined {
    return this.toolArgsStream.get(toolCallId);
  }

  /** Install/replace one tool call's streaming-args buffer. */
  setToolArgsStream(toolCallId: ToolCallId, state: ToolArgsStreamState): void {
    this.toolArgsStream.set(toolCallId, state);
  }

  /**
   * Forget one tool call's buffer — the call is final (`tool_call` / result) or
   * its cell is gone.
   */
  dropToolArgsStream(toolCallId: ToolCallId): void {
    this.toolArgsStream.delete(toolCallId);
  }

  // ── journal / snapshots ─────────────────────────────────────────────

  /** Journaled ops (in order) — the manager ships them as one patch batch. */
  takeJournal(): CellPatch[] {
    const ops = this.journal;
    this.journal = [];
    return ops;
  }

  peekJournal(): readonly CellPatch[] {
    return this.journal;
  }

  /** Host-side state snapshot (shared model, cells excluded). */
  stateModel(): SessionStateModel {
    return {
      sessionId: this.sessionId,
      title: this.title,
      status: this.status,
      attention: this.attention,
      meta: this.meta,
      context: this.context,
      totals: this.totals,
      turn: this.turn,
      lastError: this.lastError,
      draft: this.draft,
      draftSeq: this.draftSeq,
      panels: this.panels,
      seq: this.seq,
    };
  }

  /** Full snapshot (`hydrate` payload). */
  viewModel(): SessionViewModel {
    return { ...this.stateModel(), cells: [...this.cellsInternal] };
  }

  // ── internals ───────────────────────────────────────────────────────

  private insertAt(position: number, cell: CellModel): void {
    if (this.index.has(cell.id)) {
      throw new Error(`cell id ${cell.id} already exists in ${this.sessionId}`);
    }
    this.cellsInternal.splice(position, 0, cell);
    this.reindexFrom(position);
  }

  private reindexFrom(position: number): void {
    for (let cursor = position; cursor < this.cellsInternal.length; cursor += 1) {
      const cell = this.cellsInternal[cursor];
      if (cell !== undefined) {
        this.index.set(cell.id, cursor);
      }
    }
  }

  private rebuildIndex(): void {
    this.index.clear();
    this.pendingIds.clear();
    this.toolCells.clear();
    for (const [position, cell] of this.cellsInternal.entries()) {
      this.index.set(cell.id, position);
      if (cell.kind === 'tool_call') {
        this.toolCells.set(cell.toolCallId, cell.id);
      }
    }
  }

  /** Forget every lookup pointing at a removed cell. */
  private forgetCell(cell: CellModel): void {
    this.index.delete(cell.id);
    this.pendingIds.delete(cell.id);
    if (cell.kind === 'tool_call') {
      this.toolCells.delete(cell.toolCallId);
      this.toolArgsStream.delete(cell.toolCallId);
    }
    if (cell.kind === 'ask') {
      this.awaitingAsks.delete(cell.requestId);
    }
    if (cell.kind === 'user') {
      for (const [requestId, cellId] of [...this.pendingRequests.entries()]) {
        if (cellId === cell.id) {
          this.pendingRequests.delete(requestId);
        }
      }
    }
  }
}
