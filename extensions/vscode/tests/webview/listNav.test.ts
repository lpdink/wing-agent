import { describe, expect, it } from 'vitest';

import { revealScrollTop } from '../../src/webview/app/panels/listNav';

/**
 * `revealScrollTop` — the arithmetic behind "the highlight never leaves the
 * visible window" (checkpoint② #2).
 *
 * Pure on purpose: jsdom has no layout and no `scrollIntoView`, so the only
 * honest way to test list scrolling is to test the computation and let the
 * component test feed it real numbers (see `composer.test.tsx`).
 */
describe('revealScrollTop', () => {
  it('does not move a row that is already fully visible', () => {
    expect(revealScrollTop(100, 300, 150, 20)).toBe(100);
    expect(revealScrollTop(100, 300, 100, 20)).toBe(100);
    expect(revealScrollTop(100, 300, 380, 20)).toBe(100);
  });

  it('scrolls up just enough for a row above the viewport', () => {
    expect(revealScrollTop(200, 300, 50, 20)).toBe(50);
  });

  it('scrolls down just enough for a row below the viewport', () => {
    // Viewport is 300..600; the row ends at 640 → move down by 40.
    expect(revealScrollTop(300, 300, 620, 20)).toBe(340);
  });

  it('handles a row taller than the viewport (top wins)', () => {
    // Row 300..600 with a 0..300 viewport: below → scroll down to its top.
    expect(revealScrollTop(0, 300, 300, 300)).toBe(300);
    // Same row, viewport 301..601: it started above → scroll back up to its top.
    expect(revealScrollTop(301, 300, 300, 300)).toBe(300);
  });
});
