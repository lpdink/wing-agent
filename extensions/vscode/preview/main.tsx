import type { CellPatch, PanelsModel, SessionViewModel } from '../src/shared';
import { EMPTY_PANELS } from '../src/shared';
import {
  makeApprovalAskCell,
  makeBranchPicker,
  makeCommandCatalog,
  makeEmptySession,
  makeFailedToolCell,
  makeFixtureSession,
  makeLongSession,
  makeModelPicker,
  makeModelPickerSession,
  makeSessionPicker,
  makeShellSession,
  makeStreamingCells,
  makeStreamingToolCell,
  makeWorkingSession,
} from '../src/testing/fixtures';
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

const SHELL_PANELS: PanelsModel = {
  ...EMPTY_PANELS,
  commandCatalog: makeCommandCatalog(),
};

/** A second session so the tab bar (and tab switching) is part of the preview. */
const SECOND_SESSION = makeFixtureSession({
  sessionId: 'session-b',
  title: 'Second tab',
  status: 'waiting-for-input',
  cells: [makeApprovalAskCell()],
  seq: 0,
});

const FIXTURES: Record<string, readonly SessionViewModel[]> = {
  'all cells': [makeFixtureSession()],
  'streaming turn': [makeFixtureSession({ title: 'Streaming turn', cells: makeStreamingCells(), seq: 0 })],
  'failed tool': [
    makeFixtureSession({
      title: 'Tool failure',
      cells: [makeFailedToolCell(), makeStreamingToolCell()],
      seq: 0,
    }),
  ],
  approval: [makeFixtureSession({ title: 'Approval', cells: [makeApprovalAskCell()], seq: 0 })],
  empty: [makeEmptySession()],
  'shell (idle)': [makeShellSession()],
  'shell (working + queue)': [makeWorkingSession()],
  'shell (model picker open)': [makeModelPickerSession()],
  'shell (session picker open)': [
    makeShellSession({
      panels: { ...EMPTY_PANELS, commandCatalog: makeCommandCatalog(), sessionPicker: makeSessionPicker() },
    }),
  ],
  'shell (branch picker open)': [
    makeShellSession({
      panels: {
        ...EMPTY_PANELS,
        commandCatalog: makeCommandCatalog(),
        branchPicker: makeBranchPicker('rewind'),
      },
    }),
  ],
  'two tabs': [makeShellSession(), SECOND_SESSION],
  'long session': [makeLongSession()],
};

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

let bridge = createMockBridge({ sessions: FIXTURES['all cells'] ?? [] });
let app = mountApp(appRoot, { transport: bridge.transport });
let streamTimer: number | null = null;
/** Mirror of the session the scripted host is streaming into (kept in sync with `seq`). */
let current: SessionViewModel = FIXTURES['all cells']?.[0] ?? makeEmptySession();

function stopStream(): void {
  if (streamTimer !== null) {
    window.clearInterval(streamTimer);
    streamTimer = null;
  }
}

function remount(name: FixtureName): void {
  stopStream();
  app.dispose();
  const sessions = FIXTURES[name] ?? [];
  current = sessions[0] ?? makeEmptySession();
  bridge = createMockBridge({ sessions });
  app = mountApp(appRoot, { transport: bridge.transport });
}

/** Push the shell's catalogs as the host would. */
function pushPanels(panels: PanelsModel): void {
  bridge.push({ type: 'panels', sessionId: current.sessionId, panels });
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
for (const name of Object.keys(FIXTURES)) {
  const option = document.createElement('option');
  option.value = name;
  option.textContent = name;
  select.append(option);
}
select.addEventListener('change', () => {
  remount(select.value);
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
  button('Catalogs', () => {
    pushPanels(SHELL_PANELS);
  }),
);

toolbar.append(
  button('Model panel', () => {
    pushPanels({ ...SHELL_PANELS, modelPicker: makeModelPicker() });
  }),
);

toolbar.append(
  button('Session picker', () => {
    pushPanels({ ...SHELL_PANELS, sessionPicker: makeSessionPicker() });
  }),
);

toolbar.append(
  button('Branch picker', () => {
    pushPanels({ ...SHELL_PANELS, branchPicker: makeBranchPicker('rewind') });
  }),
);

toolbar.append(
  button('Notice', () => {
    pushPanels({
      ...EMPTY_PANELS,
      globalNotice: { level: 'warning', text: 'Gateway reconnecting (attempt 2)…' },
    });
  }),
);

toolbar.append(
  button('Close overlays', () => {
    bridge.push({ type: 'ui', action: { kind: 'closeOverlays' } });
  }),
);

toolbar.append(
  button('Reload', () => {
    remount(select.value);
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
