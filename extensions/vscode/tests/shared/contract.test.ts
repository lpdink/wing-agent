import { describe, expect, it, vi } from 'vitest';

import type {
  CellModel,
  CellPatch,
  HostToWebviewMessage,
  PanelsModel,
  SessionViewModel,
  WebviewToHostMessage,
} from '../../src/shared';
import {
  BRANCH_CURRENT_UUID,
  BRIDGE_PROTOCOL_VERSION,
  EMPTY_PANELS,
  LOCAL_COMMANDS,
  SESSION_TITLE_MAX_LENGTH,
  TOOL_NAMES,
  assertNever,
  isHostToWebviewMessage,
  isWebviewToHostMessage,
  unhandledVariant,
} from '../../src/shared';
import {
  makeAllCells,
  makeApprovalAskCell,
  makeEmptySession,
  makeFixtureSession,
} from '../../src/testing/fixtures';

/**
 * Contract tests for `src/shared`.
 *
 * Two things are checked here and nowhere else:
 * 1. the discriminated unions are *exhaustively* handled — the `assertNever`
 *    switches below stop compiling the moment a variant is added without an
 *    update, which is the only way 03/04/05 can grow the contract safely;
 * 2. every model value survives a JSON round-trip (the bridge serializes through
 *    `postMessage`, so a `Date`/`Map`/`undefined` would silently become something
 *    else).
 */

/** Compile-time exhaustiveness: every cell kind must be listed exactly once. */
function describeCell(cell: CellModel): string {
  switch (cell.kind) {
    case 'user':
      return `user:${cell.state}`;
    case 'assistant':
      return `assistant:${cell.streaming ? 'streaming' : 'done'}`;
    case 'thinking':
      return `thinking:${cell.durationMs ?? 'n/a'}`;
    case 'system':
      return `system:${cell.level}`;
    case 'tool_call':
      return `tool_call:${cell.status}:${cell.display.title}`;
    case 'diff':
      return `diff:${cell.path}:+${cell.added}/-${cell.removed}`;
    case 'todo':
      return `todo:${cell.items.length}`;
    case 'ask':
      return `ask:${cell.state}:${cell.requestId}`;
    case 'metrics':
      return `metrics:${cell.usage.promptTokens}`;
    case 'separator':
      return `separator:${cell.label}`;
    default:
      return assertNever(cell, 'describeCell');
  }
}

/** Compile-time exhaustiveness: every patch op must be listed exactly once. */
function describePatch(patch: CellPatch): string {
  switch (patch.op) {
    case 'append':
      return `append:${patch.cell.id}`;
    case 'insert_after':
      return `insert_after:${patch.afterCellId}:${patch.cell.id}`;
    case 'update':
      return `update:${patch.cell.id}`;
    case 'append_text':
      return `append_text:${patch.cellId}:${patch.text.length}`;
    case 'remove':
      return `remove:${patch.cellId}`;
    case 'replace_all':
      return `replace_all:${patch.cells.length}`;
    default:
      return assertNever(patch, 'describePatch');
  }
}

/** Compile-time exhaustiveness: every host → webview message must be listed. */
function describeHostMessage(message: HostToWebviewMessage): string {
  switch (message.type) {
    case 'hydrate':
      return `hydrate:${message.session.sessionId}:${message.session.cells.length}`;
    case 'patch':
      return `patch:${message.seq}:${message.patches.length}`;
    case 'state':
      return `state:${message.state.status}`;
    case 'panels':
      return `panels:${message.sessionId}`;
    case 'tabs':
      return `tabs:${message.tabs.length}:${String(message.activeSessionId)}`;
    case 'ui':
      return `ui:${message.action.kind}`;
    case 'pong':
      return `pong:${message.id}`;
    default:
      return assertNever(message, 'describeHostMessage');
  }
}

