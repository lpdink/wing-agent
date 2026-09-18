/**
 * `/model` panel: provider-grouped model rows plus the reasoning controls.
 *
 * Data comes from the host (`panels.modelPicker`); the panel is rendered only while
 * the host keeps it open (the contract's rule: a non-null `modelPicker` means "show
 * it"). Selecting a row posts one intent and lets the host close the panel — the
 * renderer never hides a host-owned overlay by itself.
 *
 * Grouping and the reasoning section follow VS Code's action widget
 * (`actionWidget.css:59-66` group header: `descriptionForeground`, weight 500, 11px;
 * `:68-80` separator) and the model picker's own configuration buttons
 * (`chatModelPicker.ts:2004` `configButtons`, rendered with the picker's chip label).
 */

import { useEffect, useMemo, useRef } from 'react';
import type { ReactElement } from 'react';

import type { EffortLevel, ModelPickerModel, SessionMetaModel } from '../../../shared';
import { EFFORT_LEVELS } from '../../../shared';
import { postToHost } from '../../bridge/channel';
import styles from '../../styles/panels.module.css';
import { PanelShell } from './PanelShell';
import { optionId, useListNav, useRevealIndex } from './listNav';

/** One rendered row; `selectable` drives both navigation and the click handler. */
export type ModelPanelRow =
  | { readonly kind: 'group'; readonly selectable: false; readonly label: string }
  | { readonly kind: 'empty'; readonly selectable: false; readonly label: string }
  | {
      readonly kind: 'model';
      readonly selectable: true;
      readonly provider: string;
      readonly model: string;
      readonly selected: boolean;
    }
  | { readonly kind: 'separator'; readonly selectable: false; readonly label: string }
  | { readonly kind: 'thinking'; readonly selectable: true; readonly enabled: boolean }
  | {
      readonly kind: 'effort';
      readonly selectable: true;
      readonly level: EffortLevel;
      readonly current: boolean;
    };

/**
 * Build the row list (pure — exercised directly by the tests).
 *
 * Providers are grouped in the order the host sent them, which is the order the
 * gateway reported (it groups by provider already, see the TUI's picker).
 */
export function buildModelRows(
  picker: ModelPickerModel,
  meta: Pick<SessionMetaModel, 'thinking' | 'reasoningEffort'>,
): readonly ModelPanelRow[] {
  const rows: ModelPanelRow[] = [];
  let provider = '';
  for (const row of picker.rows) {
    if (row.provider !== provider) {
      provider = row.provider;
      rows.push({ kind: 'group', selectable: false, label: provider });
    }
    rows.push({
      kind: 'model',
      selectable: true,
      provider: row.provider,
      model: row.model,
      selected: row.selected,
    });
  }

  if (picker.rows.length === 0) {
    // Nothing to choose, but the Reasoning controls are still valid — say why the
    // list is empty instead of showing a heading with nothing under it.
    rows.push({ kind: 'empty', selectable: false, label: 'No models available.' });
  }

  rows.push({ kind: 'separator', selectable: false, label: 'Reasoning' });
  rows.push({ kind: 'thinking', selectable: true, enabled: meta.thinking });
  if (meta.thinking) {
    for (const level of EFFORT_LEVELS) {
      rows.push({ kind: 'effort', selectable: true, level, current: level === meta.reasoningEffort });
    }
  }
  return rows;
}

export interface ModelPanelProps {
  readonly sessionId: string;
  readonly picker: ModelPickerModel;
  readonly meta: SessionMetaModel;
  readonly onClose: () => void;
}

