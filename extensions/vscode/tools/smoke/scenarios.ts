/**
 * The smoke scenarios.
 *
 * Each scenario drives the production host through the real gateway and asserts
 * on the **UI model** the webview would render (cells / status / title / meta),
 * plus the real side effects (files on disk, the fake provider's request log).
 *
 * Conventions:
 * - one scenario = one script for the model, reset right before it runs, so
 *   "one request = one turn" stays true;
 * - waiting is condition-based with a deadline (`SmokeWorld.waitFor`), never a
 *   fixed sleep;
 * - a scenario that leaves state behind (a session, a yolo toggle) cleans up so
 *   the next one starts from a known world.
 */

import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';

import type { AskCellModel, ToolCallCellModel } from '../../src/shared';

import type { SmokeGateway } from './gateway';
import type { FakeProvider, Turn } from './fake-provider';
import type { SmokeWorld } from './world';
import { activeTab } from './world';

export interface ScenarioContext {
  readonly world: SmokeWorld;
  readonly gateway: SmokeGateway;
  readonly provider: FakeProvider;
  readonly model: string;
  /** Print a line of progress (scenario steps are useful when something fails). */
  readonly log: (message: string) => void;
}

export interface Scenario {
  readonly name: string;
  readonly description: string;
  run(context: ScenarioContext): Promise<string>;
}

/** A concise turn builder for scenarios. */
export function turn(partial: Turn): Turn {
  return partial;
}

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

function askCell(world: SmokeWorld, sessionId: string): AskCellModel | undefined {
  return world
    .cells(sessionId)
    .filter((cell): cell is AskCellModel => cell.kind === 'ask')
    .at(-1);
}

function toolCell(world: SmokeWorld, sessionId: string, name: string): ToolCallCellModel | undefined {
  return world
    .cells(sessionId)
    .filter((cell): cell is ToolCallCellModel => cell.kind === 'tool_call' && cell.name === name)
    .at(-1);
}

async function createSession(context: ScenarioContext): Promise<string> {
  const before = context.world.openSessionIds();
  await context.world.intent({ type: 'newSession' });
  await context.world.waitFor(() => context.world.openSessionIds().length === before.length + 1, {
    label: 'a new tab',
  });
  const created = context.world.openSessionIds().find((id) => !before.includes(id));
  assert(created !== undefined, 'the new session did not appear in the tab list');
  // `state` for the new session means subscribe resolved and the replay landed.
  await context.world.waitFor(() => context.world.state(created) !== undefined, {
    label: 'the new session to be subscribed',
  });
  return created;
}

async function closeSession(context: ScenarioContext, sessionId: string): Promise<void> {
  await context.world.intent({ type: 'closeSession', sessionId });
  await context.world.waitFor(() => !context.world.openSessionIds().includes(sessionId), {
    label: 'the tab to close',
  });
}

async function sessionInfo(gateway: SmokeGateway, sessionId: string): Promise<Record<string, unknown>> {
  // The scenarios verify the gateway's *own* view with a raw fetch, so they must
  // authenticate exactly like the host does when `WING_SMOKE_AUTH_KEY` is set
  // (review #109 [P3-6] — the key travels in a header, never in the URL).
  const authKey = process.env['WING_SMOKE_AUTH_KEY'];
  const response = await fetch(`${gateway.baseUrl}/api/session/info?session_id=${sessionId}`, {
    ...(authKey === undefined || authKey === '' ? {} : { headers: { Authorization: `Bearer ${authKey}` } }),
  });
  assert(response.ok, `GET /api/session/info failed with ${response.status}`);
  return (await response.json()) as Record<string, unknown>;
}

/** One request/turn of plain text, with a deterministic usage report. */
function textTurn(text: string): Turn {
  return turn({
    text,
    usage: { promptTokens: 12, completionTokens: 5, cachedTokens: 0 },
    chunk: 6,
  });
}

// ─────────────────────────────────────────────────────────────────────
// S1 — create → subscribe → send → stream → turn done
// ─────────────────────────────────────────────────────────────────────

