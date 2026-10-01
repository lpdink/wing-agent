/**
 * The screenshot scenes — code, not configuration (design.md D11).
 *
 * Each scene is "a fixture world + a viewport set (+ an optional interaction)".
 * Relative timestamps are computed against the run's clock so the *labels* in the
 * list ("4 min ago") are stable no matter when the scene is rendered — only the
 * absolute dates would drift, and they are not shown.
 */

import type { Page } from 'playwright';

import type { World, WorldSession } from './server';

export type ViewportId = 'desktop' | 'mobile';

export interface ViewportSpec {
  readonly id: ViewportId;
  readonly width: number;
  readonly height: number;
}

/** The two fixed viewports of the acceptance protocol. */
export const VIEWPORTS: Readonly<Record<ViewportId, ViewportSpec>> = {
  desktop: { id: 'desktop', width: 1_440, height: 900 },
  mobile: { id: 'mobile', width: 390, height: 844 },
};

export interface SceneContext {
  /** A loopback port with nothing listening (the "gateway is down" case). */
  readonly deadPort: number;
}

export interface ShotScene {
  readonly name: string;
  /** One line for the manifest: what this shot is evidence of. */
  readonly title: string;
  readonly world: World;
  readonly viewports: readonly ViewportId[];
  readonly colorScheme?: 'light' | 'dark';
  /** localStorage values to install before the app boots. */
  readonly seed?: (context: SceneContext) => Record<string, string>;
  /** Run after `ready`, before the screenshot (clicks, dialogs, …). */
  readonly interact?: (page: Page) => Promise<void>;
  /** Resolves when the UI is in the state worth photographing. */
  readonly ready: (page: Page) => Promise<unknown>;
}

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;

function ago(ms: number): string {
  return new Date(Date.now() - ms).toISOString();
}

function message(
  role: string,
  content: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return { role, content, uuid: `${role}-${content.slice(0, 12)}`, ...extra };
}

/** `context_stats`: the context-window numbers the main pane shows. */
function contextStats(count: number, used: number, window: number): Record<string, unknown> {
  return {
    type: 'context_stats',
    session_id: 'wing-1',
    created_at: ago(HOUR),
    request_id: 'ctx-1',
    message_count: count,
    total_tokens: used,
    context_window_tokens: window,
    system_prompt_parts: [],
  };
}

/** `session_state_changed`: model knobs, which the replay alone does not carry. */
function stateChanged(): Record<string, unknown> {
  return {
    type: 'session_state_changed',
    session_id: 'wing-1',
    created_at: ago(HOUR),
    request_id: 'state-1',
    model: 'glm-4.6',
    model_display_name: 'GLM-4.6',
    thinking: true,
    reasoning_effort: 'high',
    yolo: false,
    title: 'Fix the parser',
    agent: null,
  };
}

const WORKSPACE = '/Users/dev/projects/wing';

function session(seed: Partial<WorldSession> & { id: string; name: string }): WorldSession {
  return {
    workspace: WORKSPACE,
    status: 'idle',
    lastInteraction: ago(HOUR),
    messages: [],
    events: [],
    info: {
      model: 'glm-4.6',
      thinking: true,
      reasoningEffort: 'high',
      yolo: false,
      workdir: WORKSPACE,
      usedTokens: 42_100,
      windowTokens: 200_000,
      messageCount: 18,
    },
    ...seed,
  };
}

