import type {
  AskAnswerModel,
  AskCellModel,
  AssistantCellModel,
  CellModel,
  CellPatch,
  CommandCatalogModel,
  HostToWebviewMessage,
  PanelsModel,
  SessionId,
  SessionListStatus,
  SystemLevel,
  UiActionModel,
} from '../../shared';
import {
  FRONTEND_COMMANDS,
  MAX_PATCH_TEXT_CHUNK,
  matchCommand,
  normalizeCommandName,
  unhandledVariant,
} from '../../shared';
import type { CoreLogger, GatewayConnection, GatewayHttpClient, SessionStatus, WingEvent } from '../../core';
import { GatewayHttpError, createClientRequest, isKnownEvent } from '../../core';

import type { WebviewIntent } from '../bridge';
import type { EditorActions } from '../editorActions';
import { SerialQueue } from './queue';
import type { ReductionEffect } from './reducer';
import { applyLive, applySync, pushSystem } from './reducer';
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

export class SessionManager {
  private readonly sessions = new Map<SessionId, ManagedSession>();
  private order: SessionId[] = [];
  private active: SessionId | null = null;
  private webviewReady = false;
  private initialSessionRequested = false;
  private commandsCatalog: CommandCatalogModel | null = null;
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
      // The picker closes as soon as the choice has had its effect: a failed
      // subscribe must not leave a stale overlay on top of the new tab.
      this.clearPicker('sessionPicker');
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
      record.setDraft(response.draft);
      const managed = this.adopt(record, { activate: true });
      this.clearPicker('branchPicker');
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
      return;
    }
    this.clearPicker('branchPicker');
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
      agent?: string;
      workspace?: string;
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
    // Optimistic local update (TUI `runner.rs` does the same): the gateway's
    // `session_state_changed` carries no provider, so a cross-provider model
    // switch would otherwise leave `meta.provider` stale until the next sync —
    // and the /model picker's `selected` row depends on it.
    const record = managed.record;
    const meta = { ...record.meta };
    if (fields.model !== undefined) {
      meta.model = fields.model;
    }
    if (fields.provider !== undefined) {
      meta.provider = fields.provider;
    }
    if (fields.thinking !== undefined) {
      meta.thinking = fields.thinking;
    }
    if (fields.reasoning_effort !== undefined) {
      meta.reasoningEffort = fields.reasoning_effort;
    }
    if (fields.yolo !== undefined) {
      meta.yolo = fields.yolo;
    }
    if (fields.title !== undefined) {
      record.explicitTitle = fields.title;
    }
    if (fields.agent !== undefined) {
      meta.agent = fields.agent;
    }
    if (fields.workspace !== undefined) {
      meta.workspace = fields.workspace;
    }
    record.meta = meta;
    record.refreshTitle();
    record.dirtyState = true;
    record.dirtyTabs = true;
    this.flush(record);
    if (options.closeModelPicker === true && record.panels.modelPicker !== null) {
      record.panels = { ...record.panels, modelPicker: null };
      record.dirtyPanels = true;
      this.postPanels(record);
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
        status: historyStatus(info.status),
        current: info.id === sessionId,
      }));
      managed.record.panels = { ...managed.record.panels, sessionPicker: { rows } };
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
      managed.record.panels = { ...managed.record.panels, branchPicker: { mode, rows } };
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

  /**
   * Drop one host-opened picker once its action ran (no stale rows).
   *
   * Cleared on **every** session that has it open: a picker is a modal overlay,
   * and the action's source session (the tab the user typed in) is not always
   * the session the action targets (`/ss <id>` resumes another session).
   */
  private clearPicker(kind: 'sessionPicker' | 'branchPicker'): void {
    for (const managed of this.all()) {
      if (managed.record.panels[kind] === null) {
        continue;
      }
      managed.record.panels = { ...managed.record.panels, [kind]: null };
      managed.record.dirtyPanels = true;
      this.postPanels(managed.record);
    }
  }

  private closeOverlays(): void {
    const managed = this.active === null ? undefined : this.sessions.get(this.active);
    if (managed === undefined) {
      return;
    }
    // The three host-opened overlays; `globalNotice` is not one of them — it
    // expires on its own (connection state), never on user input.
    managed.record.panels = {
      ...managed.record.panels,
      modelPicker: null,
      sessionPicker: null,
      branchPicker: null,
    };
    managed.record.dirtyPanels = true;
    this.postPanels(managed.record);
  }

  // ── messaging ───────────────────────────────────────────────────────

  /** `runPromptCommand`: local commands run here, everything else is a message. */
  private async runPromptCommand(sessionId: SessionId, name: string, argsText: string): Promise<void> {
    const command = normalizeCommandName(name);
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
      // ── local (frontend-only) commands: the TUI's `app/commands.rs` table ──
      case '/context':
        await this.showContextInfo(sessionId);
        return;
      case '/skills':
        await this.showSkillsInfo(sessionId);
        return;
      case '/reload':
        await this.reloadSystem();
        return;
      case '/copy':
        await this.copyAssistantMessage(sessionId, args);
        return;
      case '/title':
        await this.setOrShowTitle(sessionId, args);
        return;
      case '/workdir':
        await this.setOrShowWorkdir(sessionId, args);
        return;
      case '/agents':
        await this.useAgent(sessionId, args);
        return;
      case '/clear':
        // The transcript is a projection of gateway state, so there is no local
        // "clear" that survives the next patch or sync; saying so is honest,
        // sending the word to the model (the old behavior) is not.
        this.toast(
          'info',
          'Clearing the chat view is not supported in the VS Code extension yet — use /new for a fresh session.',
        );
        return;
      default: {
        // A *frontend* command that reached this point has no implementation:
        // answer, never leak it to the model as a prompt. Gateway commands
        // (`/init`, …) are message text — the backend expands them (TUI parity).
        if (matchCommand(command, FRONTEND_COMMANDS) !== null) {
          this.toast('info', `${command} is not supported in the VS Code extension yet.`);
          return;
        }
        this.sendText(sessionId, `${command}${args === '' ? '' : ` ${args}`}`, {
          requireSubscribed: true,
        });
      }
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
      // text goes back into the composer instead of a phantom pending bubble —
      // the composer clears optimistically (it cannot know whether the host
      // forwarded anything), so dropping the text here would lose the user's
      // input exactly when they need it most.
      this.returnToComposer(managed.record, text);
      this.toast('warning', 'Not sent — the gateway is not connected.');
      this.postUi({ kind: 'focusComposer' });
      return;
    }
    if (connection === null) {
      this.returnToComposer(managed.record, text);
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
   * Hand text that never left the client back to the composer.
   *
   * The draft is a one-shot restore (`state.draft` + `draftSeq`), so it must be
   * installed through {@link SessionRecord.setDraft} and flushed immediately —
   * the webview has already cleared its optimistic copy by then.
   */
  private returnToComposer(record: SessionRecord, text: string): void {
    record.setDraft(text);
    this.flush(record);
  }

  /**
   * Local commands (TUI's frontend command table) — handled without a round trip.
   *
   * The table is `FRONTEND_COMMANDS` (shared, pinned against the TUI's
   * `TUI_ONLY_COMMANDS`): anything in it is consumed here, so the composer path and
   * the `runPromptCommand` path can never disagree about what is local. A gateway
   * prompt command (`/init`, …) returns `false` and is sent as a message below.
   */
  private resolveLocalCommand(managed: ManagedSession, text: string): boolean {
    const trimmed = text.trim();
    if (!trimmed.startsWith('/')) {
      return false;
    }
    const [head, ...rest] = trimmed.split(' ');
    const name = head ?? '';
    if (matchCommand(name, FRONTEND_COMMANDS) === null) {
      return false;
    }
    void this.runPromptCommand(managed.record.sessionId, name, rest.join(' ').trim());
    return true;
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

  // ── local commands (TUI `app/commands.rs` parity) ───────────────────

  /**
   * `/context` — context window usage + the system prompt.
   *
   * Same source and wording as the TUI's `show_context_info`
   * (`crates/wing/src/app/runner.rs`), rendered as a system cell instead of a
   * chat message.
   */
  private async showContextInfo(sessionId: SessionId): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      this.toast('warning', 'Context info failed — the session is not available.');
      return;
    }
    try {
      const info = await http.sessionInfo(sessionId);
      const lines = [
        `Messages: ${info.context_stats.message_count}`,
        `Tokens: ${info.context_stats.total_tokens} / ${info.context_window_tokens}`,
      ];
      if (info.system_prompt !== '') {
        lines.push('', '--- System Prompt ---', info.system_prompt);
      }
      this.pushNotice(managed.record, 'info', lines.join('\n'));
    } catch (error) {
      this.reportFailure('Context info failed', error);
    }
  }

  /** `/skills` — the loaded skills / rules summary (TUI `show_skills_info`). */
  private async showSkillsInfo(sessionId: SessionId): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      this.toast('warning', 'Skills info failed — the session is not available.');
      return;
    }
    try {
      const info = await http.sessionInfo(sessionId);
      this.pushNotice(
        managed.record,
        'info',
        info.skills_info === '' ? 'No skills loaded.' : info.skills_info,
      );
    } catch (error) {
      this.reportFailure('Skills info failed', error);
    }
  }

  /** `/reload` — hot-reload config / hooks / providers (TUI `reload_system`). */
  private async reloadSystem(): Promise<void> {
    const http = this.deps.http();
    if (http === null) {
      this.toast('warning', 'Not connected — the gateway is not available.');
      return;
    }
    try {
      const response = await http.reloadSystem();
      const head = response.ok ? '✅' : '⚠️';
      const details = response.results.map((result) =>
        result.ok ? `✅ ${result.name}` : `❌ ${result.name}: ${result.detail ?? 'unknown'}`,
      );
      this.toast('info', `${head} Reload: ${details.join(', ')}`);
    } catch (error) {
      this.reportFailure('Reload failed', error);
    }
  }

  /**
   * `/copy [N]` — copy the N-th (1-based) or the last assistant message.
   *
   * The TUI reads its own chat cells; here the authority is the host record, so
   * the text is exactly what the transcript shows.
   */
  private async copyAssistantMessage(sessionId: SessionId, args: string): Promise<void> {
    const managed = this.sessions.get(sessionId);
    if (managed === undefined) {
      return;
    }
    const texts = managed.record.cells
      .filter((cell): cell is AssistantCellModel => cell.kind === 'assistant')
      .map((cell) => cell.text);
    const index = args === '' ? texts.length : Number.parseInt(args, 10);
    const text = Number.isInteger(index) && index >= 1 ? texts[index - 1] : undefined;
    if (text === undefined || text === '') {
      this.toast('warning', 'No assistant message to copy');
      return;
    }
    await this.deps.editor.copyText(text);
    // TUI parity is "copy silently"; a webview has no visible feedback, so the
    // extension adds one toast (documented deviation).
    this.toast('info', 'Copied to clipboard');
  }

  /** `/title [name]` — set the session title, or show it (TUI `set_or_show_title`). */
  private async setOrShowTitle(sessionId: SessionId, args: string): Promise<void> {
    const managed = this.sessions.get(sessionId);
    if (managed === undefined) {
      return;
    }
    if (args === '') {
      this.toast('info', `title: ${managed.record.explicitTitle ?? '(not set)'}`);
      return;
    }
    await this.updateSession(sessionId, { title: args });
  }

  /** `/workdir [path]` — set the workspace, or show it (TUI `set_or_show_workdir`). */
  private async setOrShowWorkdir(sessionId: SessionId, args: string): Promise<void> {
    const managed = this.sessions.get(sessionId);
    if (managed === undefined) {
      return;
    }
    if (args === '') {
      const workspace = managed.record.meta.workspace;
      this.toast('info', `workdir: ${workspace === '' ? '(not set)' : workspace}`);
      return;
    }
    await this.updateSession(sessionId, { workspace: args });
  }

  /**
   * `/agents [name]` — switch the agent template, or list what is available.
   *
   * Bare `/agents` lists instead of opening a picker: the extension has no agents
   * panel (out of scope), and a toast is the honest version of "here are the
   * candidates".
   */
  private async useAgent(sessionId: SessionId, args: string): Promise<void> {
    const managed = this.sessions.get(sessionId);
    const http = this.deps.http();
    if (managed === undefined || http === null) {
      this.toast('warning', 'Agents failed — the session is not available.');
      return;
    }
    if (args !== '') {
      await this.updateSession(sessionId, { agent: args });
      return;
    }
    try {
      const response = await http.listAgents();
      if (response.agents.length === 0) {
        this.toast('info', 'No agent templates available.');
        return;
      }
      const names = response.agents.map((name) =>
        name === response.default_agent ? `${name} (default)` : name,
      );
      this.toast('info', `Agents: ${names.join(', ')}`);
    } catch (error) {
      this.reportFailure('Could not list agents', error);
    }
  }

  /** Append one host-authored system cell to a session's transcript. */
  private pushNotice(record: SessionRecord, level: SystemLevel, text: string): void {
    pushSystem(record, level, text);
    this.flush(record);
  }

  /**
   * Pull the runtime knobs the replay does not carry.
   *
   * `sync_session.agent` has model/provider/tools — but **not** `yolo`, `thinking`
   * or `reasoning_effort`. The TUI fetches `GET /api/session/info` on every
   * session switch for exactly this reason; without it a resumed session shows
   * "yolo off" while the backend has it on. Failures stay quiet: the tab works,
   * the knobs keep their last known value.
   */
  private async refreshRuntimeState(managed: ManagedSession): Promise<void> {
    const http = this.deps.http();
    const sessionId = managed.record.sessionId;
    if (http === null || managed.closed || this.disposed) {
      return;
    }
    try {
      const info = await http.sessionInfo(sessionId);
      if (managed.closed || this.disposed) {
        return;
      }
      const record = managed.record;
      record.meta = {
        ...record.meta,
        thinking: info.thinking,
        reasoningEffort: info.reasoning_effort ?? '',
        yolo: info.yolo,
        // Fill, never clobber: a model switch that landed while this response was
        // in flight is newer than the response (the gateway echoes it through
        // `session_state_changed` and the optimistic update already applied).
        model: record.meta.model === '' ? info.model : record.meta.model,
        workspace: info.workdir ?? record.meta.workspace,
      };
      record.dirtyState = true;
      this.flush(record);
    } catch (error) {
      this.deps.logger.debug(`could not refresh the runtime state of ${sessionId}`, error);
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
      // `sync_session` (which follows the attach) does not carry yolo / thinking /
      // effort — fetch the runtime state on the side (TUI parity).
      void this.refreshRuntimeState(managed);
    } catch (error) {
      if (error instanceof GatewayHttpError && error.isNotFound()) {
        try {
          await http.resumeSession(managed.record.sessionId);
          await http.subscribe(managed.record.sessionId, clientId);
          managed.subscribed = true;
          managed.retryAttempt = 0;
          void this.refreshRuntimeState(managed);
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
   * later `state` must carry `null` so it can never overwrite typing. The token
   * (`record.draftSeq`) is *not* consumed here — see `SessionStateModel.draftSeq`.
   */
  private consumeDraft(record: SessionRecord): void {
    if (record.draft !== null) {
      record.consumeDraft();
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
      record.panels = { ...record.panels, commandCatalog: this.commandsCatalog };
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
      this.commandsCatalog = {
        commands: response.commands.map((command) => ({
          name: command.name,
          aliases: [...command.aliases],
          description: command.description,
          params: command.params,
        })),
      };
      for (const managed of this.all()) {
        managed.record.panels = {
          ...managed.record.panels,
          commandCatalog: this.commandsCatalog,
        };
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

/**
 * Keep one postMessage small: any text a patch carries is sliced to
 * {@link MAX_PATCH_TEXT_CHUNK}.
 *
 * `append_text` is split into several ops; a cell *created* with a long body
 * (the first streamed delta, or a replay cell that reaches the live lane) keeps
 * its head in the `append` / `insert_after` op and delivers the rest through
 * `append_text`. Both forms reconstruct the identical text in the webview.
 *
 * `tool_call` cells are deliberately *not* covered here: their `argsText` is
 * capped at the source (`reducer.ts` `TOOL_ARGS_MAX_CHARS`) because it is a
 * streaming preview — re-chunking it would still send every byte, which is the
 * traffic the cap exists to remove (review #109 [P1-2]).
 */
function sliceTextOp(op: CellPatch): CellPatch[] {
  switch (op.op) {
    case 'append_text':
      return sliceText(op.cellId, op.text);
    case 'append':
    case 'insert_after': {
      const split = spillCellText(op.cell);
      if (split === null) {
        return [op];
      }
      const head = { ...op, cell: split.head };
      return [head, ...sliceText(split.head.id, split.tail)];
    }
    default:
      return [op];
  }
}

function sliceText(cellId: string, text: string): CellPatch[] {
  if (text.length <= MAX_PATCH_TEXT_CHUNK) {
    return [{ op: 'append_text', cellId, text }];
  }
  const parts: CellPatch[] = [];
  for (let index = 0; index < text.length; index += MAX_PATCH_TEXT_CHUNK) {
    parts.push({ op: 'append_text', cellId, text: text.slice(index, index + MAX_PATCH_TEXT_CHUNK) });
  }
  return parts;
}

/** Split a text-bearing cell whose body exceeds the chunk limit. */
function spillCellText(cell: CellModel): {
  readonly head: CellModel;
  readonly tail: string;
} | null {
  switch (cell.kind) {
    case 'user':
    case 'assistant':
    case 'thinking':
    case 'system': {
      if (cell.text.length <= MAX_PATCH_TEXT_CHUNK) {
        return null;
      }
      return {
        head: { ...cell, text: cell.text.slice(0, MAX_PATCH_TEXT_CHUNK) },
        tail: cell.text.slice(MAX_PATCH_TEXT_CHUNK),
      };
    }
    default:
      return null;
  }
}

/** Gateway session status → the shared picker vocabulary (same word, one name). */
function historyStatus(status: SessionStatus): SessionListStatus {
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