const createSubscribeSendStream: Scenario = {
  name: 'create-subscribe-send-stream',
  description: 'the initial session, a streamed answer, the derived title and the metrics cell',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [
      turn({
        thinking: 'The user said ping; answer with one short line.',
        text: 'pong from the smoke provider.',
        usage: { promptTokens: 21, completionTokens: 9, cachedTokens: 4 },
        chunk: 7,
      }),
    ]);

    const sessionId = world.openSessionIds()[0];
    assert(sessionId !== undefined, 'no initial session was created on ready');
    assert(world.hydrate(sessionId) !== undefined, 'the initial session never got a hydrate message');

    await world.send(sessionId, 'ping');
    await world.waitFor(
      () => world.textOf(sessionId, 'assistant').includes('pong from the smoke provider.'),
      {
        label: 'the streamed answer',
      },
    );
    await world.waitFor(() => world.state(sessionId)?.status === 'idle', {
      label: 'the turn to finish',
    });
    // The turn started and ended (a fast model finishes between two polls, so the
    // "working" phase is asserted on the state history the webview received).
    assert(
      world.statuses(sessionId).includes('working'),
      `the session never showed working: ${world.statuses(sessionId).join(' → ')}`,
    );
    assert(
      world.statuses(sessionId).at(-1) === 'idle',
      `the session should end idle: ${world.statuses(sessionId).join(' → ')}`,
    );

    const kinds = world.kinds(sessionId);
    assert(kinds.includes('user'), `no user cell in ${kinds.join(', ')}`);
    assert(kinds.includes('thinking'), `no thinking cell in ${kinds.join(', ')}`);
    assert(kinds.includes('assistant'), `no assistant cell in ${kinds.join(', ')}`);
    assert(kinds.includes('metrics'), `no metrics cell in ${kinds.join(', ')}`);

    const userCell = world.cells(sessionId).find((cell) => cell.kind === 'user');
    assert(userCell !== undefined && userCell.kind === 'user', 'the user cell is missing');
    assert(userCell.state === 'accepted', `the user cell stayed ${userCell.state}`);

    const state = world.state(sessionId);
    assert(
      state?.title === 'ping',
      `title should be the first user message, got ${JSON.stringify(state?.title)}`,
    );
    assert(state?.turn.active === false, 'the turn stayed active after done');
    assert(
      state.meta.yolo === false,
      `a fresh session must report yolo=false from /api/session/info, got ${String(state.meta.yolo)}`,
    );
    assert(world.mirror.errors.length === 0, `webview mirror errors: ${world.mirror.errors.join(' | ')}`);

    const info = await sessionInfo(context.gateway, sessionId);
    const stats = info['context_stats'] as { message_count?: number } | undefined;
    assert(
      (stats?.message_count ?? 0) >= 2,
      `the gateway should know about the user + assistant message, got ${JSON.stringify(info['context_stats'])}`,
    );
    assert(world.editor.copies.length === 0, 'nothing should have been copied');

    return `${kinds.length} cells, title ${JSON.stringify(state?.title)}`;
  },
};

// ─────────────────────────────────────────────────────────────────────
// S2 — tool call + diff (Write)
// ─────────────────────────────────────────────────────────────────────

const toolCallAndDiff: Scenario = {
  name: 'tool-call-diff',
  description: 'a real Write tool call: tool cell, diff cell and the file on disk',
  async run(context) {
    const { world, provider, model, gateway } = context;
    const content = 'written by the wing vscode smoke\n';
    provider.setScript(model, [
      turn({
        toolCalls: [{ name: 'Write', args: { path: 'smoke.txt', content } }],
        usage: { promptTokens: 30, completionTokens: 12 },
      }),
      textTurn('The file is written.'),
    ]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'write smoke.txt');
    await world.waitFor(() => world.kinds(sessionId).includes('diff'), {
      label: 'the diff cell',
      timeoutMs: 20_000,
    });
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('The file is written.'), {
      label: 'the follow-up answer',
    });

    const tool = toolCell(world, sessionId, 'Write');
    assert(tool !== undefined, 'no Write tool cell');
    assert(tool.status === 'success', `the Write tool cell is ${tool.status}`);

    const diff = world.cells(sessionId).find((cell) => cell.kind === 'diff');
    assert(diff !== undefined && diff.kind === 'diff', 'no diff cell');
    assert(path.basename(diff.path) === 'smoke.txt', `the diff should point at smoke.txt, got ${diff.path}`);

    const file = path.join(gateway.workspace, 'smoke.txt');
    assert(existsSync(file), `the tool did not write ${file}`);
    assert(readFileSync(file, 'utf8') === content, 'the file content differs from the tool call args');

    await closeSession(context, sessionId);
    return `diff at ${path.basename(diff.path)}, file verified`;
  },
};

