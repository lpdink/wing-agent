/**
 * Keyboard navigation for the shell's lists (panels and the `/` candidates).
 *
 * Modelled on VS Code's list widgets (`base/browser/ui/list/listWidget.ts`):
 * `Up` / `Down` move the highlight, `Home` / `End` jump to the ends, `Enter`
 * activates, `Escape` closes. The highlight never wraps (VS Code does not wrap
 * either) and always lands on a *selectable* row — group headers and the branch
 * panel's "current point" row are skipped.
 *
 * The hook returns `index` (a row index, or `-1` when nothing is selectable) plus
 * the props the caller spreads on the list container: `aria-activedescendant`
 * points at the highlighted row id, which is the ARIA pattern the suggest widget
 * uses (`suggestWidget.ts:235` gives its rows `role='option'`).
 */

import { useCallback, useEffect, useRef, useState } from 'react';
import type { KeyboardEvent, RefObject } from 'react';

/** One row of a navigable list, as far as navigation is concerned. */
export interface ListRow {
  readonly selectable: boolean;
}

export interface ListNavOptions {
  /**
   * Where to start (row index); the first selectable row when omitted.
   *
   * Explicitly `| undefined` so callers can forward an optional value under
   * `exactOptionalPropertyTypes`.
   */
  readonly initialIndex?: number | undefined;
  readonly onSelect: (index: number) => void;
  /** Called on Escape. When omitted, Escape is left to the caller. */
  readonly onEscape?: (() => void) | undefined;
}

export interface ListNav {
  /** Highlighted row index (`-1` when the list has no selectable row). */
  readonly index: number;
  /** Spread on the list container (`role="listbox"`). */
  readonly listProps: {
    readonly 'aria-activedescendant': string | undefined;
  };
  /** Spread on the list container's `onKeyDown`. */
  readonly onKeyDown: (event: KeyboardEvent<HTMLElement>) => void;
  readonly setIndex: (index: number) => void;
  /** Activate a row (click). Ignores non-selectable rows. */
  readonly activate: (index: number) => void;
}

/** `id` for the option element of `index` (also used as `aria-activedescendant`). */
export function optionId(listId: string, index: number): string {
  return `${listId}-option-${index}`;
}

export function useListNav(rows: readonly ListRow[], listId: string, options: ListNavOptions): ListNav {
  const [rawIndex, setRawIndex] = useState(() => options.initialIndex ?? -1);

  // Callbacks are re-created on every render by callers (inline arrows); keeping
  // them in refs makes the handlers below stable without lying about deps.
  const optionsRef = useRef(options);
  optionsRef.current = options;
  const rowsRef = useRef(rows);
  rowsRef.current = rows;

  const selectable = useCallback((index: number): boolean => rowsRef.current[index]?.selectable ?? false, []);
  const firstSelectable = useCallback((): number => {
    const table = rowsRef.current;
    for (let index = 0; index < table.length; index += 1) {
      if (table[index]?.selectable === true) {
        return index;
      }
    }
    return -1;
  }, []);

  // Derived, not stored: when the list shrinks (a filter, a catalog refresh) the
  // stale index must not survive, and no effect is needed to enforce that.
  const index = selectable(rawIndex) ? rawIndex : firstSelectable();

  const move = useCallback(
    (step: number): void => {
      const table = rowsRef.current;
      const from = selectable(rawIndex) ? rawIndex : firstSelectable();
      if (from < 0) {
        return;
      }
      for (let candidate = from + step; candidate >= 0 && candidate < table.length; candidate += step) {
        if (table[candidate]?.selectable === true) {
          setRawIndex(candidate);
          return;
        }
      }
    },
    [firstSelectable, rawIndex, selectable],
  );

  const activate = useCallback(
    (target: number): void => {
      if (selectable(target)) {
        setRawIndex(target);
        optionsRef.current.onSelect(target);
      }
    },
    [selectable],
  );

  const onKeyDown = useCallback(
    (event: KeyboardEvent<HTMLElement>): void => {
      switch (event.key) {
        case 'ArrowDown':
          event.preventDefault();
          move(1);
          return;
        case 'ArrowUp':
          event.preventDefault();
          move(-1);
          return;
        case 'Home': {
          event.preventDefault();
          const first = firstSelectable();
          if (first >= 0) {
            setRawIndex(first);
          }
          return;
        }
        case 'End': {
          event.preventDefault();
          const table = rowsRef.current;
          for (let candidate = table.length - 1; candidate >= 0; candidate -= 1) {
            if (table[candidate]?.selectable === true) {
              setRawIndex(candidate);
              return;
            }
          }
          return;
        }
        case 'Enter':
        case ' ': {
          if (index >= 0) {
            event.preventDefault();
            activate(index);
          }
          return;
        }
        case 'Escape': {
          const escape = optionsRef.current.onEscape;
          if (escape !== undefined) {
            event.preventDefault();
            event.stopPropagation();
            escape();
          }
          return;
        }
        default:
          return;
      }
    },
    [activate, firstSelectable, index, move],
  );

  return {
    index,
    listProps: { 'aria-activedescendant': index >= 0 ? optionId(listId, index) : undefined },
    onKeyDown,
    setIndex: setRawIndex,
    activate,
  };
}

/**
 * New scroll offset that brings a row fully into view.
 *
 * Pure so the arithmetic is testable without a layout engine (jsdom has none, and
 * `scrollIntoView` does not exist there at all). The rule is VS Code's list
 * `reveal` behavior: scroll the *least* amount — a row above the viewport moves
 * the viewport up to the row, a row below it moves down just enough, and a row
 * already visible does not move anything.
 */
export function revealScrollTop(
  scrollTop: number,
  viewportHeight: number,
  rowTop: number,
  rowHeight: number,
): number {
  if (rowTop < scrollTop) {
    return rowTop;
  }
  const rowBottom = rowTop + rowHeight;
  const viewportBottom = scrollTop + viewportHeight;
  if (rowBottom > viewportBottom) {
    return scrollTop + (rowBottom - viewportBottom);
  }
  return scrollTop;
}

/**
 * Keep the highlighted row of a list visible while the keyboard moves through it.
 *
 * `listId` is the id prefix the rows were rendered with (`optionId`); the row is
 * looked up by its own id, so grouped lists (the model panel) work unchanged.
 */
export function useRevealIndex(listRef: RefObject<HTMLElement | null>, listId: string, index: number): void {
  useEffect(() => {
    const list = listRef.current;
    if (list === null || index < 0) {
      return;
    }
    const row = list.querySelector<HTMLElement>(`#${optionId(listId, index)}`);
    if (row === null) {
      return;
    }
    const next = revealScrollTop(list.scrollTop, list.clientHeight, row.offsetTop, row.offsetHeight);
    if (next !== list.scrollTop) {
      list.scrollTop = next;
    }
  }, [listRef, listId, index]);
}
