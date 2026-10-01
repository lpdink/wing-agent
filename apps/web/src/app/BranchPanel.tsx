/**
 * Branch panel — fork / rewind target picker.
 *
 * Rendered as an overlay when `record.panels.branchPicker !== null`.
 * Mirrors `extensions/vscode/src/host/session/manager.ts::openBranchesPanel`.
 */

import { type ReactElement } from 'react';

import type { BranchPickerModel } from '@wing-agent/session';

import type { ShellActions } from './App';

export interface BranchPanelProps {
  readonly branchPicker: BranchPickerModel;
  readonly sessionId: string;
  readonly actions: ShellActions;
  readonly onClose: () => void;
}

export function BranchPanel({
  branchPicker,
  sessionId,
  actions,
  onClose,
}: BranchPanelProps): ReactElement {
  const mode = branchPicker.mode;
  const isFork = mode === 'fork';

  return (
    <div className="overlay" role="dialog" aria-modal="true" aria-label={isFork ? 'Fork session' : 'Rewind session'}>
      <div className="panel-card panel-card--branch">
        <div className="panel-card__head">
          <h2 className="panel-card__title">
            {isFork ? 'Fork session at' : 'Rewind session to'}
          </h2>
          <button type="button" className="button button--ghost panel-card__close" onClick={onClose}>
            Close
          </button>
        </div>

        <div className="panel-card__body">
          <p className="panel-card__hint">
            {isFork
              ? 'Create a new branch of the conversation starting from a message. The current session is unchanged.'
              : 'Rewind the session to a previous state. Messages after the chosen point will be removed.'}
          </p>

          {branchPicker.rows.length === 0 ? (
            <p className="panel-card__empty">No branch targets available.</p>
          ) : (
            <div className="panel-card__list">
              {branchPicker.rows.map((target) => {
                const isCurrent = 'current' in target && target.current === true;
                return (
                  <button
                    key={target.uuid}
                    type="button"
                    className={`panel-card__row${isCurrent ? ' panel-card__row--current' : ''}`}
                    disabled={isCurrent}
                    onClick={() => {
                      if (isCurrent) {
                        return;
                      }
                      if (isFork) {
                        void actions.fork(sessionId, target.uuid);
                      } else {
                        void actions.rewind(target.uuid);
                      }
                      onClose();
                    }}
                  >
                    <span className="panel-card__row-label">
                      {target.content.length > 80
                        ? `${target.content.slice(0, 80)}…`
                        : target.content}
                    </span>
                    {isCurrent ? (
                      <span className="panel-card__tag">current</span>
                    ) : null}
                  </button>
                );
              })}
            </div>
          )}
        </div>

        <div className="panel-card__foot">
          <button type="button" className="button button--ghost" onClick={onClose}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}