/** Compile-time exhaustiveness: every webview → host message must be listed. */
function describeWebviewMessage(message: WebviewToHostMessage): string {
  switch (message.type) {
    case 'ready':
      return `ready:${message.protocolVersion}`;
    case 'resync':
      return `resync:${message.sessionId}:${message.reason}`;
    case 'ping':
      return `ping:${message.id}`;
    case 'sendMessage':
      return `send:${message.text}`;
    case 'interrupt':
      return `interrupt:${message.sessionId}`;
    case 'answerAsk':
      return `answerAsk:${message.requestId}:${message.answers.length}`;
    case 'approveTool':
      return `approveTool:${message.requestId}:${message.decision}`;
    case 'newSession':
      return 'newSession';
    case 'closeSession':
      return `closeSession:${message.sessionId}`;
    case 'activateSession':
      return `activateSession:${message.sessionId}`;
    case 'compact':
      return `compact:${message.sessionId}`;
    case 'setModel':
      return `setModel:${message.provider}/${message.model}`;
    case 'setThinking':
      return `setThinking:${String(message.enabled)}`;
    case 'setEffort':
      return `setEffort:${message.effort}`;
    case 'setYolo':
      return `setYolo:${String(message.enabled)}`;
    case 'runPromptCommand':
      return `runPromptCommand:${message.name}`;
    case 'openModelPicker':
      return `openModelPicker:${message.sessionId}`;
    case 'closeOverlays':
      return 'closeOverlays';
    case 'openLink':
      return `openLink:${message.href}`;
    case 'openFile':
      return `openFile:${message.path}:${String(message.line)}`;
    case 'openDiff':
      return `openDiff:${message.cellId}`;
    case 'copyText':
      return `copyText:${message.text.length}`;
    default:
      return assertNever(message, 'describeWebviewMessage');
  }
}

/** JSON round-trip, exactly like the bridge transport does. */
function roundTrip<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

/**
 * One message per variant per direction.
 *
 * Kept at module scope so both the exhaustiveness switch *and* the runtime guard
 * assertions below exercise exactly the same values — a variant added to a union
 * forces this array to grow (the `assertNever` switches fail to compile
 * otherwise), and the guards are then checked against it automatically.
 */
const FIXTURE_SESSION = makeFixtureSession();

const HOST_MESSAGES: readonly HostToWebviewMessage[] = [
  { type: 'hydrate', session: FIXTURE_SESSION },
  { type: 'patch', sessionId: FIXTURE_SESSION.sessionId, seq: 1, patches: [{ op: 'remove', cellId: 'c1' }] },
  { type: 'state', state: FIXTURE_SESSION },
  { type: 'panels', sessionId: FIXTURE_SESSION.sessionId, panels: EMPTY_PANELS },
  { type: 'tabs', tabs: [], activeSessionId: null },
  { type: 'ui', action: { kind: 'scrollToBottom' } },
  { type: 'pong', id: 'ping-1', hostTimeMs: 0 },
];

const WEBVIEW_MESSAGES: readonly WebviewToHostMessage[] = [
  { type: 'ready', protocolVersion: BRIDGE_PROTOCOL_VERSION },
  { type: 'resync', sessionId: 's', lastSeq: 3, reason: 'seq-gap' },
  { type: 'ping', id: 'ping-1' },
  { type: 'sendMessage', sessionId: 's', text: 'hi' },
  { type: 'interrupt', sessionId: 's' },
  { type: 'answerAsk', sessionId: 's', requestId: 'r', answers: [] },
  { type: 'approveTool', sessionId: 's', requestId: 'r', decision: 'approve' },
  { type: 'newSession' },
  { type: 'closeSession', sessionId: 's' },
  { type: 'activateSession', sessionId: 's' },
  { type: 'compact', sessionId: 's' },
  { type: 'setModel', sessionId: 's', provider: 'p', model: 'm' },
  { type: 'setThinking', sessionId: 's', enabled: true },
  { type: 'setEffort', sessionId: 's', effort: 'high' },
  { type: 'setYolo', sessionId: 's', enabled: false },
  { type: 'runPromptCommand', sessionId: 's', name: '/init', argsText: '' },
  { type: 'openModelPicker', sessionId: 's' },
  { type: 'closeOverlays' },
  { type: 'openLink', href: 'https://example.com' },
  { type: 'openFile', path: '/tmp/a.ts', line: 3 },
  { type: 'openDiff', sessionId: 's', cellId: 'c1' },
  { type: 'copyText', text: 'x' },
];