// ─────────────────────────────────────────────────────────────────────
// S3 — AskUserQuestion round trip
// ─────────────────────────────────────────────────────────────────────

const askRoundTrip: Scenario = {
  name: 'ask-round-trip',
  description: 'AskUserQuestion → waiting-for-input → answer → the model continues',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [
      turn({
        toolCalls: [
          {
            name: 'AskUserQuestion',
            args: {
              questions: [
                {
                  id: 'pick_one',
                  header: 'Choice',
                  question: 'Which option should I take?',
                  options: [
                    { label: 'Alpha', description: 'the first one' },
                    { label: 'Beta', description: 'the second one' },
                  ],
                },
              ],
            },
          },
        ],
      }),
      textTurn('Alpha it is.'),
    ]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'ask me something');
    await world.waitFor(() => askCell(world, sessionId)?.state === 'awaiting', {
      label: 'the ask cell',
    });
    assert(world.state(sessionId)?.status === 'waiting-for-input', 'the status should be waiting-for-input');

    const cell = askCell(world, sessionId);
    assert(cell !== undefined, 'no ask cell');
    assert(cell.approval === false, 'AskUserQuestion must not be the approval shape');
    assert(cell.questions.length === 1, `expected one question, got ${cell.questions.length}`);
    const question = cell.questions[0];
    assert(question !== undefined && question.options.length === 2, 'the question did not carry the options');

    await world.intent({
      type: 'answerAsk',
      sessionId,
      requestId: cell.requestId,
      answers: [{ questionId: question.id, selected: ['Alpha'], text: '' }],
    });
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('Alpha it is.'), {
      label: 'the answer to reach the model',
    });

    const answered = askCell(world, sessionId);
    assert(answered?.state === 'answered', `the ask cell should be answered, is ${String(answered?.state)}`);
    assert(
      answered.answers[0]?.selected[0] === 'Alpha',
      `the answer should be echoed back, got ${JSON.stringify(answered.answers)}`,
    );
    assert(world.state(sessionId)?.status === 'idle', 'the session should be idle again');

    await closeSession(context, sessionId);
    return 'answered with Alpha, model continued';
  },
};

// ─────────────────────────────────────────────────────────────────────
// S4 — Bash dangerous-command approval, then yolo
// ─────────────────────────────────────────────────────────────────────

