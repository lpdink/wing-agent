import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { resetCollapseOverrides, resetImageUris } from '@wing-agent/ui';
import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest';

import { App } from '../src/app/App';
import { GatewayRuntime } from '../src/connection/runtime';
import { DEFAULT_SETTINGS, type GatewaySettings } from '../src/settings/settings';

import { FakeGateway } from './support/fake-gateway';
import { TEST_LOCATION } from './support/harness';
import { richSession, streamingSession } from './support/transcript-fixture';

/**
 * The transcript, rendered for real.
 *
 * Together these are the acceptance-level assertions of step 08: every cell kind the
 * replay produces reaches the DOM through the shared renderer (`@wing-agent/ui`'s
 * `CellView`), streaming keeps working through a live event, and an ask can be
 * answered end to end (click → `ClientRequest` on the socket → answered cell).
 */

/**
 * The image path needs two browser APIs jsdom does not have.
 *
 * Stubbing them here (rather than poking the renderer's cache from the test) is what
 * makes the image test assert the *app's* path: bridge → resolver → gateway fetch →
 * object URL → `<img>`. The default answers 404, so a test that does not care about
 * images gets the "refused → link" state deterministically.
 */
/** The stubbed `fetch` (typed, so the assertions and `mockImplementation` agree). */
type FetchMock = Mock<(url: string, init?: RequestInit) => Promise<Response>>;

let fetchMock: FetchMock;

