import type { MenuItemConstructorOptions } from 'electron';

/**
 * The shell's application menu.
 *
 * Deliberately the smallest useful set: the standard macOS roles (app / edit /
 * view / window). No File menu (there is nothing to open — sessions live in the
 * gateway), no tray, no custom items: any menu entry without a real function
 * would be a decorative control, which the project's quality bar forbids.
 *
 * Other platforms get no menu at all (`src/main.ts` calls
 * `Menu.setApplicationMenu(null)`), matching the July branch's behaviour.
 *
 * The Electron import above is a **type-only** import — this module stays plain
 * data so `tests/menu.test.ts` can assert it without an Electron runtime.
 */
export function buildMenuTemplate(isMac: boolean): MenuItemConstructorOptions[] {
  if (!isMac) {
    return [];
  }
  return [{ role: 'appMenu' }, { role: 'editMenu' }, { role: 'viewMenu' }, { role: 'windowMenu' }];
}
