/**
 * `/rewind` and `/fork` panel: the branch targets of the active session.
 *
 * The gateway builds the list in `ContextManager.get_branch_targets()`
 * (`libs/core/wing/context_manager.py:898-926`): every user message (truncated to
 * 100 chars), every compaction node, and — appended last — a sentinel entry
 * `{ uuid: 'current', content: '(current)' }` that stands for "the newest state".
 * That sentinel is rendered as the **current point**: not selectable (there is
 * nothing to rewind to) but always visible, so the list reads as a timeline.
 *
 * The same list serves both commands; only the wording and the intent differ
 * (`runPromptCommand` with `/rewind` or `/fork`, matching the TUI where both are
 * commands carrying a message uuid).
 */

import { useEffect, useMemo, useRef } from 'react';
import type { ReactElement } from 'react';

import type { BranchCatalogModel, BranchTargetModel } from '../../../shared';
import { BRANCH_CURRENT_UUID } from '../../../shared';
import { postToHost } from '../../bridge/channel';
import styles from '../../styles/panels.module.css';
import { PanelEmpty, PanelShell } from './PanelShell';
import { optionId, useListNav } from './listNav';

/** Which command opened the panel. */
export type BranchMode = 'rewind' | 'fork';

export interface BranchRow {
  readonly target: BranchTargetModel;
  /** The gateway's `current` sentinel — shown, but not selectable. */
  readonly current: boolean;
}

export interface BranchPanelProps {
  readonly sessionId: string;
  readonly mode: BranchMode;
  readonly catalog: BranchCatalogModel | null;
  readonly onClose: () => void;
}

const TITLES: Record<BranchMode, string> = {
  rewind: 'Rewind to message',
  fork: 'Fork from message',
};

/** Split the gateway's list into selectable targets and the current-point row. */
export function branchRows(catalog: BranchCatalogModel | null): readonly BranchRow[] {
  if (catalog === null) {
    return [];
  }
  return catalog.targets.map((target) => ({ target, current: target.uuid === BRANCH_CURRENT_UUID }));
}

export function BranchPanel({ sessionId, mode, catalog, onClose }: BranchPanelProps): ReactElement {
  const rows = useMemo(() => branchRows(catalog), [catalog]);
  const listRef = useRef<HTMLDivElement>(null);
  const command = mode === 'rewind' ? '/rewind' : '/fork';

  // Default highlight: the newest selectable target (the one right before the
  // current point) — rewind/fork almost always mean "back to what I just said".
  const initialIndex = useMemo(() => {
    for (let index = rows.length - 1; index >= 0; index -= 1) {
      if (rows[index]?.current !== true) {
        return index;
      }
    }
    return undefined;
  }, [rows]);

  const select = (index: number): void => {
    const row = rows[index];
    if (row === undefined || row.current) {
      return;
    }
    postToHost({ type: 'runPromptCommand', sessionId, name: command, argsText: row.target.uuid });
    onClose();
  };

  const nav = useListNav(
    rows.map((row) => ({ selectable: !row.current })),
    'branch-panel',
    { initialIndex, onSelect: select, onEscape: onClose },
  );

  useEffect(() => {
    listRef.current?.focus();
  }, []);

  return (
    <PanelShell
      title={TITLES[mode]}
      testId="branch-panel"
      onClose={onClose}
      hint="↑↓ navigate · Enter apply · Esc close"
    >
      {rows.length === 0 ? (
        <PanelEmpty text="Branch targets are not available yet." />
      ) : (
        <div
          className={styles.list}
          role="listbox"
          aria-label={TITLES[mode]}
          tabIndex={-1}
          ref={listRef}
          data-testid="branch-panel-list"
          data-mode={mode}
          onKeyDown={nav.onKeyDown}
          {...nav.listProps}
        >
          {rows.map((row, index) => (
            <div
              key={row.target.uuid}
              className={styles.option}
              role="option"
              id={optionId('branch-panel', index)}
              aria-selected={index === nav.index}
              aria-disabled={row.current ? 'true' : undefined}
              data-highlighted={index === nav.index ? 'true' : 'false'}
              data-current={row.current ? 'true' : 'false'}
              data-testid={row.current ? 'branch-current-row' : 'branch-row'}
              onClick={() => nav.activate(index)}
            >
              <span className={styles.optionColumn}>
                <span className={styles.optionLabel}>{row.target.content}</span>
                {row.current ? null : (
                  <span className={styles.optionMeta}>{row.target.uuid.slice(0, 8)}</span>
                )}
              </span>
              {row.current ? <span className={styles.badge}>current</span> : null}
            </div>
          ))}
        </div>
      )}
    </PanelShell>
  );
}