describe('shared contract', () => {
  it('exposes a positive protocol version', () => {
    expect(BRIDGE_PROTOCOL_VERSION).toBeGreaterThan(0);
  });

  it('keeps constants in sync with the backend vocabulary', () => {
    // Pinned so a rename in `libs/core/wing/tools/*` or
    // `crates/wing/src/shared/constants.rs` shows up as a failing test here.
    expect(TOOL_NAMES.bash).toBe('Bash');
    expect(TOOL_NAMES.askUserQuestion).toBe('AskUserQuestion');
    expect(TOOL_NAMES.todoWrite).toBe('TodoWrite');
    expect(LOCAL_COMMANDS.new).toBe('/new');
    expect(SESSION_TITLE_MAX_LENGTH).toBe(100);
  });

  it('describes every cell kind exhaustively', () => {
    const cells = makeAllCells();
    const kinds = cells.map((cell) => cell.kind);
    expect(new Set(kinds).size).toBe(kinds.length);
    for (const cell of cells) {
      expect(describeCell(cell)).toBeTypeOf('string');
    }
    expect(describeCell(makeApprovalAskCell())).toBe('ask:awaiting:approval-1');
  });

  it('keeps cell ids unique inside a fixture transcript', () => {
    const ids = makeAllCells().map((cell) => cell.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('describes every patch op exhaustively', () => {
    const cells = makeAllCells();
    const first = cells[0];
    expect(first).toBeDefined();
    const patches: CellPatch[] = [
      { op: 'append', cell: makeApprovalAskCell() },
      { op: 'insert_after', afterCellId: 'c1', cell: makeApprovalAskCell() },
      { op: 'update', cell: makeApprovalAskCell() },
      { op: 'append_text', cellId: 'c1', text: 'abc' },
      { op: 'remove', cellId: 'c1' },
      { op: 'replace_all', cells },
    ];
    expect(patches.map(describePatch)).toEqual([
      'append:ask-approval',
      'insert_after:c1:ask-approval',
      'update:ask-approval',
      'append_text:c1:3',
      'remove:c1',
      `replace_all:${cells.length}`,
    ]);
  });

  it('describes every bridge message exhaustively', () => {
    const session = FIXTURE_SESSION;
    expect(HOST_MESSAGES.map(describeHostMessage)).toEqual([
      `hydrate:${session.sessionId}:${session.cells.length}`,
      'patch:1:1',
      'state:idle',
      `panels:${session.sessionId}`,
      'tabs:0:null',
      'ui:scrollToBottom',
      'pong:ping-1',
    ]);

    expect(WEBVIEW_MESSAGES.map(describeWebviewMessage)).toEqual([
      `ready:${BRIDGE_PROTOCOL_VERSION}`,
      'resync:s:seq-gap',
      'ping:ping-1',
      'send:hi',
      'interrupt:s',
      'answerAsk:r:0',
      'approveTool:r:approve',
      'newSession',
      'closeSession:s',
      'activateSession:s',
      'compact:s',
      'setModel:p/m',
      'setThinking:true',
      'setEffort:high',
      'setYolo:false',
      'runPromptCommand:/init',
      'openModelPicker:s',
      'closeOverlays',
      'openLink:https://example.com',
      'openFile:/tmp/a.ts:3',
      'openDiff:c1',
      'copyText:1',
    ]);
  });

  it('accepts every message of both unions with the runtime guards', () => {
    // Guards are what the transport and the host trust; if a variant is missing
    // from their tag tables it would be dropped silently (review r1 [S1]).
    // (`guard` is typed `(value: unknown) => boolean` on purpose: a type predicate
    // would narrow the negation to `never` and hide the very case under test.)
    const rejected = (messages: readonly unknown[], guard: (value: unknown) => boolean): string[] =>
      messages
        .filter((message) => !guard(message))
        .map((message) => (message as { type?: unknown }).type)
        .map(String);

    expect({
      rejectedHost: rejected(HOST_MESSAGES, isHostToWebviewMessage),
      rejectedWebview: rejected(WEBVIEW_MESSAGES, isWebviewToHostMessage),
    }).toEqual({ rejectedHost: [], rejectedWebview: [] });
  });

  it('survives a JSON round-trip (no Date / Map / undefined on the wire)', () => {
    const session = makeFixtureSession();
    const roundTripped = roundTrip(session);
    expect(roundTripped).toEqual(session);
    expect(JSON.stringify(roundTripped)).toBe(JSON.stringify(session));
    // The empty session is the "new tab" shape a host sends right after create.
    expect(roundTrip(makeEmptySession())).toEqual(makeEmptySession());
  });

  it('has no undefined values anywhere in a session snapshot', () => {
    const walk = (value: unknown, path: string): void => {
      if (value === undefined) {
        throw new Error(`undefined at ${path}`);
      }
      if (Array.isArray(value)) {
        value.forEach((item, index) => walk(item, `${path}[${index}]`));
        return;
      }
      if (typeof value === 'object' && value !== null) {
        for (const [key, nested] of Object.entries(value)) {
          walk(nested, `${path}.${key}`);
        }
      }
    };
    expect(() => walk(makeFixtureSession(), 'session')).not.toThrow();
  });
});

describe('exhaustiveness helpers', () => {
  it('assertNever throws (pure logic: an unhandled variant is a bug)', () => {
    const value = { kind: 'future-cell' } as unknown as never;
    expect(() => assertNever(value, 'test context')).toThrowError(/test context: unhandled variant/);
  });

  it('unhandledVariant only warns (a live channel must survive a newer peer)', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const value = { type: 'future-message' } as unknown as never;

    expect(() => unhandledVariant(value, 'bridge controller')).not.toThrow();
    expect(warn).toHaveBeenCalledWith(expect.stringContaining('bridge controller'), value);

    warn.mockRestore();
  });
});

describe('bridge guards', () => {
  it('accepts known host messages and rejects everything else', () => {
    expect(isHostToWebviewMessage({ type: 'pong', id: 'p', hostTimeMs: 0 })).toBe(true);
    expect(isHostToWebviewMessage({ type: 'hydrate' })).toBe(true);
    expect(isHostToWebviewMessage({ type: 'unknown-future-message' })).toBe(false);
    expect(isHostToWebviewMessage('pong')).toBe(false);
    expect(isHostToWebviewMessage(null)).toBe(false);
    expect(isHostToWebviewMessage({})).toBe(false);
  });

  it('accepts known webview messages and rejects everything else', () => {
    expect(isWebviewToHostMessage({ type: 'ready', protocolVersion: 1 })).toBe(true);
    expect(isWebviewToHostMessage({ type: 'platform-message', payload: {} })).toBe(false);
    expect(isWebviewToHostMessage(42)).toBe(false);
    expect(isWebviewToHostMessage(undefined)).toBe(false);
  });
});

describe('session view model', () => {
  it('keeps filters-independent derivations coherent', () => {
    const session: SessionViewModel = makeFixtureSession();
    expect(session.title.length).toBeLessThanOrEqual(SESSION_TITLE_MAX_LENGTH);
    expect(session.cells.every((cell) => cell.id.length > 0)).toBe(true);
    expect(session.seq).toBe(0);
    expect(session.panels.modelPicker).toBeNull();
    expect(session.panels.globalNotice).toBeNull();
  });

  it('starts with nothing open and nothing fetched (step 05, interfaces.md)', () => {
    // `PanelsModel` after the cross-lane freeze: three overlays the host opens
    // (`modelPicker` / `sessionPicker` / `branchPicker`) and one data-only catalog
    // (`commandCatalog`). `null` is the "closed"/"not fetched yet" state the shell
    // must handle for all four.
    const session: SessionViewModel = makeFixtureSession();
    expect(session.panels.modelPicker).toBeNull();
    expect(session.panels.globalNotice).toBeNull();
    expect(session.panels.commandCatalog).toBeNull();
    expect(session.panels.sessionPicker).toBeNull();
    expect(session.panels.branchPicker).toBeNull();
    expect(EMPTY_PANELS.sessionPicker).toBeNull();
    expect(EMPTY_PANELS.branchPicker).toBeNull();
  });

  it('keeps populated panels JSON-round-trippable (they are plain data)', () => {
    const panels: PanelsModel = {
      ...EMPTY_PANELS,
      commandCatalog: { commands: [{ name: '/init', aliases: [], description: 'Init', params: '' }] },
      sessionPicker: {
        rows: [
          {
            sessionId: 'session-a',
            title: 'Fixture session',
            workspace: null,
            status: 'working',
            current: true,
          },
        ],
      },
      branchPicker: {
        mode: 'fork',
        rows: [
          { uuid: 'uuid-1', content: 'first user message', current: false },
          { uuid: BRANCH_CURRENT_UUID, content: '(current)', current: true },
        ],
      },
    };

    expect(JSON.parse(JSON.stringify(panels))).toEqual(panels);
  });
});
