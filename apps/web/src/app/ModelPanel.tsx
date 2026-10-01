/**
 * Model panel — provider × model picker with thinking / effort controls.
 *
 * Rendered as an overlay when `record.panels.modelPicker !== null`.
 * Mirrors `extensions/vscode/src/host/session/manager.ts::openModelPicker`.
 */

import { useState, type ReactElement } from 'react';

import type { ModelPickerModel } from '@wing-agent/session';
import { EFFORT_LEVELS, type EffortLevel } from '@wing-agent/session';

import type { ShellActions } from './App';

export interface ModelPanelProps {
  readonly modelPicker: ModelPickerModel;
  readonly thinking: boolean;
  readonly reasoningEffort: string;
  readonly actions: ShellActions;
  readonly onClose: () => void;
}

export function ModelPanel({
  modelPicker,
  thinking,
  reasoningEffort,
  actions,
  onClose,
}: ModelPanelProps): ReactElement {
  const [thinkingEnabled, setThinkingEnabled] = useState(thinking);
  const [effort, setEffort] = useState(reasoningEffort || 'medium');
  const [filter, setFilter] = useState('');

  // Unique providers list.
  const providers = modelPicker.rows.reduce<string[]>((acc, row) => {
    if (!acc.includes(row.provider)) {
      acc.push(row.provider);
    }
    return acc;
  }, []);

  const filteredRows = filter
    ? modelPicker.rows.filter(
        (row) =>
          row.provider.toLowerCase().includes(filter.toLowerCase()) ||
          row.model.toLowerCase().includes(filter.toLowerCase()),
      )
    : modelPicker.rows;

  return (
    <div className="overlay" role="dialog" aria-modal="true" aria-label="Model picker">
      <div className="panel-card panel-card--model">
        <div className="panel-card__head">
          <h2 className="panel-card__title">Select model</h2>
          <button type="button" className="button button--ghost panel-card__close" onClick={onClose}>
            Close
          </button>
        </div>

        {/* Search filter */}
        <div className="panel-card__search">
          <input
            className="field__control"
            type="text"
            placeholder="Filter providers and models…"
            value={filter}
            onChange={(e) => {
              setFilter(e.target.value);
            }}
            aria-label="Filter models"
          />
        </div>

        {/* Model list grouped by provider */}
        <div className="panel-card__body">
          {filteredRows.length === 0 ? (
            <p className="panel-card__empty">No models match the filter.</p>
          ) : (
            providers.map((provider) => {
              const models = filteredRows.filter((row) => row.provider === provider);
              if (models.length === 0) {
                return null;
              }
              return (
                <div key={provider} className="panel-card__group">
                  <h3 className="panel-card__group-title">{provider}</h3>
                  {models.map((row) => (
                    <button
                      key={`${row.provider}:${row.model}`}
                      type="button"
                      className={`panel-card__row${row.selected ? ' panel-card__row--selected' : ''}`}
                      onClick={() => {
                        void actions.updateMeta({ model: row.model, provider: row.provider });
                        onClose();
                      }}
                    >
                      <span className="panel-card__row-label">{row.model}</span>
                      {row.selected ? <span className="panel-card__check">✓</span> : null}
                    </button>
                  ))}
                </div>
              );
            })
          )}
        </div>

        {/* Thinking / Effort controls */}
        <div className="panel-card__foot">
          <div className="panel-card__section">
            <label className="panel-card__toggle">
              <input
                type="checkbox"
                checked={thinkingEnabled}
                onChange={(event) => {
                  const next = event.target.checked;
                  setThinkingEnabled(next);
                  void actions.updateMeta({ thinking: next });
                }}
              />
              <span>Thinking</span>
            </label>
          </div>

          {thinkingEnabled ? (
            <div className="panel-card__section">
              <label className="field__label" htmlFor="model-effort">
                Reasoning effort
              </label>
              <select
                id="model-effort"
                className="field__control"
                value={effort}
                onChange={(event) => {
                  const next = event.target.value;
                  setEffort(next);
                  if (isEffortLevel(next)) {
                    void actions.updateMeta({ thinking: true, reasoning_effort: next });
                  }
                }}
              >
                {EFFORT_LEVELS.map((level) => (
                  <option key={level} value={level}>
                    {level}
                  </option>
                ))}
              </select>
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

function isEffortLevel(value: string): value is EffortLevel {
  return (EFFORT_LEVELS as readonly string[]).includes(value);
}