/** The "a real workspace" world: four sessions, one of them mid-turn. */
const WORKING_WORLD: World = {
  sessions: [
    session({
      id: 'wing-1',
      name: 'Fix the parser',
      status: 'working',
      lastInteraction: ago(4 * MINUTE),
      messages: [
        message('user', 'The parser chokes on nested tables in the docs — can you take a look?'),
        message('assistant', 'Looking at it now: the table body is consumed as a paragraph.'),
        message('user', 'Right. Fix it without breaking the CJK wrapping.'),
      ],
      events: [contextStats(18, 42_100, 200_000), stateChanged()],
      info: {
        model: 'glm-4.6',
        thinking: true,
        reasoningEffort: 'high',
        yolo: false,
        workdir: WORKSPACE,
        usedTokens: 42_100,
        windowTokens: 200_000,
        messageCount: 18,
      },
    }),
    session({ id: 'wing-2', name: 'Deploy check', lastInteraction: ago(35 * MINUTE) }),
    session({
      id: 'wing-3',
      name: 'Rewrite the docs',
      status: 'waiting',
      lastInteraction: ago(2 * HOUR),
    }),
    session({
      id: 'wing-4',
      name: 'Nightly run',
      status: 'inactive',
      lastInteraction: ago(26 * HOUR),
    }),
  ],
};

/**
 * The rich transcript world (step 08): one session whose replay produces every cell
 * kind the renderer has — user / thinking / assistant markdown (list, table, KaTeX,
 * a highlighted code block, a workspace image) / two tool calls / a diff / a system
 * line / todo / an awaiting question form / a Bash approval / the turn's metrics.
 *
 * Two scenes photograph it: `transcript` (scrolled to the top — the assistant's
 * answer) and `transcript-tail` (pinned to the bottom — the cells that ask for
 * input). `uncommitted_tools` is empty: the streaming path has its own world.
 */
const ANSWER_MARKDOWN = [
  'Found it — the table body is consumed as a paragraph.',
  '',
  '**What changes**',
  '',
  '- `src/parser.ts` — stop the paragraph at the table boundary',
  '- `src/wrap.ts` — keep the CJK line breaking intact',
  '',
  'The fix keeps inline math like $a^2 + b^2 = c^2$ working, and this display block:',
  '',
  '$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$',
  '',
  '```ts',
  'export function tableBody(tokens: Token[]): Node {',
  "  return parseParagraph(tokens, { stopAt: 'table' });",
  '}',
  '```',
  '',
  '| case | before | after |',
  '| --- | --- | --- |',
  '| nested table | 1 row | 3 rows |',
  '| CJK wrap | broken | intact |',
  '',
  'The render before the change:',
  '',
  '![before the fix](assets/chart.png)',
].join('\n');