beforeEach(() => {
  // The renderer keeps two module-level caches for the document (resolved image URIs
  // and the user's expand/collapse choices, the latter keyed by cell id — which is
  // reused across sessions by design). Both are per-document state in production, so
  // clearing them here is what keeps these tests independent of each other.
  resetImageUris();
  resetCollapseOverrides();
  fetchMock = vi.fn<(url: string, init?: RequestInit) => Promise<Response>>(() =>
    Promise.resolve(new Response(null, { status: 404 })),
  );
  vi.stubGlobal('fetch', fetchMock);
  const url = URL as unknown as { createObjectURL?: unknown; revokeObjectURL?: unknown };
  url.createObjectURL = (blob: Blob) => `blob:${blob.type}`;
  url.revokeObjectURL = () => undefined;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function buildRuntime(gateway: FakeGateway, settings: Partial<GatewaySettings> = {}): GatewayRuntime {
  return new GatewayRuntime({
    initialSettings: { ...DEFAULT_SETTINGS, ...settings },
    location: TEST_LOCATION,
    socketFactory: gateway.socketFactory,
    httpTransport: gateway.transport,
    listPollIntervalMs: 0,
    noticeTtlMs: 0,
    connectRetryBaseMs: 3_600_000,
  });
}

/**
 * A PNG answer, built fresh per call.
 *
 * `mockResolvedValue` would hand the *same* `Response` to every request, and a body
 * can only be read once — the second fetch of the same source (which is exactly what
 * a session switch produces) would fail to read it and look like a refusal.
 */
function imageResponse(): Response {
  return new Response(new Blob([new Uint8Array([1, 2, 3])], { type: 'image/png' }), {
    status: 200,
    headers: { 'content-type': 'image/png' },
  });
}

/** Mount the shell against one fake gateway and wait for the replay. */
async function mount(session = richSession()): Promise<{ gateway: FakeGateway; runtime: GatewayRuntime }> {
  const gateway = new FakeGateway({ sessions: [session] });
  const runtime = buildRuntime(gateway);
  render(<App runtime={runtime} />);
  await screen.findByTestId('transcript');
  return { gateway, runtime };
}

/** The row of one cell kind, in transcript order. */
function rows(kind: string): HTMLElement[] {
  return [...document.querySelectorAll<HTMLElement>(`[data-cell-kind="${kind}"]`)];
}

describe('transcript rendering', () => {
  it('renders one row per cell kind, in the replay’s order', async () => {
    const { runtime } = await mount();

    const kinds = [...document.querySelectorAll<HTMLElement>('[data-cell-id]')].map((row) =>
      row.getAttribute('data-cell-kind'),
    );
    expect(kinds).toEqual([
      'user',
      'thinking',
      'assistant',
      'tool_call', // Read
      'tool_call', // TodoWrite
      'todo',
      'tool_call', // Bash
      'user',
      'metrics', // the finished turn's accounting (`done` → finishTurn)
      'diff',
      'system', // the replayed notice
      'ask', // the question form
      'ask', // the approval
    ]);

    runtime.stop();
  });

  it('renders a user bubble and the assistant markdown', async () => {
    const { runtime } = await mount();

    expect(screen.getByText(/The parser chokes on nested tables/)).toBeTruthy();
    // Real markdown: a heading-free paragraph, inline code and a fenced block.
    const assistant = rows('assistant')[0];
    expect(assistant?.textContent).toContain('consumed as a paragraph');
    expect(assistant?.querySelector('[data-testid="md-code"]')).not.toBeNull();

    runtime.stop();
  });

  it('renders KaTeX for inline and display math', async () => {
    const { runtime } = await mount();

    const math = document.querySelectorAll('[data-testid="md-math"]');
    expect(math.length).toBe(2);
    // The engine rendered (not the source fallback): KaTeX emits its own spans.
    expect(math[0]?.querySelector('.katex')).not.toBeNull();
    expect(math[1]?.getAttribute('data-display')).toBe('true');

    runtime.stop();
  });

  it('loads a workspace image through the gateway and renders it', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(imageResponse()));
    const { runtime } = await mount();

    await waitFor(() => {
      expect(rows('assistant')[0]?.querySelector('[data-testid="md-image"]')).not.toBeNull();
    });
    expect(rows('assistant')[0]?.querySelector('[data-testid="md-image"]')?.getAttribute('src')).toBe(
      'blob:image/png',
    );
    // The frozen endpoint, addressed with the open session, from the page origin.
    expect(fetchMock.mock.calls[0]?.[0]).toBe(
      'http://localhost:5173/api/workspace/image?session_id=session-1&path=assets%2Fchart.png',
    );

    runtime.stop();
  });

  it('re-asks for images when the open session changes', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(imageResponse()));
    const other = richSession({ id: 'session-2', name: 'Second workspace', workspace: '/tmp/other' });
    const gateway = new FakeGateway({ sessions: [richSession(), other] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);
    await screen.findByTestId('transcript');

    await waitFor(() => {
      expect(fetchMock.mock.calls[0]?.[0]).toContain('session_id=session-1');
    });

    fireEvent.click(await screen.findByRole('button', { name: /Second workspace/ }));
    await waitFor(() => {
      // The renderer's image cache is document-level and keyed by the markdown source
      // alone, so the same `assets/chart.png` must be asked *again* for the new
      // session — otherwise the second workspace would show the first one's file.
      expect(fetchMock.mock.calls.some((call) => String(call[0]).includes('session_id=session-2'))).toBe(
        true,
      );
    });
    // …and the answer must survive the switch (a cache reset racing the new answer
    // would leave the transcript with a link where an image belongs).
    await waitFor(() => {
      expect(rows('assistant')[0]?.querySelector('[data-testid="md-image"]')).not.toBeNull();
    });

    runtime.stop();
  });

  it('keeps the link when the gateway refuses the image', async () => {
    const { runtime } = await mount();

    const imageRow = rows('assistant')[0];
    expect(imageRow?.textContent).toContain('before'); // the alt text of the link
    await waitFor(() => {
      expect(fetchMock).toHaveBeenCalled();
    });
    expect(imageRow?.querySelector('[data-testid="md-image"]')).toBeNull();
    // Refused is final: the renderer never asks again (the fetch happened once).
    expect(fetchMock).toHaveBeenCalledTimes(1);

    runtime.stop();
  });

  it('collapses a finished thinking block and opens a failed tool call', async () => {
    const { runtime } = await mount();

    const thinking = rows('thinking')[0];
    expect(thinking?.getAttribute('data-collapsed')).toBe('true');
    expect(thinking?.textContent).toContain('Thought');

    const tools = rows('tool_call');
    expect(tools[0]?.getAttribute('data-cell-status')).toBe('success');
    expect(tools[0]?.getAttribute('data-collapsed')).toBe('true');
    expect(tools[2]?.getAttribute('data-cell-status')).toBe('success'); // replayed results are success
    // The row already names its subject (a file reference, clickable on its own).
    expect(tools[0]?.textContent).toContain('src/parser.ts');
    // Expanding it reveals the arguments and the result cards.
    fireEvent.click(tools[0]?.querySelector('[role="button"]') as HTMLElement);
    await waitFor(() => {
      expect(rows('tool_call')[0]?.getAttribute('data-collapsed')).toBe('false');
    });
    expect(rows('tool_call')[0]?.textContent).toContain('Result');

    runtime.stop();
  });

  it('renders the diff card with its window and counts', async () => {
    const { runtime } = await mount();

    const diff = rows('diff')[0];
    expect(diff?.textContent).toContain('src/parser.ts');
    expect(diff?.textContent).toContain('+1');
    expect(diff?.textContent).toContain('−1');
    expect(diff?.querySelector('[data-testid="diff-body"]')?.children.length).toBeGreaterThan(0);
    // The escape hatch to the editor exists but cannot open anything here; the
    // bridge answers it with a notice (asserted in `bridge.test.ts`).
    expect(within(diff as HTMLElement).getByRole('button', { name: 'Open diff' })).toBeTruthy();

    runtime.stop();
  });

  it('renders the todo list and the metrics line', async () => {
    const { runtime } = await mount();

    const todo = rows('todo')[0];
    expect(todo?.querySelectorAll('[data-todo-status]').length).toBe(3);
    expect(todo?.textContent).toContain('Keep CJK wrapping intact');

    const metrics = rows('metrics')[0];
    expect(metrics?.textContent).toMatch(/12\.5k in · 640 out/);
    expect(metrics?.textContent).toContain('glm-4.6');

    runtime.stop();
  });

  it('renders a system line for a notice event', async () => {
    const { runtime } = await mount();

    expect(rows('system')[0]?.textContent).toContain('rate limited');

    runtime.stop();
  });

  it('shows the two ask shapes: a question form and an approval', async () => {
    const { runtime } = await mount();

    const asks = rows('ask');
    const question = asks.find((row) => row.getAttribute('data-ask-form') === 'question');
    const approval = asks.find((row) => row.getAttribute('data-ask-form') === 'approval');

    expect(question?.getAttribute('data-ask-state')).toBe('awaiting');
    expect(question?.textContent).toContain('Which files should the fix touch?');
    expect(approval?.textContent).toContain('Approval required');

    runtime.stop();
  });

  it('streams a live assistant delta, opening a cell and then growing it', async () => {
    const { gateway, runtime } = await mount();
    const before = rows('assistant').length;

    gateway.emit('session-1', {
      type: 'text',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:09Z',
      request_id: 'r-8',
      content: 'Also added ',
    });
    await waitFor(() => {
      expect(rows('assistant')).toHaveLength(before + 1);
    });
    const streamed = rows('assistant')[before];
    expect(streamed?.getAttribute('data-streaming')).toBe('true');
    expect(document.querySelectorAll('[data-testid="stream-caret"]').length).toBeGreaterThan(0);

    // The next fragment lands in the *same* cell (the reducer's rule: the last
    // committed cell is an assistant cell → append).
    gateway.emit('session-1', {
      type: 'text',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:10Z',
      request_id: 'r-9',
      content: 'the regression test.',
    });
    await waitFor(() => {
      expect(rows('assistant')).toHaveLength(before + 1);
    });
    expect(rows('assistant')[before]?.textContent).toContain('Also added the regression test.');

    runtime.stop();
  });

  it('expands a failed tool call by itself (live lane)', async () => {
    const { gateway, runtime } = await mount();

    gateway.emit('session-1', {
      type: 'tool_call',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:11Z',
      request_id: 'r-10',
      tool_call_id: 'toolu-live',
      tool_name: 'Bash',
      tool_args: { command: 'pnpm test parser' },
    });
    gateway.emit('session-1', {
      type: 'tool_call_result',
      session_id: 'session-1',
      created_at: '2026-10-01T12:00:12Z',
      request_id: 'r-11',
      tool_call_id: 'toolu-live',
      tool_name: 'Bash',
      tool_args: { command: 'pnpm test parser' },
      tool_result: 'FAIL src/parser.test.ts',
      tool_success: false,
      model: 'glm-4.6',
    });

    await waitFor(() => {
      const failed = rows('tool_call').find((row) => row.getAttribute('data-cell-status') === 'failed');
      expect(failed).toBeDefined();
    });
    const failed = rows('tool_call').find((row) => row.getAttribute('data-cell-status') === 'failed');
    expect(failed?.getAttribute('data-collapsed')).toBe('false');
    expect(failed?.textContent).toContain('FAIL src/parser.test.ts');

    runtime.stop();
  });

  it('renders an unterminated tool call from the replay projection', async () => {
    const gateway = new FakeGateway({ sessions: [streamingSession()] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);
    await screen.findByTestId('transcript');

    const tool = rows('tool_call')[0];
    expect(tool?.getAttribute('data-cell-status')).toBe('streaming');
    // The row's subject comes from the partial-JSON parse of the streamed fragment
    // (the reducer's, never the backend's: it never parses half a JSON).
    expect(tool?.textContent).toContain('docs/release.md');

    runtime.stop();
  });

  it('answers an ask through the DOM and the socket', async () => {
    const { gateway, runtime } = await mount();

    const question = rows('ask').find((row) => row.getAttribute('data-ask-form') === 'question');
    fireEvent.click(within(question as HTMLElement).getByLabelText(/parser\.ts/));
    fireEvent.click(within(question as HTMLElement).getByRole('button', { name: 'Submit' }));

    await waitFor(() => {
      const frames = gateway.lastSocket()?.clientFrames() ?? [];
      expect(frames).toHaveLength(1);
      expect(frames[0]).toMatchObject({
        session_id: 'session-1',
        content: 'Scope: parser.ts',
        tool_call_id: 'toolu-ask',
      });
      expect(typeof frames[0]?.['request_id']).toBe('string');
    });
    await waitFor(() => {
      expect(rows('ask')[0]?.getAttribute('data-ask-state')).toBe('answered');
    });

    runtime.stop();
  });

  it('answers an approval through the DOM', async () => {
    const { gateway, runtime } = await mount();

    const approval = rows('ask').find((row) => row.getAttribute('data-ask-form') === 'approval');
    fireEvent.click(within(approval as HTMLElement).getByRole('button', { name: 'Deny' }));

    await waitFor(() => {
      expect(gateway.lastSocket()?.clientFrames()[0]?.['content']).toBe('n');
    });

    runtime.stop();
  });

  it('shows the session details folded away, with the transcript filling the pane', async () => {
    const { runtime } = await mount();

    const details = document.querySelector('.pane__details');
    expect(details).not.toBeNull();
    expect(details?.hasAttribute('open')).toBe(false);
    expect(details?.textContent).toContain('Session details');
    expect(details?.textContent).toMatch(/12,480 \/ 200,000 tokens \(6%\) · 10 messages/);
    expect(details?.textContent).toContain('glm-4.6');

    runtime.stop();
  });

  it('renders an empty transcript line for a session without cells', async () => {
    const gateway = new FakeGateway({ sessions: [richSession({ messages: [], events: [] })] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);

    expect(await screen.findByTestId('transcript-empty')).toBeTruthy();

    runtime.stop();
  });
});
