// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/tests (the head/tail slicing is asserted
// through terminal-block.client.spec.tsx and diff-block.client.spec.tsx there)
// Modified for Wing: extracted into the arithmetic the two cards share, with the
// boundary cases spelled out.

import { describe, expect, it } from 'vitest';

import { headTailCap } from '../../src/tool/head-tail-cap';

describe('headTailCap', () => {
  it('reports nothing hidden at or under the cap', () => {
    expect(headTailCap(4, 4, false)).toEqual({ hidden: 0, capped: false, headLines: 2, tailLines: 2 });
    expect(headTailCap(3, 4, false).capped).toBe(false);
    expect(headTailCap(0, 16, false)).toEqual({ hidden: -16, capped: false, headLines: 8, tailLines: 8 });
  });

  it('slices head and tail over the cap with the remainder hidden', () => {
    // maxLines 4 → head 2, tail 2, 6 hidden.
    expect(headTailCap(10, 4, false)).toEqual({ hidden: 6, capped: true, headLines: 2, tailLines: 2 });
  });

  it('gives the extra row to the head when the cap is odd', () => {
    // ceil(5 / 2) = 3 head rows, 2 tail rows.
    expect(headTailCap(10, 5, false)).toEqual({ hidden: 5, capped: true, headLines: 3, tailLines: 2 });
  });

  it('leaves an odd cap of one as head-only', () => {
    expect(headTailCap(5, 1, false)).toEqual({ hidden: 4, capped: true, headLines: 1, tailLines: 0 });
  });

  it('reports the hidden count even while expanded', () => {
    expect(headTailCap(10, 4, true)).toEqual({ hidden: 6, capped: false, headLines: 2, tailLines: 2 });
  });
});
