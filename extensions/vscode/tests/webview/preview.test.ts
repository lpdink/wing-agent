import { act } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

/**
 * Preview harness smoke test.
 *
 * `preview/main.tsx` is the harness steps 05/06 use to look at renderer changes
 * without VS Code; if its wiring breaks, the harness silently becomes an empty
 * page. Importing it here mounts the real app against the scripted host in jsdom
 * and asserts the toolbar exists.
 */

describe('preview harness', () => {
  it('mounts the real app and builds its toolbar', async () => {
    document.body.innerHTML = '<div id="preview-root"></div><div id="root"></div>';

    await act(async () => {
      await import('../../preview/main');
    });

    const root = document.getElementById('root');
    expect(root?.querySelector('[data-cell-kind="assistant"]')).not.toBeNull();
    expect(root?.querySelector('[data-cell-id="tool-1"]')).not.toBeNull();

    const toolbar = document.getElementById('preview-root');
    const buttons = [...(toolbar?.querySelectorAll('button') ?? [])].map((button) => button.textContent);
    // The toolbar is the harness' interface to the protocol paths: streaming,
    // resync, toasts, and (step 05) the shell's panels and overlays.
    expect(buttons).toEqual([
      'Stream turn',
      'Break stream',
      'Toast',
      'Catalogs',
      'Model panel',
      'Notice',
      'Close overlays',
      'Reload',
    ]);
    const fixtures = [...(toolbar?.querySelectorAll('option') ?? [])].map((option) => option.textContent);
    expect(fixtures).toContain('shell (idle)');
    expect(fixtures).toContain('shell (working + queue)');
    expect(fixtures).toContain('shell (model picker open)');
    expect(fixtures).toContain('two tabs');
    expect(fixtures.length).toBeGreaterThanOrEqual(9);
  });
});