const bashApproval: Scenario = {
  name: 'bash-approval',
  description: 'dangerous Bash: ask → approve (y) → runs; with yolo on it runs without asking',
  async run(context) {
    const { world, provider, model, gateway } = context;
    provider.setScript(model, [
      turn({ toolCalls: [{ name: 'Bash', args: { command: 'touch approved.txt' } }] }),
      textTurn('Approved command done.'),
      turn({ toolCalls: [{ name: 'Bash', args: { command: 'touch second.txt' } }] }),
      textTurn('Second command done.'),
    ]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'run a command');
    await world.waitFor(() => askCell(world, sessionId)?.state === 'awaiting', {
      label: 'the approval ask',
      timeoutMs: 20_000,
    });

    const cell = askCell(world, sessionId);
    assert(cell !== undefined, 'no approval ask cell');
    assert(cell.approval === true, 'a dangerous command must use the approval shape');
    assert(
      cell.questions[0]?.options.map((option) => option.label).join(',') === 'y,n,yolo',
      `unexpected approval options: ${JSON.stringify(cell.questions[0]?.options)}`,
    );

    await world.intent({ type: 'approveTool', sessionId, requestId: cell.requestId, decision: 'approve' });
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('Approved command done.'), {
      label: 'the approved command to run',
    });
    assert(
      existsSync(path.join(gateway.workspace, 'approved.txt')),
      'the approved command did not create its file',
    );
    const asksAfterApproval = world.cells(sessionId).filter((entry) => entry.kind === 'ask').length;

    // YOLO from the status area, then a second dangerous command: no new ask.
    await world.intent({ type: 'setYolo', sessionId, enabled: true });
    await world.waitFor(() => world.state(sessionId)?.meta.yolo === true, {
      label: 'the yolo toggle to reach the session state',
    });
    await world.send(sessionId, 'run another command');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('Second command done.'), {
      label: 'the yolo command to run',
    });
    assert(
      existsSync(path.join(gateway.workspace, 'second.txt')),
      'the yolo command did not create its file',
    );
    assert(
      world.cells(sessionId).filter((entry) => entry.kind === 'ask').length === asksAfterApproval,
      'a command ran under yolo without asking, but a new ask appeared',
    );

    // The gateway agrees yolo is on (the status area reads the same source).
    const info = await sessionInfo(gateway, sessionId);
    assert(info['yolo'] === true, `the gateway should report yolo=true, got ${JSON.stringify(info['yolo'])}`);

    // Clean up the mode for the following scenarios.
    await world.intent({ type: 'setYolo', sessionId, enabled: false });
    await closeSession(context, sessionId);
    return 'approved once, then yolo ran without asking';
  },
};

// ─────────────────────────────────────────────────────────────────────
// S5 — interrupt
// ─────────────────────────────────────────────────────────────────────

const interruptTurn: Scenario = {
  name: 'interrupt',
  description: 'interrupt a streaming turn: status returns to idle, no half tool blocks',
  async run(context) {
    const { world, provider, model } = context;
    const long = Array.from({ length: 60 }, (_, index) => `line ${index} of a slow stream.`).join(' ');
    provider.setScript(model, [
      turn({ text: long, chunk: 24, delayMs: 20 }),
      textTurn('after the interrupt'),
    ]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'stream slowly');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').length > 0, {
      label: 'the first streamed delta',
    });

    await world.intent({ type: 'interrupt', sessionId });
    await world.waitFor(() => world.state(sessionId)?.status !== 'working', {
      label: 'the interrupt to land',
      timeoutMs: 20_000,
    });
    assert(world.state(sessionId)?.turn.active === false, 'the turn stayed active after interrupt');
    assert(
      !world.kinds(sessionId).includes('tool_call'),
      'an interrupted text stream must not leave tool cells behind',
    );

    // The session is still usable: a fresh message completes normally.
    await world.send(sessionId, 'are you alive?');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('after the interrupt'), {
      label: 'the session to recover',
      timeoutMs: 20_000,
    });

    await closeSession(context, sessionId);
    return 'interrupted mid-stream, then recovered';
  },
};

// ─────────────────────────────────────────────────────────────────────
// S6 — resume: history replay + runtime state (checkpoint② #3)
// ─────────────────────────────────────────────────────────────────────

