import { describe, expect, it } from 'vitest';

import { DIFF_SCHEME, VsCodeEditorActions, diffWindowTexts } from '../../src/host/editorActions';
import type { DiffCellModel } from '../../src/shared';
import { env, mockState } from '../mocks/vscode';

/**
 * Editor-side actions: the only place the host touches the editor API on behalf
 * of a webview intent. Asserted through the `vscode` mock (no GUI).
 */

function diffCell(overrides: Partial<DiffCellModel> = {}): DiffCellModel {
  return {
    kind: 'diff',
    id: 'cell-diff',
    createdAt: 0,
    path: 'src/a.ts',
    oldStartLine: 3,
    newStartLine: 3,
    lines: [
      { kind: 'hunk', text: '@@ -3,3 +3,3 @@', oldLine: null, newLine: null },
      { kind: 'context', text: 'keep', oldLine: 3, newLine: 3 },
      { kind: 'del', text: 'old', oldLine: 4, newLine: null },
      { kind: 'add', text: 'new', oldLine: null, newLine: 4 },
    ],
    added: 1,
    removed: 1,
    truncated: false,
    toolCallId: 'tc',
    ...overrides,
  };
}

describe('diffWindowTexts', () => {
  it('reconstructs both revisions from the numbered rows', () => {
    expect(diffWindowTexts(diffCell())).toEqual({ oldText: 'keep\nold', newText: 'keep\nnew' });
  });
});

describe('VsCodeEditorActions', () => {
  it('opens http(s) links in the OS browser and refuses other schemes', async () => {
    mockState.reset();
    env.openedExternal.length = 0;
    const actions = new VsCodeEditorActions();

    await actions.openLink('https://example.com/a');
    await actions.openLink('http://example.com/a');
    await actions.openLink('file:///etc/passwd');
    await actions.openLink('javascript:alert(1)');

    expect(env.openedExternal.map((uri) => String(uri))).toEqual([
      'https://example.com/a',
      'http://example.com/a',
    ]);
    actions.dispose();
  });

  it('resolves relative paths through the injected resolver', async () => {
    mockState.reset();
    const actions = new VsCodeEditorActions({
      resolvePath: (candidate) => `/workspace/${candidate}`,
    });

    await actions.openFile('src/host/extension.ts', 3);

    expect(String((mockState.shownDocuments[0] as { uri?: unknown }).uri)).toBe(
      'file:///workspace/src/host/extension.ts',
    );
    actions.dispose();
  });

  it('opens a file at a line with a one-based to zero-based selection', async () => {
    mockState.reset();
    const actions = new VsCodeEditorActions();

    await actions.openFile('/work/src/a.ts', 42);

    expect(mockState.shownDocuments).toHaveLength(1);
    const shown = mockState.shownDocuments[0] as { options?: { selection?: { start?: { line?: number } } } };
    expect(shown.options?.selection?.start?.line).toBe(41);

    await actions.openFile('/work/src/a.ts', null);
    expect((mockState.shownDocuments[1] as { options?: unknown }).options).toEqual({ preview: true });
    actions.dispose();
  });

  it('serves a native diff through a virtual document scheme', async () => {
    mockState.reset();
    const actions = new VsCodeEditorActions();

    await actions.openDiff(diffCell());

    const provider = mockState.textDocumentContentProviders.get(DIFF_SCHEME);
    expect(provider).toBeDefined();
    const call = mockState.commandCalls.find((entry) => entry.command === 'vscode.diff');
    expect(call).toBeDefined();
    const [left, right] = call?.args ?? [];
    expect(provider?.provideTextDocumentContent(left as never)).toBe('keep\nold');
    expect(provider?.provideTextDocumentContent(right as never)).toBe('keep\nnew');
    // The URIs keep the file name so the diff editor titles are readable.
    expect(String(left)).toContain('a.ts');
    expect(String(right)).toContain('a.ts');

    actions.dispose();
    expect(mockState.disposables.some((disposable) => disposable.disposed)).toBe(true);
  });

  it('writes to the clipboard', async () => {
    mockState.reset();
    const actions = new VsCodeEditorActions();

    await actions.copyText('copied text');

    expect(mockState.clipboardWrites).toEqual(['copied text']);
    actions.dispose();
  });
});
