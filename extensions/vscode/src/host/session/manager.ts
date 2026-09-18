import type {
  AskAnswerModel,
  AskCellModel,
  CellPatch,
  HostToWebviewMessage,
  PanelsModel,
  PromptCommandModel,
  SessionId,
  UiActionModel,
} from '../../shared';
import { unhandledVariant } from '../../shared';
import type { CoreLogger, GatewayConnection, GatewayHttpClient, SessionStatus, WingEvent } from '../../core';
import { GatewayHttpError, createClientRequest, isKnownEvent } from '../../core';

import type { WebviewIntent } from '../bridge';
import type { EditorActions } from '../editorActions';
import { SerialQueue } from './queue';
import type { ReductionEffect } from './reducer';
import { applyLive, applySync } from './reducer';
import { SessionRecord } from './model';
import { branchRow, buildAskReply } from './derive';

/**
 * `SessionManager` — the tab list, the control plane and the bridge producer.
 *
 * Responsibilities, and nothing else:
 *
 * - **Tabs**: which sessions are open, which is active, in what order.
 * - **Timing**: `create → subscribe → (send allowed)`, `resume → subscribe`,
 *   `fork → new tab → subscribe`, `rewind → sync replaces the model`.
 * - **Routing**: a gateway event is reduced into the record of its own session
 *   and into no other.
 * - **Reduction hosting**: it hands events to the reducer, then ships the
 *   journal as `patch` / `hydrate` / `state` / `panels` / `tabs` messages.
 *
 * It owns no socket and no HTTP client — it borrows them through the deps so the
 * host can swap them out (reconnect, settings change) without this class
 * knowing.
 */

export type { ReductionEffect };

export interface BridgeSink {
  post(message: HostToWebviewMessage): void;
}

export interface SessionManagerDeps {
  /** The webview sink, or `null` when no view is attached (messages are dropped). */
  readonly sink: () => BridgeSink | null;
  readonly connection: () => GatewayConnection | null;
  readonly http: () => GatewayHttpClient | null;
  /** First workspace folder, or `null` (new sessions need one). */
  readonly workspaceFolder: () => string | null;
  readonly editor: EditorActions;
  /** Surfaces a blocking message (`vscode.window.showErrorMessage`). */
  readonly reportError: (message: string) => void;
  readonly now: () => number;
  readonly logger: CoreLogger;
}

/** One open session: the model plus the management state around it. */
interface ManagedSession {
  readonly record: SessionRecord;
  /** The gateway route is attached (subscribe resolved, or a sync arrived). */
  subscribed: boolean;
  /** Closed by the user — late async work must not resurrect it. */
  closed: boolean;
  /** The session no longer exists on the gateway. */
  gone: boolean;
  /** Pending resubscribe retry timer. */
  retryTimer: ReturnType<typeof setTimeout> | null;
  retryAttempt: number;
}

/** Chunk size for `append_text` ops — one postMessage must stay small. */
const TEXT_CHUNK = 64 * 1024;

export class SessionManager {
  private readonly sessions = new Map<SessionId, ManagedSession>();
  private order: SessionId[] = [];
  private active: SessionId | null = null;
  private webviewReady = false;
  private initialSessionRequested = false;
  private commandsCatalog: readonly PromptCommandModel[] | null = null;
  private globalNotice: PanelsModel['globalNotice'] = null;
  /** Last notice text already surfaced as a toast (no toast spam on retries). */
  private toastedNotice: string | null = null;
  private disposed = false;
  /** Structural operations (open / close / resume / fork / resubscribe) serialize here. */
  private readonly structure = new SerialQueue();

  constructor(private readonly deps: SessionManagerDeps) {}

  // ── reads (tests / commands) ────────────────────────────────────────

  get openSessionIds(): readonly SessionId[] {
    return [...this.order];
  }

  get activeSessionId(): SessionId | null {
    return this.active;
  }

  record(sessionId: SessionId): SessionRecord | undefined {
    return this.sessions.get(sessionId)?.record;
  }

  // ── bridge lifecycle ────────────────────────────────────────────────

  /**
   * The webview mounted (or reloaded): answer with the full snapshot.
   *
   * Posting the snapshot is synchronous; the initial session (if any) is kicked
   * off without blocking the handshake — a slow `create` must not stall the
   * `ready` answer.
   */
  onReady(protocolVersion: number): void {
    this.deps.logger.debug(`webview ready (bridge v${protocolVersion})`);
    this.webviewReady = true;
    this.postTabs();
    for (const managed of this.all()) {
      this.postHydrate(managed.record);
    }
    // With no session open there is no panel to carry the banner, so a fresh
    // webview learns about a broken gateway through a toast.
    if (this.sessions.size === 0 && this.globalNotice !== null) {
      this.toast(this.globalNotice.level, this.globalNotice.text);
      this.toastedNotice = this.globalNotice.text;
    }
    void this.maybeCreateInitialSession();
  }

