/**
 * Tests for touch gesture helpers.
 */

import { describe, expect, it } from 'vitest';

import { isDownwardSwipe, type SwipeConfig } from '../src/lib/gesture';

const CONFIG: Required<SwipeConfig> = { threshold: 80, maxLateralDrift: 40, maxDurationMs: 500 };

describe('isDownwardSwipe', () => {
  it('detects a clear downward swipe', () => {
    expect(isDownwardSwipe({ x: 100, y: 200, time: 1000 }, { x: 105, y: 300, time: 1300 }, CONFIG)).toBe(
      true,
    );
  });

  it('rejects upward movement', () => {
    expect(isDownwardSwipe({ x: 100, y: 300, time: 1000 }, { x: 105, y: 200, time: 1300 }, CONFIG)).toBe(
      false,
    );
  });

  it('rejects a short vertical distance (below threshold)', () => {
    expect(isDownwardSwipe({ x: 100, y: 200, time: 1000 }, { x: 105, y: 250, time: 1300 }, CONFIG)).toBe(
      false,
    );
  });

  it('rejects excessive horizontal drift', () => {
    expect(isDownwardSwipe({ x: 100, y: 200, time: 1000 }, { x: 200, y: 300, time: 1300 }, CONFIG)).toBe(
      false,
    );
  });

  it('rejects a gesture that takes too long', () => {
    expect(isDownwardSwipe({ x: 100, y: 200, time: 1000 }, { x: 105, y: 300, time: 2000 }, CONFIG)).toBe(
      false,
    );
  });

  it('accepts a gesture just at the threshold', () => {
    expect(isDownwardSwipe({ x: 100, y: 200, time: 1000 }, { x: 100, y: 280, time: 1499 }, CONFIG)).toBe(
      true,
    );
  });

  it('rejects a gesture just below the threshold', () => {
    expect(isDownwardSwipe({ x: 100, y: 200, time: 1000 }, { x: 100, y: 279, time: 1001 }, CONFIG)).toBe(
      false,
    );
  });

  it('handles custom config', () => {
    const tight: Required<SwipeConfig> = { threshold: 30, maxLateralDrift: 10, maxDurationMs: 200 };
    expect(isDownwardSwipe({ x: 100, y: 100, time: 500 }, { x: 105, y: 140, time: 600 }, tight)).toBe(true);
  });
});
