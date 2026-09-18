// SCAFFOLD(01): static fixture used by the scaffold placeholder page.
//
// Step 01 has no gateway client (step 02) and no session orchestration (step 03),
// so the view still has to prove the whole pipeline: host reduction → bridge →
// hydrate → render. This fixture is the stand-in for the real session model.
//
// Step 03 deletes this file (grep for SCAFFOLD(01)) — nothing else imports it.
import type { CellModel, SessionViewModel, TabModel } from '../../shared';

const CREATED_AT = '2026-01-01T00:00:00.000Z';
const BASE_TIME = Date.UTC(2026, 0, 1, 0, 0, 0);

/** Fake transcript covering every cell kind except `ask` (which would need a live agent). */
const PLACEHOLDER_CELLS: readonly CellModel[] = [
  {
    kind: 'separator',
    id: 'c1',
    createdAt: BASE_TIME,
    label: 'Scaffold preview',
  },
  {
    kind: 'system',
    id: 'c2',
    createdAt: BASE_TIME + 1,
    level: 'notice',
    text: 'This page is rendered from fixture data. Sessions, streaming and controls arrive with the host layer (step 03).',
  },
  {
    kind: 'user',
    id: 'c3',
    createdAt: BASE_TIME + 2,
    text: 'Summarize what the scaffold already wires up.',
    state: 'accepted',
  },
  {
    kind: 'metrics',
    id: 'c4',
    createdAt: BASE_TIME + 3,
    usage: {
      promptTokens: 1284,
      completionTokens: 96,
      cachedTokens: 1024,
      tokensPerSecond: 41.5,
      ttftMs: 312,
    },
    durationMs: 2310,
    model: 'scaffold-model',
  },
  {
    kind: 'thinking',
    id: 'c5',
    createdAt: BASE_TIME + 4,
    text: 'The host derives every field; the webview only applies patches. Keeping the fixture JSON-only proves the bridge survives a JSON round-trip.',
    streaming: false,
    durationMs: 820,
  },
  {
    kind: 'assistant',
    id: 'c6',
    createdAt: BASE_TIME + 5,
    streaming: false,
    text: [
      'Right now the extension wires up:',
      '',
      '- an activity-bar view (`wing.chatView`) served by `src/host/chatViewProvider.ts`;',
      '- a CSP + nonce + `asWebviewUri` pipeline for the webview document;',
      '- the typed bridge in `src/shared/bridge.ts` (hydrate, ordered patches, resync).',
      '',
      '```ts',
      'const patch: CellPatch = { op: "append_text", cellId: "c6", text: "…" };',
      '```',
    ].join('\n'),
  },
  {
    kind: 'tool_call',
    id: 'c7',
    createdAt: BASE_TIME + 6,
    toolCallId: 'tool-1',
    name: 'Bash',
    status: 'success',
    display: { title: 'Bash', subject: 'pnpm run typecheck' },
    argsText: '{"command":"pnpm run typecheck"}',
    args: { command: 'pnpm run typecheck' },
    result: { text: 'tsc: no errors (3 projects)', isError: false, truncated: false },
    startedAt: BASE_TIME + 6,
    finishedAt: BASE_TIME + 6 + 5120,
  },
  {
    kind: 'diff',
    id: 'c8',
    createdAt: BASE_TIME + 7,
    path: 'src/host/html.ts',
    oldStartLine: 1,
    newStartLine: 1,
    lines: [
      { kind: 'hunk', text: '@@ -1,4 +1,5 @@', oldLine: null, newLine: null },
      { kind: 'context', text: "import { randomUUID } from 'node:crypto';", oldLine: 1, newLine: 1 },
      { kind: 'del', text: 'const csp = "default-src none";', oldLine: 2, newLine: null },
      {
        kind: 'add',
        text: 'const csp = buildContentSecurityPolicy({ cspSource, nonce });',
        oldLine: null,
        newLine: 2,
      },
      {
        kind: 'add',
        text: 'const styleTag = styleUri === null ? "" : link(styleUri);',
        oldLine: null,
        newLine: 3,
      },
    ],
    added: 2,
    removed: 1,
    truncated: false,
    toolCallId: 'tool-1',
  },
  {
    kind: 'todo',
    id: 'c9',
    createdAt: BASE_TIME + 8,
    items: [
      { content: 'Scaffold the extension package', status: 'completed' },
      { content: 'Wire the webview pipeline', status: 'completed' },
      { content: 'Land real sessions on top of the bridge', status: 'in_progress' },
    ],
  },
  {
    kind: 'system',
    id: 'c10',
    createdAt: BASE_TIME + 9,
    level: 'info',
    text: 'Bridge online — press Ping host to round-trip a message.',
  },
];

/** Fixture session shown by the scaffold placeholder. */
export function createPlaceholderSession(): SessionViewModel {
  return {
    sessionId: 'scaffold-session',
    title: 'Wing scaffold',
    status: 'idle',
    attention: 'none',
    meta: {
      model: 'scaffold-model',
      provider: 'fixture',
      thinking: true,
      reasoningEffort: 'medium',
      yolo: false,
      agent: '',
      workspace: '/workspace',
      createdAt: CREATED_AT,
    },
    context: { usedTokens: 1284, windowTokens: 200000, messageCount: 12 },
    totals: { promptTokens: 1284, completionTokens: 96, cachedTokens: 1024 },
    turn: { active: false, startedAtMs: 0, lastResult: null },
    lastError: null,
    panels: { modelPicker: null, globalNotice: null },
    seq: 7,
    cells: PLACEHOLDER_CELLS,
  };
}

/** Tab bar contents matching {@link createPlaceholderSession}. */
export const PLACEHOLDER_TABS: readonly TabModel[] = [
  { sessionId: 'scaffold-session', title: 'Wing scaffold', status: 'idle', attention: 'none' },
];