const resumeHistoryAndState: Scenario = {
  name: 'resume-history-state',
  description: 'close a tab, resume it: history replays, and yolo/thinking come from /api/session/info',
  async run(context) {
    const { world, provider, model, gateway } = context;
    provider.setScript(model, [textTurn('stored answer'), textTurn('should not be needed')]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'remember me');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('stored answer'), {
      label: 'the first answer',
    });
    await world.intent({ type: 'setYolo', sessionId, enabled: true });
    await world.waitFor(() => world.state(sessionId)?.meta.yolo === true, { label: 'yolo to be on' });

    const before = world.cells(sessionId).length;
    const title = world.state(sessionId)?.title;
    await closeSession(context, sessionId);
    await world.intent({ type: 'activateSession', sessionId });
    await world.waitFor(() => world.openSessionIds().includes(sessionId), { label: 'the resumed tab' });
    await world.waitFor(() => world.state(sessionId)?.meta.yolo === true, {
      label: 'the runtime state refresh (yolo)',
      timeoutMs: 20_000,
    });

    const state = world.state(sessionId);
    assert(
      state?.title === title,
      `the title changed across resume: ${String(state?.title)} vs ${String(title)}`,
    );
    const cells = world.cells(sessionId);
    assert(
      world.textOf(sessionId, 'assistant').includes('stored answer'),
      'the replayed transcript lost the stored answer',
    );
    assert(
      cells.filter((cell) => cell.kind === 'assistant').length === 1,
      `the replay duplicated the assistant answer: ${cells.map((cell) => cell.kind).join(', ')}`,
    );
    assert(
      cells.filter((cell) => cell.kind === 'user').length === 1,
      `the replay duplicated the user message: ${cells.map((cell) => cell.kind).join(', ')}`,
    );
    // The live transcript also carried derived cells (the metrics card) that the
    // gateway's replay deliberately does not resend — so "fewer cells" is fine,
    // "the content is missing or duplicated" is not.
    assert(cells.length <= before, `the replay appended instead of replacing: ${cells.length} > ${before}`);

    const info = await sessionInfo(gateway, sessionId);
    assert(info['yolo'] === true, 'the gateway should still report yolo=true');

    await world.intent({ type: 'setYolo', sessionId, enabled: false });
    await world.waitFor(() => world.state(sessionId)?.meta.yolo === false, { label: 'yolo to be off again' });
    return `replayed ${cells.length} cells (live transcript had ${before}), yolo survived the resume`;
  },
};

// ─────────────────────────────────────────────────────────────────────
// S7 — rewind
// ─────────────────────────────────────────────────────────────────────

const rewindToMessage: Scenario = {
  name: 'rewind',
  description:
    'bare /rewind opens the picker; /rewind <uuid> drops the tail and hands the text back as a draft',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [textTurn('first reply'), textTurn('second reply')]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'first question');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('first reply'), {
      label: 'the first reply',
    });
    await world.send(sessionId, 'second question');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('second reply'), {
      label: 'the second reply',
    });

    await world.intent({ type: 'runPromptCommand', sessionId, name: '/rewind', argsText: '' });
    await world.waitFor(() => (world.lastPanels(sessionId)?.branchPicker?.rows.length ?? 0) > 0, {
      label: 'the branch picker rows',
    });
    const picker = world.lastPanels(sessionId)?.branchPicker;
    assert(picker?.mode === 'rewind', `the picker is in ${String(picker?.mode)} mode`);
    const target = picker.rows.find((row) => row.content.startsWith('second question'));
    assert(target !== undefined, `no branch row for "second question": ${JSON.stringify(picker.rows)}`);

    const cellsBefore = world.cells(sessionId).length;
    await world.intent({ type: 'runPromptCommand', sessionId, name: '/rewind', argsText: target.uuid });
    // The draft travels once (in the `state` that follows the rewind's sync); the
    // host consumes it immediately, so the assertion is on the delivery history.
    await world.waitFor(() => world.drafts(sessionId).includes('second question'), {
      label: 'the rewound draft',
      timeoutMs: 20_000,
    });

    const cellsAfter = world.cells(sessionId);
    assert(
      cellsAfter.length < cellsBefore,
      `the transcript should shrink: ${cellsAfter.length} >= ${cellsBefore}`,
    );
    assert(
      !world.textOf(sessionId, 'assistant').includes('second reply'),
      'the rewind kept the answer it was supposed to drop',
    );
    assert(world.textOf(sessionId, 'assistant').includes('first reply'), 'the rewind dropped too much');
    assert(
      world.lastPanels(sessionId)?.branchPicker === null,
      'the branch picker should close after the action',
    );

    await closeSession(context, sessionId);
    return `rewound ${cellsBefore} → ${cellsAfter.length} cells, draft restored`;
  },
};

