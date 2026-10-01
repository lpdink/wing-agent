/**
 * jsdom setup for the React shell tests: unmount every rendered tree between
 * tests so a leftover runtime (with its timers) cannot leak into the next one.
 */

import { cleanup } from '@testing-library/react';
import { afterEach } from 'vitest';

afterEach(() => {
  cleanup();
});
