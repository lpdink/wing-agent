import { describe, expect, it } from 'vitest';

import { FORBIDDEN_PORT, assertPortAllowed } from '../tools/shot/server';

/**
 * The fixture server's one hard rule: never bind the port the user's own gateway
 * runs on. `listen(0)` never returns 32523 in practice (it sits below the ephemeral
 * range on every real system), so this is an invariant guard, not a live filter —
 * exactly why it is worth a unit test rather than a comment (review r1 N2).
 */

describe('assertPortAllowed', () => {
  it('refuses the user’s gateway port', () => {
    expect(FORBIDDEN_PORT).toBe(32_523);
    expect(() => {
      assertPortAllowed(FORBIDDEN_PORT);
    }).toThrow(/refusing to bind/);
  });

  it('allows every other port', () => {
    for (const port of [1, 80, 5_173, 32_522, 32_524, 65_535]) {
      expect(() => {
        assertPortAllowed(port);
      }).not.toThrow();
    }
  });
});
