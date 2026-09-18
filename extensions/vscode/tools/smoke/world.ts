/**
 * The smoke's "world": the **production** host wired to the **real** gateway.
 *
 * Nothing under test is stubbed here. `WingHost` is the class VS Code activates,
 * `createGatewayClients` builds the same WebSocket/HTTP clients the extension
 * ships, and the messages the host posts towards the (absent) webview are fed
 * through the same `WebviewMirror` the host tests use — i.e. through the shipped
 * patch reducer. A scenario therefore observes exactly what the webview would
 * render, plus what the gateway was asked to do.
 *
 * `vscode` is aliased to `tools/smoke/vscode-stub.ts` at bundle time; the smoke
 * provides its own settings / editor actions, so the stub is never called.
 */

import { GatewayLauncher, probeGateway } from '../../src/host/gateway/launcher';
import type { EditorActions } from '../../src/host/editorActions';
import type { GatewaySettings } from '../../src/host/settings';
import { WingHost, createGatewayClients } from '../../src/host/wingHost';
import type {
  CellModel,
  HostToWebviewMessage,
  SessionStateModel,
  SessionViewModel,
  TabModel,
} from '../../src/shared';
import type { WingEvent } from '../../src/core';
import type { WebviewIntent } from '../../src/host/bridge';
import { WebviewMirror } from '../../tests/host/support/mirror';

export interface RecordedEditor extends EditorActions {
  readonly links: string[];
  readonly files: { path: string; line: number | null }[];
  readonly copies: string[];
  readonly diffs: string[];
}

export interface SmokeWorldOptions {
  readonly port: number;
  readonly workspace: string;
  /** Where the host's warnings/errors are reported (they are progress for a human). */
  readonly report: (message: string) => void;
}

export class SmokeWorld {
  readonly posted: HostToWebviewMessage[] = [];
  readonly mirror = new WebviewMirror();
  readonly errors: string[] = [];
  readonly editor: RecordedEditor;
  readonly host: WingHost;
  /** Every gateway event the connection received (diagnostics for failures). */
  readonly events: WingEvent[] = [];
  /** Connection state transitions, in order (diagnostics). */
  readonly stateLog: string[] = [];

  private readonly settings: GatewaySettings;

  constructor(options: SmokeWorldOptions) {
    this.settings = {
      host: '127.0.0.1',
      port: options.port,
      apiKey: null,
      wingPath: null,
      // Never let the smoke start anything: the gateway it talks to was started
      // by the smoke itself, and `wing start` would talk to `~/.wing`.
      autoStart: false,
    };
    const links: string[] = [];
    const files: { path: string; line: number | null }[] = [];
    const copies: string[] = [];
    const diffs: string[] = [];
    this.editor = {
      links,
      files,
      copies,
      diffs,
      openLink: (href) => {
        links.push(href);
        return Promise.resolve();
      },
      openFile: (path, line) => {
        files.push({ path, line });
        return Promise.resolve();
      },
      openDiff: (cell) => {
        diffs.push(cell.path);
        return Promise.resolve();
      },
      copyText: (text) => {
        copies.push(text);
        return Promise.resolve();
      },
    };

    const launcher = new GatewayLauncher({ probe: probeGateway });
    const sink = {
      post: (message: HostToWebviewMessage): void => {
        this.posted.push(message);
        this.mirror.apply(message);
      },
    };
    this.host = new WingHost({
      sink: () => sink,
      settings: () => this.settings,
      gatewayFactory: {
        create: () => {
          const clients = createGatewayClients(this.settings);
          // A second, passive listener: the failure report can then show what the
          // gateway actually sent (the host's own copy is reduced away).
          clients.connection.onEvent((event) => {
            this.events.push(event);
          });
          clients.connection.onStateChange((state) => {
            this.stateLog.push(state.status);
          });
          return clients;
        },
      },
      launcher,
      workspaceFolder: () => options.workspace,
      editor: this.editor,
      reportError: (message) => {
        this.errors.push(message);
        options.report(`[host error] ${message}`);
      },
      logger: {
        debug: () => undefined,
        warn: (message, detail) => {
          options.report(`[host warn] ${message}${detail === undefined ? '' : ` — ${describe(detail)}`}`);
        },
        error: (message, detail) => {
          options.report(`[host error] ${message}${detail === undefined ? '' : ` — ${describe(detail)}`}`);
        },
      },
    });
    this.host.attachSink(sink);
  }