const RICH_WORLD: World = {
  sessions: [
    session({
      id: 'wing-1',
      name: 'Fix the parser',
      status: 'waiting',
      lastInteraction: ago(4 * MINUTE),
      messages: [
        message('user', 'The parser chokes on nested tables in the docs — can you take a look?'),
        message('assistant', '', {
          reasoning_content:
            'Check how the table body is consumed before touching the parser, and keep the CJK wrap rules in mind.',
        }),
        message('assistant', ANSWER_MARKDOWN),
        message('assistant', '', {
          tool_calls: [{ id: 'toolu-read', name: 'Read', arguments: { path: 'src/parser.ts' } }],
        }),
        message(
          'tool',
          'export function tableBody(tokens: Token[]): Node {\n  return parseParagraph(tokens);\n}',
          {
            tool_call_id: 'toolu-read',
          },
        ),
        message('assistant', '', {
          tool_calls: [
            {
              id: 'toolu-edit',
              name: 'Edit',
              arguments: {
                path: 'src/parser.ts',
                old_str: 'return parseParagraph(tokens);',
                new_str: "return parseParagraph(tokens, { stopAt: 'table' });",
              },
            },
          ],
        }),
        message('tool', 'Edited src/parser.ts (+1 −1)', { tool_call_id: 'toolu-edit' }),
        message('assistant', '', {
          tool_calls: [
            {
              id: 'toolu-todo',
              name: 'TodoWrite',
              arguments: {
                todos: [
                  { content: 'Fix the table body parser', status: 'completed' },
                  { content: 'Keep CJK wrapping intact', status: 'in_progress' },
                  { content: 'Add a regression test', status: 'pending' },
                ],
              },
            },
          ],
        }),
        message('tool', 'Todos updated', { tool_call_id: 'toolu-todo' }),
        message('user', 'Right. Fix it without breaking the CJK wrapping.'),
      ],
      events: [
        { type: 'turn_started', session_id: 'wing-1', created_at: ago(3 * MINUTE), request_id: 'r-1' },
        {
          type: 'llm_call_metrics',
          session_id: 'wing-1',
          created_at: ago(3 * MINUTE),
          request_id: 'r-2',
          model: 'glm-4.6',
          prompt_tokens: 12_480,
          completion_tokens: 640,
          cached_tokens: 3_120,
          first_chunk_rt_ms: 412,
          tokens_per_sec: 41.2,
          stop_reason: 'end_turn',
        },
        { type: 'done', session_id: 'wing-1', created_at: ago(3 * MINUTE), request_id: 'r-3' },
        contextStats(10, 12_480, 200_000),
        {
          type: 'diff_content',
          session_id: 'wing-1',
          created_at: ago(3 * MINUTE),
          request_id: 'r-4',
          path: 'src/parser.ts',
          old_text: 'export function tableBody(tokens: Token[]): Node {\n  return parseParagraph(tokens);\n}',
          new_text:
            "export function tableBody(tokens: Token[]): Node {\n  return parseParagraph(tokens, { stopAt: 'table' });\n}",
          old_start_line: 41,
          new_start_line: 41,
          tool_call_id: 'toolu-edit',
        },
        {
          type: 'notice',
          session_id: 'wing-1',
          created_at: ago(3 * MINUTE),
          request_id: 'r-5',
          level: 'warning',
          message: 'LLM call retried once (rate limited)',
          attempt: 1,
          max_attempts: 3,
          retry_in_s: 2,
        },
        {
          type: 'ask',
          session_id: 'wing-1',
          created_at: ago(2 * MINUTE),
          request_id: 'r-6',
          tool_call_id: 'toolu-ask',
          questions: [
            {
              id: 'q1',
              header: 'Scope',
              question: 'Which files should the fix touch?',
              multiSelect: false,
              options: [
                { label: 'parser.ts', description: 'the table body parser' },
                { label: 'wrap.ts', description: 'the CJK wrapper' },
              ],
            },
            {
              id: 'q2',
              header: 'Tests',
              question: 'Add a regression test for the nested case?',
              multiSelect: false,
              options: [
                { label: 'yes', description: 'src/parser.test.ts' },
                { label: 'no', description: 'keep the change minimal' },
              ],
            },
          ],
          question: '',
          choices: [],
          required: false,
        },
        {
          type: 'ask',
          session_id: 'wing-1',
          created_at: ago(2 * MINUTE),
          request_id: 'r-7',
          tool_call_id: 'toolu-approve',
          questions: [],
          question: 'Bash command needs approval: pnpm test parser',
          choices: ['Approve', 'Deny'],
          required: true,
        },
      ],
      info: {
        model: 'glm-4.6',
        thinking: true,
        reasoningEffort: 'high',
        yolo: false,
        workdir: WORKSPACE,
        usedTokens: 12_480,
        windowTokens: 200_000,
        messageCount: 10,
      },
    }),
  ],
};

/**
 * The live-streaming world: a replay of one short exchange, then two scripted arm
 * deltas — the state a user actually watches ("the model is writing").
 *
 * The script stops mid-answer on purpose: the screenshot then catches a *frozen*
 * state (an open thinking block that has just closed, an assistant cell still
 * growing with the caret), and two runs cannot differ.
 */
