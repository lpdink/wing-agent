/**
 * Local interaction state of the renderer.
 *
 * Everything here is webview-local by contract (see `src/shared/bridge.ts`): the
 * host never learns which thoughts the user expanded. Two kinds of state:
 *
 * - **Collapse overrides** live in a module-level map, not in component state, so
 *   a tab switch (which unmounts the transcript) does not forget the user's
 *   choices. `auto` is just "no override recorded".
 * - **Copy feedback** is component-local: VS Code shows the copied state for
 *   1200ms (`workbench/contrib/chat/browser/actions/chatCopyActions.ts:29`
 *   `copyFeedbackDuration`), then reverts.
 */

import { useCallback, useEffect, useRef, useState } from 'react';

/** Explicit expand/collapse choice per cell, keyed by cell id. */
const collapseOverrides = new Map<string, boolean>();

/** VS Code's copy-confirmation dwell time, in ms. */
export const COPY_FEEDBACK_MS = 1200;

export interface CollapsibleState {
  /** Whether the content is currently hidden. */
  readonly collapsed: boolean;
  readonly toggle: () => void;
}

/**
 * A collapsible section whose default follows the cell's streaming state until
 * the user overrides it.
 *
 * `autoCollapsed` is re-read on every render, so "thinking collapses when the
 * stream ends" needs no effect — the cell re-renders when `streaming` flips and
 * the default takes over, unless an override exists.
 */
export function useCollapsible(key: string, autoCollapsed: boolean): CollapsibleState {
  const [override, setOverride] = useState<boolean | null>(() => collapseOverrides.get(key) ?? null);
  const collapsed = override ?? autoCollapsed;

  const toggle = useCallback(() => {
    setOverride((previous) => {
      const next = !(previous ?? autoCollapsed);
      collapseOverrides.set(key, next);
      return next;
    });
  }, [key, autoCollapsed]);

  return { collapsed, toggle };
}

/** Test hook: forget every remembered choice (the app store resets do not own this map). */
export function resetCollapseOverrides(): void {
  collapseOverrides.clear();
}

/** `[copied, reportCopied]` — flips to true for {@link COPY_FEEDBACK_MS}. */
export function useCopyFeedback(): readonly [boolean, () => void] {
  const [copied, setCopied] = useState(false);
  const timer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (timer.current !== null) {
        window.clearTimeout(timer.current);
      }
    },
    [],
  );

  const reportCopied = useCallback(() => {
    setCopied(true);
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
    }
    timer.current = window.setTimeout(() => {
      timer.current = null;
      setCopied(false);
    }, COPY_FEEDBACK_MS);
  }, []);

  return [copied, reportCopied];
}
