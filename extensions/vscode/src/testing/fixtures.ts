import type { AskCellModel, CellModel, SessionViewModel, TabModel, UserCellModel } from '../shared';
import { EMPTY_PANELS } from '../shared';

/**
 * Fixtures for tests and the preview harness.
 *
 * They live in `src/testing` (never imported by product code — enforced by the
 * layer guard) and are plain JSON on purpose: if a fixture cannot survive a
 * `postMessage`, neither can the real thing.
 */

export type CellOverrides = Partial<CellModel>;

/** Deterministic epoch base so snapshots/asserts stay stable. */
export const FIXTURE_EPOCH = Date.UTC(2026, 0, 1, 0, 0, 0);

/** One cell per kind — the renderer's completeness fixture (step 04 / 05). */
export function makeAllCells(): readonly CellModel[] {
  return [
    { kind: 'separator', id: 'sep-1', createdAt: FIXTURE_EPOCH, label: 'Turn 1' },
    {
      kind: 'user',
      id: 'user-1',
      createdAt: FIXTURE_EPOCH + 1,
      text: 'Refactor the session store.',
      state: 'accepted',
    },
    {
      kind: 'thinking',
      id: 'thinking-1',
      createdAt: FIXTURE_EPOCH + 2,
      text: 'The store mirrors the host reducer; patches are ordered by seq.',
      streaming: false,
      durationMs: 640,
    },
    {
      kind: 'assistant',
      id: 'assistant-1',
      createdAt: FIXTURE_EPOCH + 3,
      streaming: false,
      text: 'Here is the plan:\n\n1. keep cells immutable\n2. apply ops in order\n\n```ts\ntype CellPatch = { op: "append_text" | "update" };\n```',
    },
    {
      kind: 'tool_call',
      id: 'tool-1',
      createdAt: FIXTURE_EPOCH + 4,
      toolCallId: 'call-1',
      name: 'Bash',
      status: 'success',
      display: { title: 'Bash', subject: 'pnpm run test' },
      argsText: '{"command":"pnpm run test"}',
      args: { command: 'pnpm run test' },
      result: { text: 'Test Files 12 passed (12)', isError: false, truncated: false },
      startedAt: FIXTURE_EPOCH + 4,
      finishedAt: FIXTURE_EPOCH + 4 + 4210,
    },
    {
      kind: 'diff',
      id: 'diff-1',
      createdAt: FIXTURE_EPOCH + 5,
      path: 'src/webview/state/store.ts',
      oldStartLine: 10,
      newStartLine: 10,
      lines: [
        { kind: 'hunk', text: '@@ -10,3 +10,4 @@', oldLine: null, newLine: null },
        { kind: 'context', text: '  hydrate: (session) => {', oldLine: 10, newLine: 10 },
        {
          kind: 'del',
          text: '    set({ sessions: { [session.sessionId]: session } });',
          oldLine: 11,
          newLine: null,
        },
        {
          kind: 'add',
          text: '    set((state) => ({ sessions: { ...state.sessions, [session.sessionId]: session } }));',
          oldLine: null,
          newLine: 11,
        },
      ],
      added: 1,
      removed: 1,
      truncated: false,
      toolCallId: 'call-1',
    },
    {
      kind: 'todo',
      id: 'todo-1',
      createdAt: FIXTURE_EPOCH + 6,
      items: [
        { content: 'Mirror the host model', status: 'completed' },
        { content: 'Render streamed cells', status: 'in_progress' },
        { content: 'Wire the composer', status: 'pending' },
      ],
    },
    {
      kind: 'ask',
      id: 'ask-1',
      createdAt: FIXTURE_EPOCH + 7,
      requestId: 'ask-request-1',
      sessionId: 'session-a',
      approval: false,
      state: 'awaiting',
      answers: [],
      questions: [
        {
          id: 'q1',
          question: 'Which rendering strategy should the transcript use?',
          header: 'Rendering',
          multiSelect: false,
          required: true,
          options: [
            { label: 'Memoized cells', description: 'Stable prefix + streaming tail' },
            { label: 'Full re-render', description: 'Simplest, quadratic on long turns' },
          ],
        },
      ],
    },
    {
      kind: 'metrics',
      id: 'metrics-1',
      createdAt: FIXTURE_EPOCH + 8,
      usage: {
        promptTokens: 2048,
        completionTokens: 512,
        cachedTokens: 1536,
        tokensPerSecond: 38.2,
        ttftMs: 288,
      },
      durationMs: 5120,
      model: 'fixture-model',
    },
    {
      kind: 'system',
      id: 'system-1',
      createdAt: FIXTURE_EPOCH + 9,
      level: 'warning',
      text: 'Gateway connection dropped; reconnecting.',
    },
  ];
}

/** A session with one cell of every kind. */
export function makeFixtureSession(overrides: Partial<SessionViewModel> = {}): SessionViewModel {
  const base: SessionViewModel = {
    sessionId: 'session-a',
    title: 'Fixture session',
    status: 'idle',
    attention: 'none',
    meta: {
      model: 'fixture-model',
      provider: 'fixture-provider',
      thinking: true,
      reasoningEffort: 'medium',
      yolo: false,
      agent: '',
      workspace: '/workspace',
      createdAt: '2026-01-01T00:00:00.000Z',
    },
    context: { usedTokens: 2048, windowTokens: 200_000, messageCount: 9 },
    totals: { promptTokens: 2048, completionTokens: 512, cachedTokens: 1536 },
    turn: { active: false, startedAtMs: 0, lastResult: null },
    lastError: null,
    panels: EMPTY_PANELS,
    seq: 0,
    cells: makeAllCells(),
  };
  return { ...base, ...overrides };
}

/** A session with an empty transcript (the "new tab" shape). */
export function makeEmptySession(sessionId = 'session-empty'): SessionViewModel {
  return makeFixtureSession({ sessionId, title: 'New session', cells: [], seq: 0 });
}

/** Matching tab bar entry. */
export function makeTab(session: SessionViewModel): TabModel {
  return {
    sessionId: session.sessionId,
    title: session.title,
    status: session.status,
    attention: session.attention,
  };
}

/** A pending user message cell (host-side optimistic entry). */
export function makePendingUserCell(id = 'user-pending', text = 'queued message'): UserCellModel {
  return { kind: 'user', id, createdAt: FIXTURE_EPOCH, text, state: 'pending' };
}

/** An ask cell as produced for a Bash dangerous-command confirmation. */
export function makeApprovalAskCell(id = 'ask-approval'): AskCellModel {
  return {
    kind: 'ask',
    id,
    createdAt: FIXTURE_EPOCH,
    requestId: 'approval-1',
    sessionId: 'session-a',
    approval: true,
    state: 'awaiting',
    answers: [],
    questions: [
      {
        id: 'approval',
        question: 'Run `rm -rf build`?',
        header: 'Approval',
        multiSelect: false,
        required: true,
        options: [
          { label: 'Approve', description: '' },
          { label: 'Deny', description: '' },
        ],
      },
    ],
  };
}
