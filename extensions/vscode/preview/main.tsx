import type { CellModel, CellPatch, SessionViewModel } from '../src/shared';
import { makeEmptySession, makeFixtureSession } from '../src/testing/fixtures';
import { createMockBridge } from '../src/testing/mockBridge';
import { mountApp } from '../src/webview/mount';

/**
 * Preview harness entry.
 *
 * Mounts the *real* webview app (`src/webview/mount.tsx`) against a scripted host,
 * so the renderer can be built and inspected without VS Code, a gateway, or the
 * extension being built (`pnpm run dev:preview` / `pnpm run build:preview`).
 *
 * The toolbar drives the protocol paths that matter during development: hydrate a
 * fixture, stream a turn with ordered patches, break the patch stream (resync
 * recovery), and push a UI action.
 */

const FIXTURES = {
  'all cells': makeFixtureSession(),
  empty: makeEmptySession(),
  'long session': makeLongSession(),
} satisfies Record<string, SessionViewModel>;

type FixtureName = keyof typeof FIXTURES;

/** The harness must fail loudly: a missing mount point means the HTML and the
 * entry drifted apart, and a blank page would hide it. */
function requireElement(id: string): HTMLElement {
  const element = document.getElementById(id);
  if (element === null) {
    throw new Error(`preview harness: missing #${id}`);
  }
  return element;
}

const toolbar = requireElement('preview-root');
const appRoot = requireElement('root');

let bridge = createMockBridge({ sessions: [FIXTURES['all cells']] });
let app = mountApp(appRoot, { transport: bridge.transport });
let streamTimer: number | null = null;
/** Mirror of the session the scripted host is streaming into (kept in sync with `seq`). */
let current: SessionViewModel = FIXTURES['all cells'];

function stopStream(): void {
  if (streamTimer !== null) {
    window.clearInterval(streamTimer);
    streamTimer = null;
  }
}

function remount(name: FixtureName): void {
  stopStream();
  app.dispose();
  current = FIXTURES[name];
  bridge = createMockBridge({ sessions: [current] });
  app = mountApp(appRoot, { transport: bridge.transport });
}

function pushPatches(patches: readonly CellPatch[]): void {
  current = { ...current, seq: current.seq + 1 };
  bridge.push({ type: 'patch', sessionId: current.sessionId, seq: current.seq, patches });
}

// ── Toolbar ────────────────────────────────────────────────────────────

toolbar.innerHTML = '';

const label = document.createElement('span');
label.className = 'preview-label';
label.textContent = 'Preview harness';
toolbar.append(label);

const select = document.createElement('select');
select.className = 'preview-select';
select.dataset['testid'] = 'fixture-select';
for (const name of Object.keys(FIXTURES) as FixtureName[]) {
  const option = document.createElement('option');
  option.value = name;
  option.textContent = name;
  select.append(option);
}
select.addEventListener('change', () => {
  remount(select.value as FixtureName);
});
toolbar.append(select);

toolbar.append(
  button('Stream turn', () => {
    stopStream();
    pushPatches([
      {
        op: 'append',
        cell: {
          kind: 'user',
          id: 'stream-user',
          createdAt: Date.now(),
          text: 'Stream me a reply.',
          state: 'accepted',
        },
      },
      {
        op: 'append',
        cell: {
          kind: 'assistant',
          id: 'stream-assistant',
          createdAt: Date.now(),
          text: '',
          streaming: true,
        },
      },
    ]);

    const chunks = [
      'Streaming ',
      'patches ',
      'are ',
      'applied ',
      'in ',
      'order…\n\n',
      'No ',
      'full ',
      're-render ',
      'required.',
    ];
    let index = 0;
    streamTimer = window.setInterval(() => {
      const chunk = chunks[index];
      index += 1;
      if (chunk === undefined) {
        stopStream();
        pushPatches([
          {
            op: 'update',
            cell: {
              kind: 'assistant',
              id: 'stream-assistant',
              createdAt: Date.now(),
              text: chunks.join(''),
              streaming: false,
            },
          },
        ]);
        return;
      }
      pushPatches([{ op: 'append_text', cellId: 'stream-assistant', text: chunk }]);
    }, 120);
  }),
);

toolbar.append(
  button('Break stream', () => {
    stopStream();
    // Skip a seq: the webview must notice and ask for a resync instead of
    // rendering a model it cannot trust.
    current = { ...current, seq: current.seq + 5 };
    bridge.push({
      type: 'patch',
      sessionId: current.sessionId,
      seq: current.seq,
      patches: [{ op: 'append_text', cellId: 'assistant-1', text: ' (lost patches)' }],
    });
  }),
);

toolbar.append(
  button('Toast', () => {
    bridge.push({
      type: 'ui',
      action: { kind: 'toast', level: 'info', message: 'Host-driven toast through the `ui` channel.' },
    });
  }),
);

toolbar.append(
  button('Reload', () => {
    remount(select.value as FixtureName);
  }),
);

function button(text: string, onClick: () => void): HTMLButtonElement {
  const element = document.createElement('button');
  element.type = 'button';
  element.className = 'preview-button';
  element.textContent = text;
  element.addEventListener('click', onClick);
  return element;
}

/** A session with enough cells to exercise scrolling / memoization (step 04). */
function makeLongSession(): SessionViewModel {
  const cells: CellModel[] = [];
  for (let turn = 0; turn < 25; turn += 1) {
    cells.push({ kind: 'separator', id: `sep-${turn}`, createdAt: Date.now(), label: `Turn ${turn + 1}` });
    cells.push({
      kind: 'user',
      id: `user-${turn}`,
      createdAt: Date.now(),
      text: `Question ${turn + 1}: how does the transcript stay cheap to render?`,
      state: 'accepted',
    });
    cells.push({
      kind: 'assistant',
      id: `assistant-${turn}`,
      createdAt: Date.now(),
      streaming: false,
      text: `Answer ${turn + 1}: cells are memoized by id; only the streaming tail re-renders.`,
    });
  }
  return makeFixtureSession({ sessionId: 'session-long', title: 'Long session', cells, seq: 0 });
}
