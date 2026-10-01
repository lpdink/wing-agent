/**
 * The dark-mode switch the token sheets read.
 *
 * Two sheets in `@wing-agent/ui` key their dark half on a **body attribute / class**,
 * not on a media query: the ported design tokens override on
 * `body[data-ds-dark-theme]` (Harness' mechanism) and the renderer's dark syntax
 * colours on `body.vscode-dark` (the class a VS Code webview body carries). A web
 * page has neither, so this module publishes them, and it is the *only* piece of
 * theme state this app mirrors in JS — everything else stays a `prefers-color-scheme`
 * rule (`styles.css` and `ui-theme.css`), with no JS copy to drift out of sync.
 *
 * It reads the media query once at boot (before React mounts, so the first painted
 * card is already dark) and then follows `change` events for as long as the page
 * lives; the returned disposer exists for tests and for symmetry with the rest of
 * the runtime's lifecycle code.
 */

/** The media query both the tokens and the app's own styles agree on. */
const DARK_QUERY = '(prefers-color-scheme: dark)';

/** The `classList` + attribute subset of `body` this module drives. */
export interface ColorSchemeBody {
  toggleAttribute(name: string, force: boolean): void;
  readonly classList: { toggle(token: string, force: boolean): boolean };
}

/** The `matchMedia` subset this module drives (a `MediaQueryList` satisfies it). */
export interface ColorSchemeQuery {
  readonly matches: boolean;
  addEventListener(type: 'change', listener: () => void): void;
  removeEventListener(type: 'change', listener: () => void): void;
}

/** Publish `dark` on the two attributes the token sheets switch on. */
export function applyColorScheme(body: ColorSchemeBody, dark: boolean): void {
  // `body[data-ds-dark-theme]` is what design-platform.css / base.css / shiki.css
  // override under…
  body.toggleAttribute('data-ds-dark-theme', dark);
  // …and `body.vscode-dark` is what `markdown.module.css`'s
  // `:global(body.vscode-dark) .codeToken` matches (the fix that replaced the
  // CSS-module-mangled selector; the web is the second host publishing that class).
  body.classList.toggle('vscode-dark', dark);
}

/**
 * Start following the OS colour scheme.
 *
 * @param body - the element carrying the attributes (defaults to `document.body`).
 *   Absent — a DOM-less lane, or a document without a body — nothing is published.
 * @returns A disposer that stops following the query.
 */
export function watchColorScheme(body?: ColorSchemeBody | null): () => void {
  const target = body ?? (typeof document === 'undefined' ? null : document.body);
  if (target === null || target === undefined) {
    return () => undefined;
  }
  const query = openDarkQuery();
  if (query === null) {
    return () => undefined;
  }
  const update = (): void => {
    applyColorScheme(target, query.matches);
  };
  update();
  query.addEventListener('change', update);
  return () => {
    query.removeEventListener('change', update);
  };
}

/** The dark-scheme query, or `null` where `matchMedia` does not exist. */
function openDarkQuery(): ColorSchemeQuery | null {
  if (typeof globalThis.matchMedia !== 'function') {
    return null;
  }
  return globalThis.matchMedia(DARK_QUERY);
}