const STREAMING_WORLD: World = {
  sessions: [
    session({
      id: 'wing-1',
      name: 'Release notes',
      status: 'working',
      lastInteraction: ago(20_000),
      messages: [message('user', 'Write the release notes for v0.2.')],
      events: [{ type: 'turn_started', session_id: 'wing-1', created_at: ago(15_000), request_id: 'r-live' }],
      info: {
        model: 'glm-4.6',
        thinking: true,
        reasoningEffort: 'high',
        yolo: false,
        workdir: WORKSPACE,
        usedTokens: 3_100,
        windowTokens: 200_000,
        messageCount: 2,
      },
      live: [
        {
          delayMs: 60,
          event: {
            type: 'reasoning',
            session_id: 'wing-1',
            created_at: ago(10_000),
            request_id: 'r-live',
            content: 'Collect the merged PRs, then group them by area before writing.',
          },
        },
        {
          delayMs: 90,
          event: {
            type: 'text',
            session_id: 'wing-1',
            created_at: ago(9_000),
            request_id: 'r-live',
            content: '## v0.2 highlights\n\n- Nested tables in the docs are parsed again\n- ',
          },
        },
      ],
    }),
  ],
};

/**
 * The unterminated-tool world: the model is streaming a `Write` call's arguments.
 *
 * The reducer parses the partial JSON for the row's subject and keeps the raw text
 * for the expanded "Arguments" card — both are visible in this frozen frame, and
 * the authoritative arguments only arrive with the (never sent) final `tool_call`.
 */
const STREAMING_TOOL_WORLD: World = {
  sessions: [
    session({
      id: 'wing-1',
      name: 'Release notes',
      status: 'working',
      lastInteraction: ago(20_000),
      messages: [
        message('user', 'Write the release notes for v0.2.'),
        message('assistant', 'Sure — drafting `docs/release-0.2.md` now.'),
      ],
      events: [{ type: 'turn_started', session_id: 'wing-1', created_at: ago(15_000), request_id: 'r-tool' }],
      info: {
        model: 'glm-4.6',
        thinking: true,
        reasoningEffort: 'high',
        yolo: false,
        workdir: WORKSPACE,
        usedTokens: 3_400,
        windowTokens: 200_000,
        messageCount: 3,
      },
      live: [
        {
          delayMs: 60,
          event: {
            type: 'tool_call_stream',
            session_id: 'wing-1',
            created_at: ago(8_000),
            request_id: 'r-tool',
            tool_call_id: 'toolu-write',
            tool_name: 'Write',
            args_fragment: '{"path": "docs/release-0.2.md", "content": "# v0.2\\n\\n',
            is_final: false,
          },
        },
        {
          delayMs: 60,
          event: {
            type: 'tool_call_stream',
            session_id: 'wing-1',
            created_at: ago(7_000),
            request_id: 'r-tool',
            tool_call_id: 'toolu-write',
            tool_name: 'Write',
            args_fragment: '- nested tables in the docs are parsed again\\n- ',
            is_final: false,
          },
        },
      ],
    }),
  ],
};

const EMPTY_WORLD: World = { sessions: [] };

/** Settings that point at a port where nothing listens. */
function deadAddressSeed(context: SceneContext): Record<string, string> {
  return {
    'wing.web.gateway': JSON.stringify({
      version: 1,
      scheme: 'http',
      host: '127.0.0.1',
      port: context.deadPort,
      apiKey: null,
      ignoreCertErrors: false,
    }),
  };
}

/** Scroll the transcript to its first row (the assistant's answer, not the tail). */
async function scrollTranscriptToTop(page: Page): Promise<void> {
  await page.evaluate(() => {
    const transcript = document.querySelector('[data-testid="transcript"]');
    if (transcript !== null) {
      transcript.scrollTop = 0;
    }
  });
}