  /** The webview lost continuity — answer with that session's snapshot. */
  onResync(sessionId: SessionId): void {
    const managed = this.sessions.get(sessionId);
    if (managed === undefined) {
      this.deps.logger.warn(`resync for unknown session ${sessionId}; resending tabs`);
      this.postTabs();
      return;
    }
    this.postHydrate(managed.record);
  }

  // ── gateway lifecycle (called by the host) ──────────────────────────

  /** A live connection is available (first connect or reconnect). */
  async onConnected(): Promise<void> {
    await this.structure.run(async () => {
      await this.resubscribeAll();
    });
    await this.ensureCommandsCatalog();
    await this.maybeCreateInitialSession();
  }

  /** The connection went away — parked until the next `onConnected`. */
  onDisconnected(): void {
    for (const managed of this.all()) {
      managed.subscribed = false;
      this.cancelRetry(managed);
    }
  }

  /** Update the app-level banner (all sessions show the same notice). */
  setGlobalNotice(notice: PanelsModel['globalNotice']): void {
    this.globalNotice = notice;
    for (const managed of this.all()) {
      managed.record.panels = { ...managed.record.panels, globalNotice: notice };
      managed.record.dirtyPanels = true;
      this.postPanels(managed.record);
    }
    // No session to render the banner: tell the user directly (once per text —
    // a failing retry ladder must not turn into a toast storm).
    if (notice !== null && this.sessions.size === 0 && notice.text !== this.toastedNotice) {
      this.toast(notice.level, notice.text);
      this.toastedNotice = notice.text;
    }
    if (notice === null) {
      this.toastedNotice = null;
    }
  }

  // ── event routing ───────────────────────────────────────────────────

  /**
   * Reduce one gateway event into its own session.
   *
   * The event's `session_id` decides the target; a sessionless event (a few
   * gateway-level facts) goes to the active tab, which is what a user sees as
   * "the session I am looking at". Anything else is dropped with a debug log —
   * never guessed into another tab.
   */
  handleEvent(event: WingEvent): void {
    const sessionId = event.session_id;
    if (sessionId !== null && sessionId !== '') {
      const managed = this.sessions.get(sessionId);
      if (managed === undefined) {
        this.deps.logger.debug(`event ${event.type} for unopened session ${sessionId}; ignored`);
        return;
      }
      this.reduce(managed, event);
      return;
    }
    const active = this.active === null ? undefined : this.sessions.get(this.active);
    if (active === undefined) {
      this.deps.logger.debug(`sessionless event ${event.type} with no active tab; ignored`);
      return;
    }
    this.reduce(active, event);
  }

  private reduce(managed: ManagedSession, event: WingEvent): void {
    const record = managed.record;
    if (managed.closed) {
      return;
    }
    const effects: ReductionEffect[] = [];
    if (isKnownEvent(event) && event.type === 'sync_session') {
      applySync(record, event);
      managed.subscribed = true;
    } else {
      effects.push(...applyLive(record, event));
    }
    // Attention badges are "a turn finished while you were elsewhere".
    for (const effect of effects) {
      if (effect.kind === 'attention' && this.active !== record.sessionId) {
        record.setAttention(effect.level);
      }
    }
    this.flush(record);
    for (const effect of effects) {
      switch (effect.kind) {
        case 'attention':
          break;
        case 'toast':
          this.postUi({ kind: 'toast', level: effect.level, message: effect.message });
          break;
        case 'scrollToBottom':
          this.postUi({ kind: 'scrollToBottom' });
          break;
        default:
          unhandledVariant(effect, 'SessionManager.reduce');
      }
    }
  }

  // ── intent dispatch ─────────────────────────────────────────────────

  /** Handle one webview intent. Never throws; failures become toasts. */
  async onIntent(intent: WebviewIntent): Promise<void> {
    try {
      await this.dispatchIntent(intent);
    } catch (error) {
      this.reportFailure('Unexpected error', error);
    }
  }

