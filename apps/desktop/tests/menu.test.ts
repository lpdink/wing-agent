import { describe, expect, it } from 'vitest';

import { buildMenuTemplate } from '../src/menu';

/**
 * The menu is deliberately minimal (no decorative entries). Everything it does
 * expose must be a role Electron itself implements.
 */

describe('buildMenuTemplate', () => {
  it('gives macOS the four standard roles', () => {
    const template = buildMenuTemplate(true);
    expect(template.map((item) => item.role)).toEqual(['appMenu', 'editMenu', 'viewMenu', 'windowMenu']);
  });

  it('gives other platforms no menu at all', () => {
    expect(buildMenuTemplate(false)).toEqual([]);
  });
});