export const SCENES: readonly ShotScene[] = [
  {
    name: 'transcript',
    title:
      'Rich transcript, scrolled to the top: user bubble, folded thinking, assistant markdown (list, KaTeX, highlighted code, table) and a workspace image fetched from the gateway',
    world: RICH_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.getByTestId('transcript').waitFor();
      // The image is the last thing to arrive (the gateway answers it): waiting for
      // it is what makes the shot reproducible.
      await page.getByTestId('md-image').first().waitFor();
      await page.getByTestId('md-math').first().waitFor();
    },
    async interact(page) {
      await scrollTranscriptToTop(page);
    },
  },
  {
    name: 'transcript-tail',
    title:
      'The same transcript pinned to its newest cells: tool rows, todo list, diff card, system line, the awaiting question form, the Bash approval and the turn metrics',
    world: RICH_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.getByTestId('transcript').waitFor();
      await page.getByTestId('md-image').first().waitFor();
      await page.getByRole('button', { name: 'Approve' }).waitFor();
    },
  },
  {
    name: 'transcript-dark',
    title:
      'The rich transcript under prefers-color-scheme: dark — the theme bridge and the dark syntax colours',
    world: RICH_WORLD,
    viewports: ['desktop'],
    colorScheme: 'dark',
    async ready(page) {
      await page.getByTestId('transcript').waitFor();
      await page.getByTestId('md-image').first().waitFor();
    },
    async interact(page) {
      await scrollTranscriptToTop(page);
    },
  },
  {
    name: 'streaming',
    title:
      'Mid-answer: the assistant cell still streaming (the animated caret), preceded by the thinking block the answer closed',
    world: STREAMING_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.locator('[data-cell-kind="assistant"][data-streaming="true"]').waitFor();
      await page.locator('[data-cell-kind="thinking"]').waitFor();
    },
  },
  {
    name: 'streaming-tool',
    title:
      'An unterminated tool call: the row’s subject comes from the partial-JSON parse, the expanded card from the raw streamed arguments',
    world: STREAMING_TOOL_WORLD,
    viewports: ['desktop'],
    async ready(page) {
      await page.locator('[data-cell-kind="tool_call"][data-cell-status="streaming"]').waitFor();
    },
    async interact(page) {
      // Open the row: the arguments card is the point of this shot.
      await page.locator('[data-cell-kind="tool_call"] [role="button"]').first().click();
      await page.getByText('Arguments').waitFor();
    },
  },
  {
    name: 'sessions',
    title: 'Connected shell: session list (four sessions, one working), live session state, connection pill',
    world: WORKING_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.getByRole('button', { name: /Fix the parser/ }).waitFor();
      await page.getByText('working', { exact: true }).first().waitFor();
      await page.getByText('turn running…').waitFor();
    },
  },
  {
    name: 'sessions-dark',
    title: 'Same shell under prefers-color-scheme: dark (the theme follows the OS)',
    world: WORKING_WORLD,
    viewports: ['desktop'],
    colorScheme: 'dark',
    async ready(page) {
      await page.getByRole('button', { name: /Fix the parser/ }).waitFor();
      await page.getByText('turn running…').waitFor();
    },
  },
  {
    name: 'empty',
    title: 'Connected gateway with no sessions: the empty state and the New session path',
    world: EMPTY_WORLD,
    viewports: ['desktop'],
    async ready(page) {
      await page.getByText('No session open').first().waitFor();
      await page.getByText('No sessions yet.').waitFor();
    },
  },
  {
    name: 'settings',
    title: 'The gateway settings form: scheme/host/port/api key/ignore-cert, with the resolved URLs',
    world: WORKING_WORLD,
    viewports: ['desktop', 'mobile'],
    async ready(page) {
      await page.getByRole('button', { name: /Fix the parser/ }).waitFor();
    },
    async interact(page) {
      await page.getByRole('button', { name: 'Settings' }).click();
      await page.getByRole('dialog', { name: 'Gateway settings' }).waitFor();
    },
  },
  {
    name: 'first-connect-failure',
    title: 'A gateway that cannot be reached: the banner with the reason and the settings entry point',
    world: EMPTY_WORLD,
    viewports: ['desktop', 'mobile'],
    seed: deadAddressSeed,
    async ready(page) {
      await page.getByRole('alert').first().waitFor();
    },
  },
];