// ─────────────────────────────────────────────────────────────────────
// S8 — fork
// ─────────────────────────────────────────────────────────────────────

const forkAtMessage: Scenario = {
  name: 'fork',
  description: 'bare /fork opens the picker; /fork <uuid> opens a new tab with its own session and draft',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [textTurn('reply to fork from')]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'fork source');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('reply to fork from'), {
      label: 'the source answer',
    });

    await world.intent({ type: 'runPromptCommand', sessionId, name: '/fork', argsText: '' });
    await world.waitFor(() => (world.lastPanels(sessionId)?.branchPicker?.rows.length ?? 0) > 0, {
      label: 'the fork picker rows',
    });
    const picker = world.lastPanels(sessionId)?.branchPicker;
    assert(picker?.mode === 'fork', `the picker is in ${String(picker?.mode)} mode`);
    const target = picker.rows.find((row) => row.content.startsWith('fork source'));
    assert(target !== undefined, `no branch row for "fork source": ${JSON.stringify(picker.rows)}`);

    const before = world.openSessionIds();
    await world.intent({ type: 'runPromptCommand', sessionId, name: '/fork', argsText: target.uuid });
    await world.waitFor(() => world.openSessionIds().length === before.length + 1, {
      label: 'the forked tab',
      timeoutMs: 20_000,
    });
    const forked = world.openSessionIds().find((id) => !before.includes(id));
    assert(forked !== undefined, 'the fork did not open a tab');
    assert(world.tabs().activeSessionId === forked, 'the fork should become the active tab');
    assert(
      world.drafts(forked).includes('fork source'),
      `the fork should hand the target text back as a draft, saw ${JSON.stringify(world.drafts(forked))}`,
    );
    // The source tab is untouched by the fork.
    assert(
      world.textOf(sessionId, 'assistant').includes('reply to fork from'),
      'the source transcript changed',
    );

    await closeSession(context, forked);
    await closeSession(context, sessionId);
    return `forked into ${forked} with the draft restored`;
  },
};

// ─────────────────────────────────────────────────────────────────────
// S9 — two tabs, no cross-talk
// ─────────────────────────────────────────────────────────────────────

const multiTabIsolation: Scenario = {
  name: 'multi-tab-isolation',
  description: 'two live sessions: an event for one never lands in the other',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [textTurn('answer for A'), textTurn('answer for B')]);

    const a = await createSession(context);
    const b = await createSession(context);
    await world.waitFor(() => world.state(a) !== undefined && world.state(b) !== undefined, {
      label: 'both tabs hydrated',
    });

    await world.send(a, 'question for A');
    await world.waitFor(() => world.textOf(a, 'assistant').includes('answer for A'), {
      label: 'the answer in tab A',
    });

    const bCells = world.cells(b);
    assert(!bCells.some((cell) => cell.kind === 'assistant'), 'tab B received a cell from tab A');
    assert(world.openSessionIds().length >= 2, 'both tabs should still be open');
    assert(world.tabs().tabs.length === world.openSessionIds().length, 'the tab bar lost a tab');
    assert(activeTab(world)?.sessionId === b, 'the active tab should still be the last one opened');

    // B answers on its own, and A keeps its own transcript.
    await world.send(b, 'question for B');
    await world.waitFor(() => world.textOf(b, 'assistant').includes('answer for B'), {
      label: 'the answer in tab B',
    });
    assert(world.textOf(a, 'assistant').includes('answer for A'), 'tab A lost its answer');
    assert(!world.textOf(a, 'assistant').includes('answer for B'), 'tab A got tab B’s answer');

    await closeSession(context, a);
    await closeSession(context, b);
    return 'two tabs, zero cross-talk';
  },
};

// ─────────────────────────────────────────────────────────────────────
// S10 — reconnect: resubscribe + replay, then keep working
// ─────────────────────────────────────────────────────────────────────