export function ModelPanel({ sessionId, picker, meta, onClose }: ModelPanelProps): ReactElement {
  const rows = useMemo(() => buildModelRows(picker, meta), [picker, meta]);
  const listRef = useRef<HTMLDivElement>(null);

  const select = (index: number): void => {
    const row = rows[index];
    if (row === undefined || !row.selectable) {
      return;
    }
    switch (row.kind) {
      case 'model':
        postToHost({ type: 'setModel', sessionId, provider: row.provider, model: row.model });
        return;
      case 'thinking':
        postToHost({ type: 'setThinking', sessionId, enabled: !row.enabled });
        return;
      case 'effort':
        postToHost({ type: 'setEffort', sessionId, effort: row.level });
        return;
      default:
        return;
    }
  };

  // `activeIndex` indexes the host's `rows` (models only); the rendered list also
  // contains group headers, so the highlighted row has to be resolved by identity.
  const initialIndex = useMemo(() => {
    if (picker.activeIndex === null) {
      return undefined;
    }
    const target = picker.rows[picker.activeIndex];
    if (target === undefined) {
      return undefined;
    }
    const index = rows.findIndex(
      (row) => row.kind === 'model' && row.provider === target.provider && row.model === target.model,
    );
    return index >= 0 ? index : undefined;
  }, [picker, rows]);

  const nav = useListNav(rows, 'model-panel', {
    initialIndex,
    onSelect: select,
    onEscape: onClose,
  });
  // Keyboard navigation must never walk the highlight off-screen.
  useRevealIndex(listRef, 'model-panel', nav.index);

  // The panel owns the keyboard while it is open (VS Code's action widget does the
  // same): focus the list, so arrows work without a prior click.
  useEffect(() => {
    listRef.current?.focus();
  }, []);

  return (
    <PanelShell
      title="Model and reasoning"
      testId="model-panel"
      onClose={onClose}
      hint="↑↓ navigate · Enter apply · Esc close"
    >
      <div
        className={styles.list}
        role="listbox"
        aria-label="Models"
        tabIndex={-1}
        ref={listRef}
        data-testid="model-panel-list"
        onKeyDown={nav.onKeyDown}
        {...nav.listProps}
      >
        {rows.map((row, index) => (
          <ModelRowView
            key={rowKey(row, index)}
            row={row}
            index={index}
            highlighted={index === nav.index}
            onActivate={nav.activate}
          />
        ))}
      </div>
    </PanelShell>
  );
}

function rowKey(row: ModelPanelRow, index: number): string {
  switch (row.kind) {
    case 'model':
      return `model:${row.provider}/${row.model}`;
    case 'effort':
      return `effort:${row.level}`;
    case 'thinking':
      return 'thinking';
    case 'group':
      return `group:${row.label}`;
    case 'empty':
      return `empty:${row.label}`;
    case 'separator':
      return `separator:${row.label}:${index}`;
    default:
      return `row:${index}`;
  }
}

interface ModelRowViewProps {
  readonly row: ModelPanelRow;
  readonly index: number;
  readonly highlighted: boolean;
  readonly onActivate: (index: number) => void;
}

function ModelRowView({ row, index, highlighted, onActivate }: ModelRowViewProps): ReactElement {
  if (!row.selectable) {
    const kind = row.kind;
    return (
      <div
        className={kind === 'group' ? styles.groupHeader : styles.separator}
        role="presentation"
        data-testid={
          kind === 'group' ? 'model-panel-group' : kind === 'empty' ? 'model-panel-empty' : undefined
        }
      >
        {row.label}
      </div>
    );
  }

  const shared = {
    className: styles.option,
    role: 'option' as const,
    id: optionId('model-panel', index),
    'aria-selected': highlighted,
    'data-highlighted': highlighted ? 'true' : 'false',
    onClick: () => onActivate(index),
  };

  switch (row.kind) {
    case 'model':
      return (
        <div {...shared} data-current={row.selected ? 'true' : 'false'} data-testid="model-row">
          <span className={styles.optionLabel}>{row.model}</span>
          {row.selected ? <span className={styles.badge}>current</span> : null}
        </div>
      );
    case 'thinking':
      return (
        <div {...shared} data-current={row.enabled ? 'true' : 'false'} data-testid="thinking-row">
          <span className={styles.optionLabel}>Thinking</span>
          <span className={styles.optionMeta}>{row.enabled ? 'on' : 'off'}</span>
        </div>
      );
    case 'effort':
      return (
        <div {...shared} data-current={row.current ? 'true' : 'false'} data-testid="effort-row">
          <span className={styles.optionLabel}>{row.level}</span>
          {row.current ? <span className={styles.badge}>current</span> : null}
        </div>
      );
    default:
      return <div {...shared} />;
  }
}
