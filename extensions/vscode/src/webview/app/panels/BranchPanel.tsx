/**
 * The rewind / fork picker.
 *
 * Host-opened: on screen exactly while `panels.branchPicker` is non-null
 * (`interfaces.md`), with the `mode` the host opened it for. The rows are the
 * gateway's branch targets — every user message plus compaction nodes, and the
 * `current` sentinel that the host normalizes into `current: true`.
 *
 * The sentinel marks **where the conversation is now**: it is displayed (with its
 * badge) but not a target, because there is nothing to rewind to. The highlight
 * therefore starts on the `current` row when the host made it selectable, otherwise
 * on the first row — the frozen rule from `interfaces.md`.
 *
 * Selecting a row is the TUI's `/rewind <uuid>` / `/fork <uuid>`: one
 * `runPromptCommand`, the host executes it.
 */

import { useEffect, useMemo, useRef } from 'react';
import type { ReactElement } from 'react';

import type { BranchPickerModel, BranchTargetModel } from '../../../shared';
import { postToHost } from '../../bridge/channel';
import styles from '../../styles/panels.module.css';
import { PanelEmpty, PanelShell } from './PanelShell';
import { optionId, useListNav, useRevealIndex } from './listNav';

export interface BranchPanelProps {
  /** Session the command is issued from (the active one). */
  readonly sessionId: string;
  readonly picker: BranchPickerModel;
  readonly onClose: () => void;
}

const TITLES: Record<BranchPickerModel['mode'], string> = {
  rewind: 'Rewind to message',
  fork: 'Fork from message',
};

/** One rendered row: a target, or the current-point sentinel. */
export interface BranchRow {
  readonly target: BranchTargetModel;
  /** The sentinel — shown, marked, not selectable. */
  readonly sentinel: boolean;
}

/** Rows with the sentinel flag resolved (pure; exercised by the tests). */
export function branchRows(picker: BranchPickerModel): readonly BranchRow[] {
  return picker.rows.map((target) => ({ target, sentinel: target.current }));
}

/**
 * Index of the row the highlight starts on; `-1` means "the first selectable row"
 * (what `useListNav` does). The sentinel is not selectable, so a picker whose only
 * `current` row is the sentinel starts at the top — the frozen `interfaces.md` rule.
 */
export function branchInitialIndex(rows: readonly BranchRow[]): number {
  const index = rows.findIndex((row) => row.target.current && !row.sentinel);
  return index >= 0 ? index : -1;
}

export function BranchPanel({ sessionId, picker, onClose }: BranchPanelProps): ReactElement {
  const rows = useMemo(() => branchRows(picker), [picker]);
  const listRef = useRef<HTMLDivElement>(null);
  const command = picker.mode === 'rewind' ? '/rewind' : '/fork';

  const select = (index: number): void => {
    const row = rows[index];
    if (row === undefined || row.sentinel) {
      return;
    }
    postToHost({ type: 'runPromptCommand', sessionId, name: command, argsText: row.target.uuid });
    onClose();
  };

  const nav = useListNav(
    rows.map((row) => ({ selectable: !row.sentinel })),
    'branch-panel',
    {
      initialIndex: branchInitialIndex(rows),
      onSelect: select,
      onEscape: onClose,
    },
  );
  // Keyboard navigation must never walk the highlight off-screen.
  useRevealIndex(listRef, 'branch-panel', nav.index);

  useEffect(() => {
    listRef.current?.focus();
  }, []);

  return (
    <PanelShell
      title={TITLES[picker.mode]}
      testId="branch-panel"
      onClose={onClose}
      hint="↑↓ navigate · Enter apply · Esc close"
    >
      {rows.length === 0 ? (
        <PanelEmpty text={picker.mode === 'rewind' ? 'No rewind points yet.' : 'No fork points yet.'} />
      ) : (
        <div
          className={styles.list}
          role="listbox"
          aria-label={TITLES[picker.mode]}
          tabIndex={-1}
          ref={listRef}
          data-testid="branch-panel-list"
          data-mode={picker.mode}
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
              aria-disabled={row.sentinel ? 'true' : undefined}
              data-highlighted={index === nav.index ? 'true' : 'false'}
              data-current={row.sentinel ? 'true' : 'false'}
              data-testid={row.sentinel ? 'branch-current-row' : 'branch-row'}
              onClick={() => nav.activate(index)}
            >
              <span className={styles.optionColumn}>
                <span className={styles.optionLabel}>{row.target.content}</span>
                {row.sentinel ? null : (
                  <span className={styles.optionMeta}>{row.target.uuid.slice(0, 8)}</span>
                )}
              </span>
              {row.sentinel ? <span className={styles.badge}>current</span> : null}
            </div>
          ))}
        </div>
      )}
    </PanelShell>
  );
}
