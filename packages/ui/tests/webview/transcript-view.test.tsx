import { render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it } from 'vitest';

import type { SessionViewModel } from '@wing-agent/session';
import { TranscriptView } from '../../src/app/TranscriptView';
import { FIXTURE_EPOCH, makeFixtureSession } from '../../src/testing/fixtures';

/**
 * The `renderCell` seam.
 *
 * `apps/web` (step 08b) draws its own cells — the shared code/tool/ask cards the
 * editor's renderer does not use yet — while keeping *this* scroller. The seam is
 * therefore an agreement between two packages, and it is pinned here: with no
 * `renderCell` the rows must stay exactly the ones `CellView` produces (the VS Code
 * webview's rendering), and with one the shell's renderer must be the *only* thing
 * drawing cells.
 *
 * The scroller chrome (the transcript container, the scroll-to-bottom button) is
 * asserted through the real component, not a mock, so a future refactor that drops
 * the override silently goes red.
 */

afterEach(() => {
  document.body.innerHTML = '';
});

function fixture(): SessionViewModel {
  return makeFixtureSession({
    sessionId: 'session-a',
    cells: [
      { kind: 'user', id: 'u1', createdAt: FIXTURE_EPOCH, text: 'hello', state: 'accepted' },
      { kind: 'assistant', id: 'a1', createdAt: FIXTURE_EPOCH, text: 'hi there', streaming: false },
    ],
  });
}

describe('TranscriptView', () => {
  it('renders the package’s own cells when no renderer is supplied', () => {
    render(<TranscriptView session={fixture()} />);

    // The VS Code webview's shape: one row per cell, drawn by `CellView`.
    expect(screen.getByTestId('transcript-content').children).toHaveLength(2);
    expect(document.querySelector('[data-cell-kind="user"]')?.textContent).toContain('hello');
    expect(document.querySelector('[data-cell-kind="assistant"]')?.textContent).toContain('hi there');
  });

  it('hands every cell to the shell’s renderer when one is supplied', () => {
    const seen: string[] = [];
    const { container } = render(
      <TranscriptView
        session={fixture()}
        renderCell={(cell, sessionId) => {
          seen.push(`${sessionId}:${cell.id}`);
          return <div data-testid={`custom-${cell.id}`} data-cell-kind={cell.kind} />;
        }}
      />,
    );

    expect(seen).toEqual(['session-a:u1', 'session-a:a1']);
    expect(screen.getByTestId('transcript-content').children).toHaveLength(2);
    // Nothing of the package's own row markup is left behind…
    expect(container.querySelector('[data-cell-id]')).toBeNull();
    // …and the scroller (the reason this seam exists) is still the package's.
    expect(screen.getByTestId('transcript-content')).toBeTruthy();
    expect(container.querySelector('[data-testid="scroll-to-bottom"]')).toBeNull();
  });

  it('keeps the empty state for an unhydrated session, renderer or not', () => {
    render(<TranscriptView session={null} renderCell={() => <div />} />);

    expect(screen.getByTestId('empty-state')).toBeTruthy();
  });
});