  private async dispatchIntent(intent: WebviewIntent): Promise<void> {
    switch (intent.type) {
      case 'sendMessage':
        this.sendText(intent.sessionId, intent.text, { requireSubscribed: true });
        return;
      case 'interrupt':
        await this.interrupt(intent.sessionId);
        return;
      case 'answerAsk':
        this.answerAsk(intent.sessionId, intent.requestId, intent.answers);
        return;
      case 'approveTool':
        this.approveTool(intent.sessionId, intent.requestId, intent.decision);
        return;
      case 'newSession':
        await this.newSession();
        return;
      case 'closeSession':
        await this.closeSession(intent.sessionId);
        return;
      case 'activateSession':
        await this.activate(intent.sessionId);
        return;
      case 'compact':
        await this.compact(intent.sessionId, null);
        return;
      case 'setModel':
        await this.updateSession(
          intent.sessionId,
          { model: intent.model, provider: intent.provider },
          { closeModelPicker: true },
        );
        return;
      case 'setThinking':
        await this.updateSession(intent.sessionId, { thinking: intent.enabled });
        return;
      case 'setEffort':
        await this.updateSession(intent.sessionId, { reasoning_effort: intent.effort });
        return;
      case 'setYolo':
        await this.updateSession(intent.sessionId, { yolo: intent.enabled });
        return;
      case 'runPromptCommand':
        await this.runPromptCommand(intent.sessionId, intent.name, intent.argsText);
        return;
      case 'openModelPicker':
        await this.openModelPicker(intent.sessionId);
        return;
      case 'closeOverlays':
        this.closeOverlays();
        return;
      case 'openLink':
        await this.deps.editor.openLink(intent.href);
        return;
      case 'openFile':
        await this.deps.editor.openFile(intent.path, intent.line);
        return;
      case 'openDiff': {
        const cell = this.record(intent.sessionId)?.cellById(intent.cellId);
        if (cell === undefined || cell.kind !== 'diff') {
          this.toast('warning', 'Nothing to open — the diff is no longer in the transcript.');
          return;
        }
        await this.deps.editor.openDiff(cell);
        return;
      }
      case 'copyText':
        await this.deps.editor.copyText(intent.text);
        return;
      default:
        unhandledVariant(intent, 'SessionManager.dispatchIntent');
    }
  }

  // ── session lifecycle ───────────────────────────────────────────────

  /** `+` / `/new` / the first session of a fresh window. */
  async newSession(): Promise<SessionId | null> {
    return this.structure.run(async () => {
      const http = this.deps.http();
      if (http === null) {
        this.toast('warning', 'Not connected — the gateway is not available.');
        return null;
      }
      const workspace = this.deps.workspaceFolder();
      if (workspace === null) {
        this.deps.reportError('Wing: open a folder before starting a session.');
        return null;
      }
      const response = await http.createSession({ workspace });
      const record = new SessionRecord({
        sessionId: response.session_id,
        now: this.deps.now,
        workspace: response.workspace ?? workspace,
        createdAt: '',
      });
      const managed = this.adopt(record, { activate: true });
      await this.subscribe(managed);
      return record.sessionId;
    });
  }

  /**
   * Focus a tab — or open the session when it is only in the history list.
   *
   * `activateSession` for an unknown id is the `/ss` picker's path (`resume →
   * subscribe`); a tab that is already open is just focused, without touching
   * the gateway.
   */
  async activate(sessionId: SessionId): Promise<void> {
    const managed = this.sessions.get(sessionId);
    if (managed !== undefined) {
      this.active = sessionId;
      managed.record.setAttention('none');
      managed.record.clearLastError();
      this.postTabs();
      this.postState(managed.record);
      this.postUi({ kind: 'focusComposer' });
      return;
    }
    await this.resumeInto(sessionId);
  }

  /** `resume → subscribe`: open a persisted session in its own tab. */
  async resumeInto(sessionId: SessionId): Promise<SessionId | null> {
    return this.structure.run(async () => {
      const existing = this.sessions.get(sessionId);
      if (existing !== undefined) {
        await this.activate(sessionId);
        return sessionId;
      }
      const http = this.deps.http();
      if (http === null) {
        this.toast('warning', 'Not connected — the gateway is not available.');
        return null;
      }
      let response;
      try {
        response = await http.resumeSession(sessionId);
      } catch (error) {
        this.reportFailure('Resume failed', error);
        return null;
      }
      const record = new SessionRecord({
        sessionId: response.session_id,
        now: this.deps.now,
        workspace: response.workspace,
        createdAt: '',
      });
      const managed = this.adopt(record, { activate: true });
      await this.subscribe(managed);
      return record.sessionId;
    });
  }

  /** `/fork <uuid>` — a new tab, subscribed to its own new session. */
  async fork(sourceSessionId: SessionId, targetUuid: string): Promise<SessionId | null> {
    return this.structure.run(async () => {
      const http = this.deps.http();
      const source = this.sessions.get(sourceSessionId);
      if (http === null || source === undefined) {
        this.toast('warning', 'Fork failed — the session is not available.');
        return null;
      }
      const response = await http.forkSession(sourceSessionId, targetUuid);
      const record = new SessionRecord({
        sessionId: response.session_id,
        now: this.deps.now,
        workspace: source.record.meta.workspace === '' ? null : source.record.meta.workspace,
        createdAt: '',
      });
      record.draft = response.draft;
      record.dirtyState = true;
      const managed = this.adopt(record, { activate: true });
      await this.subscribe(managed);
      if (record.draft !== null) {
        this.postState(record);
      }
      return record.sessionId;
    });
  }

