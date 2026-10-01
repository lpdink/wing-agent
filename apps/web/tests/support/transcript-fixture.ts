/**
 * A replay world that exercises every cell kind — the transcript fixtures.
 *
 * It is a *real* `sync_session` payload (the same shape step 03 / the gateway send),
 * so the tests drive the actual reduction lane: the cells they assert are the cells
 * the app produces, not a hand-built model. The message/event order mirrors what the
 * history log holds after such a session (done → facts → the waiting questions).
 */

import { makeSession, type FakeSession } from './fake-gateway';

export const RICH_WORKSPACE = '/Users/dev/projects/wing';

const ANSWER_MARKDOWN = [
  'Found it — the table body is consumed as a paragraph.',
  '',
  'The fix keeps inline math like $a^2 + b^2 = c^2$ working and uses:',
  '',
  '$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$',
  '',
  '```ts',
  'export function tableBody(tokens: Token[]): Node {',
  '  return parseParagraph(tokens, { stopAt: "table" });',
  '}',
  '```',
  '',
  'The chart below is the render before the fix:',
  '',
  '![before](assets/chart.png)',
].join('\n');

/** Messages: user → thinking → text → tool call + result → todo tool → user. */
const MESSAGES: Record<string, unknown>[] = [
  { role: 'user', content: 'The parser chokes on nested tables — can you take a look?', uuid: 'u-1' },
  {
    role: 'assistant',
    content: '',
    uuid: 'a-thinking',
    reasoning_content: 'Check how the table body is consumed before changing the parser.',
  },
  { role: 'assistant', content: ANSWER_MARKDOWN, uuid: 'a-1' },
  {
    role: 'assistant',
    content: '',
    uuid: 'a-call',
    tool_calls: [{ id: 'toolu-read', name: 'Read', arguments: { path: 'src/parser.ts' } }],
  },
  {
    role: 'tool',
    content: 'export function tableBody(tokens: Token[]): Node {\n  return parseParagraph(tokens);\n}',
    uuid: 't-read',
    tool_call_id: 'toolu-read',
  },
  {
    role: 'assistant',
    content: '',
    uuid: 'a-todo',
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
  },
  { role: 'tool', content: 'Todos updated', uuid: 't-todo', tool_call_id: 'toolu-todo' },
  {
    role: 'assistant',
    content: '',
    uuid: 'a-fail',
    tool_calls: [{ id: 'toolu-bash', name: 'Bash', arguments: { command: 'pnpm test parser' } }],
  },
  {
    role: 'tool',
    content: 'FAIL src/parser.test.ts > nested tables\n  expected 3 rows, got 1',
    uuid: 't-fail',
    tool_call_id: 'toolu-bash',
  },
  { role: 'user', content: 'Right. Fix it without breaking the CJK wrapping.', uuid: 'u-2' },
];

/** Fact events: turn accounting, the diff, a system line, then the waiting asks. */
const EVENTS: Record<string, unknown>[] = [
  { type: 'turn_started', session_id: 'session-1', created_at: '2026-10-01T12:00:00Z', request_id: 'r-1' },
  {
    type: 'llm_call_metrics',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:02Z',
    request_id: 'r-2',
    model: 'glm-4.6',
    prompt_tokens: 12_480,
    completion_tokens: 640,
    cached_tokens: 3_120,
    first_chunk_rt_ms: 412,
    tokens_per_sec: 41.2,
    stop_reason: 'end_turn',
  },
  { type: 'done', session_id: 'session-1', created_at: '2026-10-01T12:00:03Z', request_id: 'r-3' },
  {
    type: 'context_stats',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:03Z',
    request_id: 'r-3b',
    message_count: 10,
    total_tokens: 12_480,
    context_window_tokens: 200_000,
    system_prompt_parts: [],
  },
  {
    type: 'diff_content',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:04Z',
    request_id: 'r-4',
    path: 'src/parser.ts',
    old_text: 'export function tableBody(tokens: Token[]): Node {\n  return parseParagraph(tokens);\n}',
    new_text:
      'export function tableBody(tokens: Token[]): Node {\n  return parseParagraph(tokens, { stopAt: "table" });\n}',
    old_start_line: 41,
    new_start_line: 41,
    tool_call_id: 'toolu-edit',
  },
  {
    type: 'notice',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:05Z',
    request_id: 'r-5',
    level: 'warning',
    message: 'LLM call retried once (rate limited)',
    attempt: 1,
    max_attempts: 3,
    retry_in_s: 2,
  },
  {
    type: 'ask',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:06Z',
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
    ],
    question: '',
    choices: [],
    required: false,
  },
  {
    type: 'ask',
    session_id: 'session-1',
    created_at: '2026-10-01T12:00:07Z',
    request_id: 'r-7',
    tool_call_id: 'toolu-approve',
    questions: [],
    question: 'Bash command needs approval: pnpm install --force',
    choices: ['Approve', 'Deny'],
    required: true,
  },
];

/** One session whose replay produces every cell kind. */
export function richSession(seed: Partial<FakeSession> = {}): FakeSession {
  return makeSession({
    id: 'session-1',
    name: 'Fix the parser',
    workspace: RICH_WORKSPACE,
    status: 'waiting',
    messages: MESSAGES,
    events: EVENTS,
    runtime: {
      model: 'glm-4.6',
      api_url: 'http://127.0.0.1:9/v1',
      tools: ['Bash', 'Read', 'Edit', 'Write', 'TodoWrite', 'AskUserQuestion'],
      total_tokens: 12_480,
      context_window_tokens: 200_000,
      thinking: true,
      reasoning_effort: 'high',
      yolo: false,
      session_name: 'Fix the parser',
      workdir: RICH_WORKSPACE,
      status: 'waiting',
      context_stats: { message_count: 10, total_tokens: 12_480 },
      skills_info: '',
      system_prompt: '',
    },
    ...seed,
  });
}

/**
 * A session mid-turn: an unterminated tool call whose arguments are still streaming.
 *
 * `uncommitted_tools` is what the gateway sends a late subscriber — the same
 * projection a live `tool_call_stream` grows, which is why the two are asserted
 * against the same DOM hooks (see `transcript.test.tsx`).
 */
export function streamingSession(): FakeSession {
  return makeSession({
    id: 'session-2',
    name: 'Streaming',
    workspace: RICH_WORKSPACE,
    status: 'working',
    messages: [{ role: 'user', content: 'Write the release notes.', uuid: 'u-1' }],
    events: [],
    uncommittedTools: [
      {
        tool_call_id: 'toolu-write',
        tool_name: 'Write',
        args_fragment: '{"path": "docs/release.md", "content": "# Release 0.2\\n\\n- tab',
      },
    ],
  });
}
