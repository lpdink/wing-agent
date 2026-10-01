/**
 * Tests for the visual viewport / soft keyboard helpers.
 */

import { describe, expect, it } from 'vitest';

import { computeKeyboardOffset, mockVisualViewport } from '../src/lib/viewport';

describe('computeKeyboardOffset', () => {
  const LAYOUT = { innerHeight: 844 }; // iPhone 14 height

  it('returns 0 when visualViewport is null (no keyboard API)', () => {
    expect(computeKeyboardOffset(null, LAYOUT)).toBe(0);
  });

  it('returns 0 when the viewport change is small (address bar, not keyboard)', () => {
    // 40 px difference is smaller than the 80 px threshold
    const vv = mockVisualViewport(40, LAYOUT.innerHeight);
    expect(computeKeyboardOffset(vv, LAYOUT)).toBe(0);
  });

  it('returns the keyboard height when the keyboard opens', () => {
    // 300 px keyboard on a 844 px screen
    const vv = mockVisualViewport(300, LAYOUT.innerHeight);
    expect(computeKeyboardOffset(vv, LAYOUT)).toBe(300);
  });

  it('returns the exact difference for a tall keyboard', () => {
    const vv = mockVisualViewport(450, LAYOUT.innerHeight);
    expect(computeKeyboardOffset(vv, LAYOUT)).toBe(450);
  });

  it('returns 0 when the keyboard closes (height equals layout)', () => {
    const vv = mockVisualViewport(0, LAYOUT.innerHeight);
    expect(computeKeyboardOffset(vv, LAYOUT)).toBe(0);
  });

  it('handles the threshold boundary exactly', () => {
    // 80 px is exactly the threshold — should be 0 (not > 80)
    const vv = mockVisualViewport(80, LAYOUT.innerHeight);
    expect(computeKeyboardOffset(vv, LAYOUT)).toBe(0);
  });

  it('handles differences just over the threshold', () => {
    // 81 px is just over the threshold
    const vv = mockVisualViewport(81, LAYOUT.innerHeight);
    expect(computeKeyboardOffset(vv, LAYOUT)).toBe(81);
  });
});

describe('mockVisualViewport', () => {
  it('creates a VisualViewport-like object with the correct height', () => {
    const vv = mockVisualViewport(300, 844);
    expect(vv.height).toBe(544); // 844 - 300
    expect(vv.width).toBe(390);
    expect(vv.scale).toBe(1);
  });

  it('creates a full-height viewport when keyboard is closed', () => {
    const vv = mockVisualViewport(0, 844);
    expect(vv.height).toBe(844);
  });
});
