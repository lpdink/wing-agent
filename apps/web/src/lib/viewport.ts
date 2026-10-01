/**
 * Pure helpers for visual viewport / soft keyboard detection.
 *
 * Framework-free: no React imports. The React hook lives in
 * `src/app/useKeyboardOffset.ts`.
 */

/**
 * Pure: compute the keyboard offset from the visual viewport.
 *
 * `null` means "the viewport API is not available" (desktop / old browser).
 * Positive number = keyboard height in px, 0 = no keyboard.
 */
export function computeKeyboardOffset(
  visualViewport: VisualViewport | null,
  layoutViewport: { readonly innerHeight: number },
): number {
  if (visualViewport === null) {
    return 0;
  }
  const diff = layoutViewport.innerHeight - visualViewport.height;
  // A tiny difference (< 80 px) is usually the address bar collapsing, not a
  // keyboard. On phones, a keyboard is at least ~200 px tall.
  return diff > 80 ? diff : 0;
}

/**
 * For tests: a visual viewport mock that simulates keyboard open/close.
 */
export function mockVisualViewport(keyboardHeight: number, fullHeight = 844): VisualViewport {
  return {
    height: fullHeight - keyboardHeight,
    width: 390,
    offsetTop: 0,
    offsetLeft: 0,
    onresize: null,
    onscroll: null,
    pageTop: 0,
    pageLeft: 0,
    scale: 1,
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => true,
  } as VisualViewport;
}