  /** Activation: probe → connect → the webview's `ready` handshake. */
  async start(): Promise<void> {
    await this.host.start();
    this.host.onReady(1);
    await this.waitFor(() => this.host.connectionState?.status === 'connected', {
      label: 'gateway connection',
    });
    await this.waitFor(() => this.tabs().tabs.length > 0, { label: 'the initial session' });
    // A `state` message for a session implies `subscribe` resolved *and* the
    // gateway's replay landed (applySync marks the route attached), which is the
    // precondition for sending — waiting on the tab list alone is racy.
    await this.waitFor(
      () => this.openSessionIds().every((sessionId) => this.state(sessionId) !== undefined),
      { label: 'the initial session to be subscribed' },
    );
  }

  async intent(intent: WebviewIntent): Promise<void> {
    await this.host.onIntent(intent);
  }

  async send(sessionId: string, text: string): Promise<void> {
    await this.intent({ type: 'sendMessage', sessionId, text });
  }

  // ── observations ────────────────────────────────────────────────────

  cells(sessionId: string): readonly CellModel[] {
    return this.mirror.cells(sessionId);
  }

  /** The last `hydrate` snapshot for a session (what the webview re-mounted on). */
  hydrate(sessionId: string): SessionViewModel | undefined {
    const hydrates = this.posted.filter(
      (message): message is Extract<HostToWebviewMessage, { type: 'hydrate' }> =>
        message.type === 'hydrate' && message.session.sessionId === sessionId,
    );
    return hydrates.at(-1)?.session;
  }

  /**
   * Drafts the host handed to the webview for a session, in delivery order.
   *
   * The host consumes a draft exactly once (a later `state` carries `null`), and
   * which channel carries it depends on the action: `fork` ships it inside
   * `hydrate`, `rewind` inside `state`. The assertion is therefore "some message
   * delivered it", not "the latest state has it".
   */
  drafts(sessionId: string): readonly string[] {
    const drafts: string[] = [];
    for (const message of this.posted) {
      if (message.type === 'hydrate' && message.session.sessionId === sessionId) {
        if (message.session.draft !== null) {
          drafts.push(message.session.draft);
        }
      }
      if (message.type === 'state' && message.state.sessionId === sessionId) {
        if (message.state.draft !== null) {
          drafts.push(message.state.draft);
        }
      }
    }
    return drafts;
  }

  /** Kinds of the mirrored transcript (the assertion vocabulary of most scenarios). */
  kinds(sessionId: string): readonly string[] {
    return this.cells(sessionId).map((cell) => cell.kind);
  }

  textOf(sessionId: string, kind: 'assistant' | 'thinking' | 'system'): string {
    return this.cells(sessionId)
      .filter((cell) => cell.kind === kind)
      .map((cell) => ('text' in cell ? cell.text : ''))
      .join('');
  }

  state(sessionId: string): SessionStateModel | undefined {
    const states = this.posted.filter(
      (message): message is Extract<HostToWebviewMessage, { type: 'state' }> =>
        message.type === 'state' && message.state.sessionId === sessionId,
    );
    return states.at(-1)?.state;
  }

  /**
   * Every distinct status the webview was told about, in order.
   *
   * A fast turn can start and finish between two polls, so "the user saw
   * working" is a *history* assertion, not a polling one.
   */
  statuses(sessionId: string): readonly string[] {
    const seen: string[] = [];
    for (const message of this.posted) {
      if (message.type !== 'state' || message.state.sessionId !== sessionId) {
        continue;
      }
      const status = message.state.status;
      if (seen.at(-1) !== status) {
        seen.push(status);
      }
    }
    return seen;
  }

  tabs(): Extract<HostToWebviewMessage, { type: 'tabs' }> {
    const tabs = this.posted.filter(
      (message): message is Extract<HostToWebviewMessage, { type: 'tabs' }> => message.type === 'tabs',
    );
    return tabs.at(-1) ?? { type: 'tabs', tabs: [], activeSessionId: null };
  }

  toasts(): string[] {
    return this.posted
      .filter((message): message is Extract<HostToWebviewMessage, { type: 'ui' }> => message.type === 'ui')
      .flatMap((message) => (message.action.kind === 'toast' ? [message.action.message] : []));
  }