  /** `/rewind <uuid>` — the gateway answers with a `sync_session` replacement. */
  async rewind(sessionId: SessionId, targetUuid: string): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      this.toast('warning', 'Rewind failed — the session is not available.');
      return;
    }
    try {
      await http.rewindSession(sessionId, targetUuid);
    } catch (error) {
      this.reportFailure('Rewind failed', error);
    }
  }

  /** Close a tab: unsubscribe, forget the view. The session itself survives. */
  async closeSession(sessionId: SessionId): Promise<void> {
    await this.structure.run(async () => {
      const managed = this.sessions.get(sessionId);
      if (managed === undefined) {
        return;
      }
      managed.closed = true;
      managed.subscribed = false;
      this.cancelRetry(managed);
      this.sessions.delete(sessionId);
      this.order = this.order.filter((id) => id !== sessionId);
      if (this.active === sessionId) {
        this.active = this.order[this.order.length - 1] ?? null;
      }
      this.postTabs();
      const http = this.deps.http();
      const clientId = this.deps.connection()?.currentClientId ?? null;
      if (http !== null && clientId !== null) {
        try {
          await http.unsubscribe(sessionId, clientId);
        } catch (error) {
          // The session is gone from this window either way; the gateway route
          // will die with the connection if the call never lands.
          this.deps.logger.warn(`unsubscribe failed for ${sessionId}`, error);
        }
      }
    });
  }

  /** `/compact [instruction]`. */
  async compact(sessionId: SessionId, instruction: string | null): Promise<void> {
    const http = this.deps.http();
    if (http === null) {
      this.toast('warning', 'Not connected — the gateway is not available.');
      return;
    }
    this.toast('info', 'Compacting context…');
    try {
      const response = await http.compactSession(sessionId, instruction);
      this.toast(
        'info',
        `Context compacted: ${response.original_tokens} → ${response.compressed_tokens} tokens`,
      );
    } catch (error) {
      this.reportFailure('Compact failed', error);
    }
  }

  private async updateSession(
    sessionId: SessionId,
    fields: {
      model?: string;
      provider?: string;
      thinking?: boolean;
      reasoning_effort?: string;
      yolo?: boolean;
      title?: string;
    },
    options: { readonly closeModelPicker?: boolean } = {},
  ): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      this.toast('warning', 'Update failed — the session is not available.');
      return;
    }
    try {
      await http.updateSession({ session_id: sessionId, ...fields });
    } catch (error) {
      this.reportFailure('Update failed', error);
      return;
    }
    if (options.closeModelPicker === true && managed.record.panels.modelPicker !== null) {
      managed.record.panels = { ...managed.record.panels, modelPicker: null };
      managed.record.dirtyPanels = true;
      this.postPanels(managed.record);
    }
  }

  /** `/ss` — the session-history picker. */
  async openSessionsPanel(sessionId: SessionId): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      return;
    }
    try {
      const response = await http.listSessions();
      const rows = response.sessions.map((info) => ({
        sessionId: info.id,
        title: info.name !== null && info.name !== '' ? info.name : '(untitled)',
        workspace: info.workspace,
        lastInteraction: info.last_interaction,
        status: historyStatus(info.status),
      }));
      const index = rows.findIndex((row) => row.sessionId === sessionId);
      managed.record.panels = {
        ...managed.record.panels,
        sessions: { rows, activeIndex: index === -1 ? null : index },
      };
      managed.record.dirtyPanels = true;
      this.postPanels(managed.record);
    } catch (error) {
      this.reportFailure('Could not list sessions', error);
    }
  }

  /** `/rewind` / `/fork` without an argument — the branch picker. */
  async openBranchesPanel(sessionId: SessionId, mode: 'rewind' | 'fork'): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      return;
    }
    try {
      const response = await http.sessionBranches(sessionId);
      const rows = response.targets.map((target) => branchRow(target));
      managed.record.panels = {
        ...managed.record.panels,
        branches: { mode, rows, activeIndex: rows.length > 0 ? 0 : null },
      };
      managed.record.dirtyPanels = true;
      this.postPanels(managed.record);
    } catch (error) {
      this.reportFailure('Could not list branches', error);
    }
  }

  /** `/model` — the model picker rows for the session's current selection. */
  async openModelPicker(sessionId: SessionId): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      return;
    }
    try {
      const response = await http.listModels();
      const model = managed.record.meta.model;
      const provider = managed.record.meta.provider;
      const rows = response.providers.flatMap((group) =>
        group.models.map((name) => ({
          provider: group.provider,
          model: name,
          selected: name === model && group.provider === provider,
        })),
      );
      managed.record.panels = {
        ...managed.record.panels,
        modelPicker: { sessionId, rows, activeIndex: null },
      };
      managed.record.dirtyPanels = true;
      this.postPanels(managed.record);
    } catch (error) {
      this.reportFailure('Could not list models', error);
    }
  }

  private closeOverlays(): void {
    const managed = this.active === null ? undefined : this.sessions.get(this.active);
    if (managed === undefined) {
      return;
    }
    managed.record.panels = {
      ...managed.record.panels,
      modelPicker: null,
      sessions: null,
      branches: null,
    };
    managed.record.dirtyPanels = true;
    this.postPanels(managed.record);
  }

  // ── messaging ───────────────────────────────────────────────────────

  /** `runPromptCommand`: local commands run here, everything else is a message. */
  private async runPromptCommand(sessionId: SessionId, name: string, argsText: string): Promise<void> {
    const command = name.startsWith('/') ? name : `/${name}`;
    const args = argsText.trim();
    switch (command) {
      case '/new':
        await this.newSession();
        return;
      case '/ss':
      case '/session':
        if (args === '') {
          await this.openSessionsPanel(sessionId);
        } else {
          await this.resumeInto(args);
        }
        return;
      case '/model':
        await this.openModelPicker(sessionId);
        return;
      case '/compact':
        await this.compact(sessionId, args === '' ? null : args);
        return;
      case '/rewind':
        if (args === '') {
          await this.openBranchesPanel(sessionId, 'rewind');
        } else {
          await this.rewind(sessionId, args);
        }
        return;
      case '/fork':
        if (args === '') {
          await this.openBranchesPanel(sessionId, 'fork');
        } else {
          await this.fork(sessionId, args);
        }
        return;
      default:
        // A gateway prompt command (`/init`, …) is just message text — the
        // backend's prompt-command expansion picks it up (TUI behavior).
        this.sendText(sessionId, `${command}${args === '' ? '' : ` ${args}`}`, {
          requireSubscribed: true,
        });
    }
  }

  /** Send one user message (or an ask answer through {@link replyAsk}). */
  private sendText(
    sessionId: SessionId,
    text: string,
    options: { readonly requireSubscribed: boolean },
  ): void {
    const managed = this.sessions.get(sessionId);
    if (managed === undefined) {
      this.toast('warning', 'That session is no longer open.');
      return;
    }
    if (options.requireSubscribed && this.resolveLocalCommand(managed, text)) {
      return;
    }
    const connection = this.deps.connection();
    if (
      options.requireSubscribed &&
      (!managed.subscribed || connection === null || connection.state.status !== 'connected')
    ) {
      // TUI parity: never queue. The message did not leave the client, so the
      // user gets it back in the composer instead of a phantom pending bubble.
      this.toast('warning', 'Not sent — the gateway is not connected.');
      this.postUi({ kind: 'focusComposer' });
      return;
    }
    if (connection === null) {
      this.toast('warning', 'Not sent — the gateway is not connected.');
      return;
    }

    const frame = createClientRequest({ sessionId, content: text });
    const record = managed.record;
    record.addPendingUser(frame.request_id, text);
    record.refreshTitle();
    record.dirtyTabs = true;
    // The optimistic cell must reach the webview before the frame leaves, so a
    // send failure can retract exactly what the user saw.
    this.flush(record);
    try {
      connection.send(frame);
    } catch (error) {
      record.removePending(frame.request_id);
      record.refreshTitle();
      record.dirtyTabs = true;
      this.flush(record);
      this.reportFailure('Send failed', error);
    }
  }

  /**
   * Local commands (TUI's frontend command table) — handled without a round trip.
   *
   * Returns `true` when the text was consumed. `/rewind`, `/fork`, `/ss` and
   * friends are also reachable through `runPromptCommand`; this check exists so
   * typing them into the composer works exactly like it does in the TUI.
   */
  private resolveLocalCommand(managed: ManagedSession, text: string): boolean {
    const trimmed = text.trim();
    if (!trimmed.startsWith('/')) {
      return false;
    }
    const [head, ...rest] = trimmed.split(' ');
    const name = head ?? '';
    const args = rest.join(' ').trim();
    switch (name) {
      case '/new':
        void this.newSession();
        return true;
      case '/ss':
      case '/session':
        if (args === '') {
          void this.openSessionsPanel(managed.record.sessionId);
        } else {
          void this.resumeInto(args);
        }
        return true;
      case '/model':
        void this.openModelPicker(managed.record.sessionId);
        return true;
      case '/compact':
        void this.compact(managed.record.sessionId, args === '' ? null : args);
        return true;
      case '/rewind':
        if (args === '') {
          void this.openBranchesPanel(managed.record.sessionId, 'rewind');
        } else {
          void this.rewind(managed.record.sessionId, args);
        }
        return true;
      case '/fork':
        if (args === '') {
          void this.openBranchesPanel(managed.record.sessionId, 'fork');
        } else {
          void this.fork(managed.record.sessionId, args);
        }
        return true;
      default:
        return false;
    }
  }

  private async interrupt(sessionId: SessionId): Promise<void> {
    const http = this.deps.http();
    if (http === null) {
      this.toast('warning', 'Not connected — the gateway is not available.');
      return;
    }
    try {
      await http.interruptSession(sessionId);
    } catch (error) {
      this.reportFailure('Interrupt failed', error);
    }
  }

  private answerAsk(sessionId: SessionId, requestId: string, answers: readonly AskAnswerModel[]): void {
    const managed = this.sessions.get(sessionId);
    const cellId = managed?.record.awaitingAsks.get(requestId);
    const cell = cellId === undefined ? undefined : managed?.record.cellById(cellId);
    if (managed === undefined || cell === undefined || cell.kind !== 'ask' || cell.state !== 'awaiting') {
      this.toast('warning', 'That question is no longer waiting for an answer.');
      return;
    }
    const content = buildAskReply({ approval: cell.approval, questions: cell.questions }, answers);
    this.replyAsk(managed, cell, content, answers);
  }

  private approveTool(sessionId: SessionId, requestId: string, decision: 'approve' | 'deny'): void {
    const managed = this.sessions.get(sessionId);
    const cellId = managed?.record.awaitingAsks.get(requestId);
    const cell = cellId === undefined ? undefined : managed?.record.cellById(cellId);
    if (managed === undefined || cell === undefined || cell.kind !== 'ask' || cell.state !== 'awaiting') {
      this.toast('warning', 'That approval is no longer pending.');
      return;
    }
    // The backend's `_parse_feedback` accepts exactly `y` / `n` / `yolo`.
    const label = decision === 'approve' ? 'y' : 'n';
    const answers = [{ questionId: cell.questions[0]?.id ?? 'choice', selected: [label], text: '' }];
    this.replyAsk(managed, cell, label, answers);
  }

  private replyAsk(
    managed: ManagedSession,
    cell: AskCellModel,
    content: string,
    answers: readonly AskAnswerModel[],
  ): void {
    const connection = this.deps.connection();
    if (connection === null || connection.state.status !== 'connected') {
      this.toast('warning', 'Not sent — the gateway is not connected.');
      return;
    }
    if (content === '') {
      this.toast('warning', 'Nothing to send — pick an answer first.');
      return;
    }
    const frame = createClientRequest({
      sessionId: managed.record.sessionId,
      content,
      toolCallId: cell.requestId,
    });
    try {
      connection.send(frame);
    } catch (error) {
      this.reportFailure('Send failed', error);
      return;
    }
    managed.record.resolveAsk(cell.requestId);
    managed.record.update({ ...cell, state: 'answered', answers: [...answers] });
    managed.record.refreshStatus();
    this.flush(managed.record);
  }

  // ── subscription plumbing ───────────────────────────────────────────

  /** Subscribe every open session (after a reconnect, the client id changed). */
  private async resubscribeAll(): Promise<void> {
    for (const managed of this.all()) {
      if (managed.closed) {
        continue;
      }
      await this.subscribe(managed);
    }
  }

  /**
   * Attach this client to a session's event route.
   *
   * A 404 means the gateway no longer has the session loaded (a restarted
   * gateway loses in-memory sessions, they are persisted on disk): `resume`
   * first, then subscribe again. Any other failure is transient — a per-session
   * backoff retry (cancelled on close/dispose) keeps the tab honest.
   */
  private async subscribe(managed: ManagedSession): Promise<void> {
    const http = this.deps.http();
    const clientId = this.deps.connection()?.currentClientId ?? null;
    if (managed.closed || managed.gone || this.disposed) {
      return;
    }
    if (http === null || clientId === null) {
      return;
    }
    try {
      await http.subscribe(managed.record.sessionId, clientId);
      if (managed.closed) {
        // The tab was closed while the call was in flight — undo the route.
        try {
          await http.unsubscribe(managed.record.sessionId, clientId);
        } catch (error) {
          this.deps.logger.debug(`unsubscribe after raced close failed`, error);
        }
        return;
      }
      managed.subscribed = true;
      managed.retryAttempt = 0;
    } catch (error) {
      if (error instanceof GatewayHttpError && error.isNotFound()) {
        try {
          await http.resumeSession(managed.record.sessionId);
          await http.subscribe(managed.record.sessionId, clientId);
          managed.subscribed = true;
          managed.retryAttempt = 0;
          return;
        } catch (resumeError) {
          this.markGone(managed, resumeError);
          return;
        }
      }
      this.scheduleResubscribe(managed, error);
    }
  }

  private scheduleResubscribe(managed: ManagedSession, error: unknown): void {
    if (managed.closed || managed.gone || this.disposed) {
      return;
    }
    const delay = Math.min(1_000 * 2 ** Math.min(managed.retryAttempt, 5), 30_000);
    managed.retryAttempt += 1;
    this.deps.logger.warn(`subscribe failed for ${managed.record.sessionId}; retrying in ${delay} ms`, error);
    this.cancelRetry(managed);
    managed.retryTimer = setTimeout(() => {
      managed.retryTimer = null;
      void this.structure.run(async () => {
        await this.subscribe(managed);
      });
    }, delay);
  }

  private cancelRetry(managed: ManagedSession): void {
    if (managed.retryTimer !== null) {
      clearTimeout(managed.retryTimer);
      managed.retryTimer = null;
    }
  }

  /** The session vanished from the gateway (resume 404) — say so, don't retry. */
  private markGone(managed: ManagedSession, error: unknown): void {
    managed.gone = true;
    managed.subscribed = false;
    this.deps.logger.warn(`session ${managed.record.sessionId} is gone from the gateway`, error);
    managed.record.pushCell({
      kind: 'system',
      id: managed.record.newCellId(),
      createdAt: managed.record.now(),
      level: 'error',
      text: 'This session no longer exists on the gateway. Close the tab or start a new session.',
    });
    this.flush(managed.record);
    this.toast('error', 'Session not found on the gateway.');
  }

  // ── bridge producers ────────────────────────────────────────────────

  /** Ship whatever the reduction produced (patch / hydrate / state / panels / tabs). */
  private flush(record: SessionRecord): void {
    if (record.replaced) {
      record.takeJournal();
      record.replaced = false;
      this.postHydrate(record);
      record.dirtyState = false;
      record.dirtyPanels = false;
      if (record.dirtyTabs) {
        record.dirtyTabs = false;
        this.postTabs();
      }
      return;
    }
    this.postPatch(record);
    if (record.dirtyState) {
      record.dirtyState = false;
      this.postState(record);
    }
    if (record.dirtyPanels) {
      record.dirtyPanels = false;
      this.postPanels(record);
    }
    if (record.dirtyTabs) {
      record.dirtyTabs = false;
      this.postTabs();
    }
  }

  private postPatch(record: SessionRecord): void {
    this.postPatchOps(record, record.takeJournal());
  }

  /** Ship one patch batch with the cursor's next sequence number. */
  private postPatchOps(record: SessionRecord, ops: readonly CellPatch[]): void {
    if (ops.length === 0) {
      return;
    }
    const sink = this.deps.sink();
    if (sink === null) {
      // No webview attached: the model keeps the truth, and the next hydrate
      // resets the cursor — dropping the ops is lossless.
      return;
    }
    record.seq += 1;
    sink.post({
      type: 'patch',
      sessionId: record.sessionId,
      seq: record.seq,
      patches: ops.flatMap((op) => sliceTextOp(op)),
    });
  }

  private postHydrate(record: SessionRecord): void {
    const sink = this.deps.sink();
    if (sink === null) {
      return;
    }
    sink.post({ type: 'hydrate', session: record.viewModel() });
    this.consumeDraft(record);
  }

  private postState(record: SessionRecord): void {
    const sink = this.deps.sink();
    if (sink === null) {
      return;
    }
    sink.post({ type: 'state', state: record.stateModel() });
    this.consumeDraft(record);
  }

  /**
   * The draft is a one-shot restore: once the webview has been told about it, a
   * later `state` must carry `null` so it can never overwrite typing.
   */
  private consumeDraft(record: SessionRecord): void {
    if (record.draft !== null) {
      record.draft = null;
    }
  }

  private postPanels(record: SessionRecord): void {
    const sink = this.deps.sink();
    if (sink === null) {
      return;
    }
    sink.post({ type: 'panels', sessionId: record.sessionId, panels: record.panels });
  }

  private postTabs(): void {
    const sink = this.deps.sink();
    if (sink === null) {
      return;
    }
    sink.post({
      type: 'tabs',
      tabs: this.order.flatMap((sessionId) => {
        const managed = this.sessions.get(sessionId);
        if (managed === undefined) {
          return [];
        }
        const { record } = managed;
        return [
          {
            sessionId: record.sessionId,
            title: record.title,
            status: record.status,
            attention: record.attention,
          },
        ];
      }),
      activeSessionId: this.active,
    });
  }

  private postUi(action: UiActionModel): void {
    const sink = this.deps.sink();
    if (sink === null) {
      return;
    }
    sink.post({ type: 'ui', action });
  }

  private toast(level: 'info' | 'warning' | 'error', message: string): void {
    this.postUi({ kind: 'toast', level, message });
  }

  // ── helpers ─────────────────────────────────────────────────────────

  /** Install a record as a tab (activation + hydration + tab list). */
  private adopt(record: SessionRecord, options: { readonly activate: boolean }): ManagedSession {
    const managed: ManagedSession = {
      record,
      subscribed: false,
      closed: false,
      gone: false,
      retryTimer: null,
      retryAttempt: 0,
    };
    this.sessions.set(record.sessionId, managed);
    this.order.push(record.sessionId);
    if (this.commandsCatalog !== null) {
      record.panels = { ...record.panels, commands: this.commandsCatalog };
    }
    if (this.globalNotice !== null) {
      record.panels = { ...record.panels, globalNotice: this.globalNotice };
    }
    if (options.activate || this.active === null) {
      this.active = record.sessionId;
    }
    // Order matters for the first paint: the snapshot lands before the tab bar
    // points at it, so the renderer never has to render "active tab, no data".
    this.postHydrate(record);
    this.postTabs();
    // `hydrate` already carries the panels, but the dedicated channel is what
    // 05 subscribes to for overlay updates — keep both views of the data.
    this.postPanels(record);
    if (options.activate) {
      this.postUi({ kind: 'focusComposer' });
    }
    return managed;
  }

  private async maybeCreateInitialSession(): Promise<void> {
    if (this.initialSessionRequested || this.disposed) {
      return;
    }
    if (this.sessions.size > 0) {
      this.initialSessionRequested = true;
      return;
    }
    if (!this.webviewReady) {
      return;
    }
    if (this.deps.http() === null) {
      return;
    }
    if (this.deps.workspaceFolder() === null) {
      // No folder to work in: the webview shows its empty state; the `+`
      // button surfaces a clear error when pressed.
      return;
    }
    this.initialSessionRequested = true;
    try {
      await this.newSession();
    } catch (error) {
      this.reportFailure('Could not start a session', error);
    }
  }

  private async ensureCommandsCatalog(): Promise<void> {
    if (this.commandsCatalog !== null) {
      return;
    }
    const http = this.deps.http();
    if (http === null) {
      return;
    }
    try {
      const response = await http.listCommands();
      this.commandsCatalog = response.commands.map((command) => ({
        name: command.name,
        aliases: [...command.aliases],
        description: command.description,
        params: command.params,
      }));
      for (const managed of this.all()) {
        managed.record.panels = { ...managed.record.panels, commands: this.commandsCatalog };
        managed.record.dirtyPanels = true;
        this.postPanels(managed.record);
      }
    } catch (error) {
      this.deps.logger.warn('could not fetch the prompt-command catalog', error);
    }
  }

  private all(): ManagedSession[] {
    return this.order.flatMap((sessionId) => {
      const managed = this.sessions.get(sessionId);
      return managed === undefined ? [] : [managed];
    });
  }

  private reportFailure(prefix: string, error: unknown): void {
    const detail = describeError(error);
    this.deps.logger.warn(`${prefix}: ${detail}`, error);
    this.toast('error', `${prefix}: ${detail}`);
  }

  dispose(): void {
    this.disposed = true;
    for (const managed of this.all()) {
      managed.closed = true;
      this.cancelRetry(managed);
    }
  }
}

/** Split an oversized `append_text` op so one postMessage stays small. */
function sliceTextOp(op: CellPatch): CellPatch[] {
  if (op.op !== 'append_text' || op.text.length <= TEXT_CHUNK) {
    return [op];
  }
  const parts: CellPatch[] = [];
  for (let index = 0; index < op.text.length; index += TEXT_CHUNK) {
    parts.push({ op: 'append_text', cellId: op.cellId, text: op.text.slice(index, index + TEXT_CHUNK) });
  }
  return parts;
}

/** Gateway session status → the shared history vocabulary (same word, one name). */
function historyStatus(status: SessionStatus): 'idle' | 'working' | 'waiting-for-input' | 'inactive' {
  switch (status) {
    case 'idle':
      return 'idle';
    case 'working':
      return 'working';
    case 'waiting':
      return 'waiting-for-input';
    case 'inactive':
      return 'inactive';
  }
}

/** Human-readable failure detail (`GatewayHttpError` carries the backend's message). */
function describeError(error: unknown): string {
  if (error instanceof GatewayHttpError) {
    return error.message;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}
