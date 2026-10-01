// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/tests/diff-block.client.spec.tsx
// Modified for Wing: the fixtures are Wing `DiffCellModel`s (the host already
// computed and windowed the rows), so the assertions cover the row mapping, the
// counters, the windowed `…` marker, folding and copy — not patch computation.

import { act, fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import type { DiffCellModel, DiffLineModel } from '@wing-agent/session';

import { supportsHighlighting } from '../../src/chat/markdown/highlight';
import { DEFAULT_DIFF_MAX_LINES, DiffBlock, LANGUAGE_BY_EXTENSION } from '../../src/tool/DiffBlock';
import type { DiffBlockLabels } from '../../src/tool/DiffBlock';

const LABELS: DiffBlockLabels = {
  codeLabel: 'Code',
  wrapLabel: 'Wrap lines',
  unwrapLabel: 'Do not wrap',
  copy: 'Copy',
  copied: 'Copied',
  collapseAria: 'Collapse diff',
  collapse: 'Collapse',
  expandAria: (hidden) => `Show ${hidden} more lines`,
  expand: (hidden) => `… ${hidden} more lines`,
};

/** A cell whose rows are just the given lines. */
function cell(lines: readonly DiffLineModel[], overrides: Partial<DiffCellModel> = {}): DiffCellModel {
  return {
    kind: 'diff',
    id: 'd1',
    createdAt: 0,
    path: 'src/chat/Cells.tsx',
    oldStartLine: 1,
    newStartLine: 1,
    added: lines.filter((line) => line.kind === 'add').length,
    removed: lines.filter((line) => line.kind === 'del').length,
    truncated: false,
    toolCallId: null,
    lines,
    ...overrides,
  };
}

const context = (text: string, line: number): DiffLineModel => ({
  kind: 'context',
  text,
  oldLine: line,
  newLine: line,
});
const add = (text: string, line: number): DiffLineModel => ({
  kind: 'add',
  text,
  oldLine: null,
  newLine: line,
});
const del = (text: string, line: number): DiffLineModel => ({
  kind: 'del',
  text,
  oldLine: line,
  newLine: null,
});
const hunk = (text: string): DiffLineModel => ({ kind: 'hunk', text, oldLine: null, newLine: null });

function rowsOf(container: HTMLElement): { kind: string | null; text: string }[] {
  return [...container.querySelectorAll('[data-diff-kind]')].map((row) => ({
    kind: row.getAttribute('data-diff-kind'),
    text: row.textContent ?? '',
  }));
}

function stubClipboard(writeText: (text: string) => Promise<void>): void {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
}

function filler(count: number): DiffLineModel[] {
  return Array.from({ length: count }, (_value, index) => context(`row ${index + 1}`, index + 1));
}

describe('DiffBlock structure', () => {
  it('renders one row per cell line, tagging each row with its kind', () => {
    const { container } = render(
      <DiffBlock
        cell={cell([
          hunk('@@ -1,3 +1,4 @@'),
          context('const a = 1;', 1),
          del('const b = 2;', 2),
          add('const b = 3;', 2),
        ])}
        labels={LABELS}
      />,
    );
    expect(rowsOf(container)).toEqual([
      { kind: 'hunk', text: '@@ -1,3 +1,4 @@' },
      { kind: 'context', text: 'const a = 1;' },
      { kind: 'del', text: 'const b = 2;' },
      { kind: 'add', text: 'const b = 3;' },
    ]);
  });

  it('shows the path, the language label and the +n/−m counters in the toolbar', () => {
    render(<DiffBlock cell={cell([del('old', 1), add('new', 1), add('more', 2)])} labels={LABELS} />);
    expect(screen.getByText('src/chat/Cells.tsx')).toBeInTheDocument();
    expect(screen.getByText('tsx')).toBeInTheDocument();
    expect(screen.getByText('+2')).toBeInTheDocument();
    expect(screen.getByText('−1')).toBeInTheDocument();
  });

  it('falls back to the generic language label for an unknown extension', () => {
    render(<DiffBlock cell={cell([add('x', 1)], { path: 'notes/README' })} labels={LABELS} />);
    expect(screen.getByText('Code')).toBeInTheDocument();
  });

  it('maps every extension to a grammar the shared highlighter ships', () => {
    // The invariant behind the table: a hint `supportsHighlighting` rejects is a
    // silent label failure — the card falls back to "Code" while its own comment
    // claims the language. `.jsx` / `.md` used to be exactly that (the bundled
    // grammars register no `jsx` alias and load no markdown grammar); the table no
    // longer lists them, and this case keeps any future entry honest.
    const dead = Object.entries(LANGUAGE_BY_EXTENSION)
      .filter(([, language]) => !supportsHighlighting(language))
      .map(([extension, language]) => `${extension} -> ${language}`);
    expect(dead).toEqual([]);
    expect(Object.keys(LANGUAGE_BY_EXTENSION).length).toBeGreaterThan(10);
  });

  it('marks a windowed payload with a final … row', () => {
    const { container } = render(
      <DiffBlock cell={cell([context('a', 1)], { truncated: true })} labels={LABELS} />,
    );
    expect(rowsOf(container)).toEqual([
      { kind: 'context', text: 'a' },
      { kind: 'gap', text: '…' },
    ]);
  });

  it('renders nothing for a cell with no rows', () => {
    const { container } = render(<DiffBlock cell={cell([])} labels={LABELS} />);
    expect(container.firstChild).toBeNull();
  });

  it('keeps hunk headers out of the change colours', () => {
    const { container } = render(<DiffBlock cell={cell([hunk('@@ -1 +1 @@')])} labels={LABELS} />);
    const row = container.querySelector('[data-diff-kind="hunk"]');
    expect(row?.className).toContain('hunk');
    expect(row?.className).not.toContain('add');
    expect(row?.className).not.toContain('del');
  });
});

describe('DiffBlock height cap', () => {
  it('slices head and tail over the cap, then expands on click', () => {
    const { container } = render(<DiffBlock cell={cell(filler(20))} labels={LABELS} maxLines={4} />);
    expect(rowsOf(container).map((row) => row.text)).toEqual(['row 1', 'row 2', 'row 19', 'row 20']);
    const toggle = screen.getByRole('button', { name: 'Show 16 more lines' });
    fireEvent.click(toggle);
    expect(rowsOf(container)).toHaveLength(20);
    fireEvent.click(screen.getByRole('button', { name: 'Collapse diff' }));
    expect(rowsOf(container)).toHaveLength(4);
  });

  it('shows no fold control at or under the cap', () => {
    const { container } = render(<DiffBlock cell={cell(filler(DEFAULT_DIFF_MAX_LINES))} labels={LABELS} />);
    expect(container.querySelector('[aria-expanded]')).toBeNull();
  });
});

describe('DiffBlock copy and wrap', () => {
  it('copies the path-led diff text with row prefixes and flips the label', async () => {
    vi.useFakeTimers();
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    render(
      <DiffBlock
        cell={cell([hunk('@@'), context('same', 1), del('old', 2), add('new', 2)], {
          path: 'src/a.ts',
          truncated: true,
        })}
        labels={LABELS}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith('src/a.ts\n@@\n  same\n- old\n+ new\n…');
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole('button', { name: 'Copied' })).toBeInTheDocument();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(screen.getByRole('button', { name: 'Copy' })).toBeInTheDocument();
    vi.useRealTimers();
  });

  it('copies the folded rows too', () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    render(<DiffBlock cell={cell(filler(20))} labels={LABELS} maxLines={4} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    const payload = writeText.mock.calls[0]?.[0] as string;
    expect(payload.split('\n')).toHaveLength(21);
    expect(payload).toContain('  row 10');
  });

  it('keeps the label on a refused clipboard write', async () => {
    stubClipboard(vi.fn().mockRejectedValue(new Error('denied')));
    render(<DiffBlock cell={cell([add('x', 1)])} labels={LABELS} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole('button', { name: 'Copy' })).toBeInTheDocument();
  });

  it('toggles the wrap attribute the stylesheet keys on', () => {
    const { container } = render(<DiffBlock cell={cell([add('x', 1)])} labels={LABELS} />);
    const root = container.firstElementChild;
    expect(root?.getAttribute('data-code-wrap')).toBe('false');
    fireEvent.click(screen.getByRole('button', { name: 'Wrap lines' }));
    expect(root?.getAttribute('data-code-wrap')).toBe('true');
  });
});