  lastPanels(sessionId: string) {
    const panels = this.posted.filter(
      (message): message is Extract<HostToWebviewMessage, { type: 'panels' }> =>
        message.type === 'panels' && message.sessionId === sessionId,
    );
    return panels.at(-1)?.panels;
  }

  /** `session_state_changed`-shaped checks read the record (the host's authority). */
  record(sessionId: string) {
    return this.host.sessionManager.record(sessionId);
  }

  /** Message counts of every `sync_session` seen for a session (diagnostics). */
  syncPayloads(sessionId: string): readonly number[] {
    return this.events
      .filter((event) => event.type === 'sync_session' && event.session_id === sessionId)
      .map((event) => {
        const payload = event as unknown as { messages?: readonly unknown[] };
        return payload.messages?.length ?? -1;
      });
  }

  openSessionIds(): readonly string[] {
    return this.host.sessionManager.openSessionIds;
  }

  /** Wait for a world condition; polling with a deadline, never a fixed sleep. */
  async waitFor(
    predicate: () => boolean,
    options: { readonly label: string; readonly timeoutMs?: number },
  ): Promise<void> {
    const timeoutMs = options.timeoutMs ?? 15_000;
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      if (predicate()) {
        return;
      }
      if (Date.now() >= deadline) {
        throw new Error(`timed out after ${timeoutMs} ms waiting for ${options.label}\n${this.describe()}`);
      }
      await new Promise<void>((resolve) => {
        setTimeout(resolve, 15);
      });
    }
  }

  /** Compact state dump for failure messages. */
  describe(): string {
    const sessions = this.openSessionIds().map((sessionId) => {
      const record = this.record(sessionId);
      const state = this.state(sessionId);
      const transcript = this.cells(sessionId)
        .map((cell) => {
          const text = 'text' in cell ? cell.text.replace(/\s+/g, ' ').slice(0, 60) : cell.kind;
          return `      ${cell.kind}: ${text}`;
        })
        .join('\n');
      const hydrates = this.posted
        .filter(
          (message): message is Extract<HostToWebviewMessage, { type: 'hydrate' }> =>
            message.type === 'hydrate' && message.session.sessionId === sessionId,
        )
        .map((message) => message.session.cells.length)
        .join(',');
      const patches = this.posted.filter(
        (message) => message.type === 'patch' && message.sessionId === sessionId,
      ).length;
      return [
        `  ${sessionId}: status=${state?.status ?? record?.status ?? '?'} title=${JSON.stringify(
          state?.title ?? record?.title ?? '',
        )}`,
        `    cells: ${this.kinds(sessionId).join(', ') || '<none>'}`,
        `    meta: yolo=${state?.meta.yolo ?? '?'} thinking=${state?.meta.thinking ?? '?'} model=${state?.meta.model ?? '?'}`,
        `    hydrates(cells)=[${hydrates}] patches=${patches} drafts=${JSON.stringify(this.drafts(sessionId))}`,
        `    sync_session payloads: ${JSON.stringify(this.syncPayloads(sessionId))}`,
        transcript === '' ? '    (empty transcript)' : transcript,
      ].join('\n');
    });
    return [
      '--- smoke world ---',
      `  open sessions: ${this.openSessionIds().join(', ') || '<none>'}`,
      ...sessions,
      `  last events: ${this.events
        .slice(-12)
        .map((event) => `${event.type}@${String(event.session_id).slice(-6)}`)
        .join(', ')}`,
      `  connection states: ${this.stateLog.join(' → ')}`,
      `  mirror errors: ${this.mirror.errors.length === 0 ? 'none' : this.mirror.errors.join(' | ')}`,
      `  host errors: ${this.errors.length === 0 ? 'none' : this.errors.join(' | ')}`,
    ].join('\n');
  }

  dispose(): void {
    this.host.dispose();
  }
}

function describe(detail: unknown): string {
  if (detail instanceof Error) {
    return detail.message;
  }
  try {
    return JSON.stringify(detail) ?? String(detail);
  } catch {
    return String(detail);
  }
}

/** Convenience: the tab list of a world (empty when nothing was posted yet). */
export function activeTab(world: SmokeWorld): TabModel | undefined {
  const tabs = world.tabs();
  return tabs.tabs.find((tab) => tab.sessionId === tabs.activeSessionId);
}