const reconnectAndResubscribe: Scenario = {
  name: 'reconnect-resubscribe',
  description: 'a re-established connection resubscribes every tab, replays it, and keeps delivering',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [textTurn('before the reconnect'), textTurn('after the reconnect')]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'before the break');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('before the reconnect'), {
      label: 'the pre-reconnect answer',
    });
    const cellsBefore = world.cells(sessionId).length;
    const textsBefore = world.textOf(sessionId, 'assistant');

    // Drop the connection and reconnect the way `wing.reconnectGateway` does.
    await world.host.reconnect();
    await world.waitFor(() => world.host.connectionState?.status === 'connected', {
      label: 'the reconnection',
      timeoutMs: 20_000,
    });
    // The resubscribe + replay must not duplicate what the transcript already had.
    await world.waitFor(() => world.cells(sessionId).length === cellsBefore, {
      label: 'the replayed transcript to match the previous one',
    });
    assert(
      world.textOf(sessionId, 'assistant') === textsBefore,
      'the replay duplicated or dropped the transcript content',
    );

    // And the route is attached again: a new message round-trips.
    await world.send(sessionId, 'after the break');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('after the reconnect'), {
      label: 'a message after the reconnect',
      timeoutMs: 20_000,
    });

    await closeSession(context, sessionId);
    return 'resubscribed, replayed without duplicates, still delivering';
  },
};

// ─────────────────────────────────────────────────────────────────────
// S11 — local commands never reach the model (checkpoint② #1)
// ─────────────────────────────────────────────────────────────────────

const localCommands: Scenario = {
  name: 'local-commands',
  description: '/context and /skills answer from the session info and never touch the model',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [textTurn('should never be requested')]);

    const sessionId = await createSession(context);
    await world.send(sessionId, 'seed the context');
    await world.waitFor(() => world.textOf(sessionId, 'assistant').includes('should never be requested'), {
      label: 'the seed answer',
    });
    const requestsBefore = provider.requestCount();

    await world.intent({ type: 'runPromptCommand', sessionId, name: '/context', argsText: '' });
    await world.waitFor(() => world.textOf(sessionId, 'system').includes('Tokens:'), {
      label: 'the /context answer',
    });
    const contextText = world.textOf(sessionId, 'system');
    assert(
      contextText.includes('Messages:'),
      `the context answer is missing the message count: ${contextText}`,
    );
    assert(
      contextText.includes('--- System Prompt ---'),
      `the context answer should include the system prompt: ${contextText}`,
    );

    await world.intent({ type: 'runPromptCommand', sessionId, name: '/skills', argsText: '' });
    await world.waitFor(
      () => {
        const cells = world.cells(sessionId);
        const last = cells.at(-1);
        return last?.kind === 'system';
      },
      { label: 'the /skills answer' },
    );

    assert(
      provider.requestCount() === requestsBefore,
      `local commands reached the model: ${requestsBefore} → ${provider.requestCount()}`,
    );

    await closeSession(context, sessionId);
    return 'both commands answered locally (0 model requests)';
  },
};

// ─────────────────────────────────────────────────────────────────────
// S12 — a gateway prompt command still goes to the model
// ─────────────────────────────────────────────────────────────────────

const gatewayPromptCommand: Scenario = {
  name: 'gateway-command-to-model',
  description: '/init is a gateway prompt command: it must still reach the model',
  async run(context) {
    const { world, provider, model } = context;
    provider.setScript(model, [textTurn('gateway command handled')]);

    const sessionId = await createSession(context);
    const before = provider.requestCount();
    await world.send(sessionId, '/init');
    await world.waitFor(() => provider.requestCount() === before + 1, {
      label: 'the model request for /init',
      timeoutMs: 20_000,
    });

    await closeSession(context, sessionId);
    return '/init reached the provider';
  },
};

export const SCENARIOS: readonly Scenario[] = [
  createSubscribeSendStream,
  toolCallAndDiff,
  askRoundTrip,
  bashApproval,
  interruptTurn,
  resumeHistoryAndState,
  rewindToMessage,
  forkAtMessage,
  multiTabIsolation,
  reconnectAndResubscribe,
  localCommands,
  gatewayPromptCommand,
];
