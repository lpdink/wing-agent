// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/ConnectionIndicator.tsx (+ ConnectionIndicator.module.css)
// Modified for Wing: icons come from this package's `icons` module; the state /
// label / reconnect props are unchanged (the mapping from a shell's connection state
// to `ConnectionIndicatorState` stays with the shell).

import { useEffect, useState } from 'react';

import { IconCheckOutlineRegular, IconRefreshOutlineRegular } from '../icons';
import { StateDot } from './StateDot';
import css from './ConnectionIndicator.module.css';

/** Visual state rendered by {@link ConnectionIndicator}. */
export type ConnectionIndicatorState = 'disconnected' | 'connecting' | 'recovered';

/** Exit-transition length; keep equal to the `.leaving` transition duration in the stylesheet. */
const EXIT_MS = 150;

export interface ConnectionIndicatorProps {
  /** Visible outage, retry-attempt, or recovered state; `undefined` renders nothing. */
  readonly state: ConnectionIndicatorState | undefined;
  /** Outage text naming the retry action. */
  readonly disconnectedLabel: string;
  /** Retry text followed by the attempt dots. */
  readonly connectingLabel: string;
  /** Recovery confirmation. */
  readonly recoveredLabel: string;
  /** Accessible label for the outage action. */
  readonly reconnectActionLabel: string;
  /** Accessible label for replacing an active attempt. */
  readonly restartActionLabel: string;
  /** Request an immediate reconnect attempt. */
  readonly onReconnect: () => void;
}

/**
 * Render an inline connection-recovery control. The outage and retry-attempt
 * states are one button whose static label already names the retry action;
 * clicking it requests an immediate reconnect. The indicator animates in on
 * appearance and fades out for {@link EXIT_MS} before unmounting.
 * @param props - visible state, localized copy and the reconnect sink.
 * @returns the indicator, or null when no connection feedback is active.
 */
export function ConnectionIndicator({
  state,
  disconnectedLabel,
  connectingLabel,
  recoveredLabel,
  reconnectActionLabel,
  restartActionLabel,
  onReconnect,
}: ConnectionIndicatorProps) {
  const [rendered, setRendered] = useState(state);
  const leaving = state === undefined && rendered !== undefined;
  useEffect(() => {
    if (state !== undefined) {
      setRendered(state);
      return;
    }
    if (rendered === undefined) return;
    const timeout = window.setTimeout(() => {
      setRendered(undefined);
    }, EXIT_MS);
    return () => {
      window.clearTimeout(timeout);
    };
  }, [state, rendered]);

  if (rendered === undefined) return null;
  const leavingClass = leaving ? ` ${css.leaving}` : '';
  if (rendered === 'recovered') {
    return (
      <div
        className={`${css.indicator} ${css.success}${leavingClass}`}
        role="status"
        aria-label={recoveredLabel}
      >
        <span className={css.icon} aria-hidden="true">
          <IconCheckOutlineRegular size={14} />
        </span>
        <span className={css.label}>{recoveredLabel}</span>
      </div>
    );
  }

  const connecting = rendered === 'connecting';
  return (
    <button
      type="button"
      className={`${css.indicator} ${css.warning}${leavingClass}`}
      data-phase={rendered}
      aria-label={connecting ? restartActionLabel : reconnectActionLabel}
      onClick={onReconnect}
    >
      <span className={css.icon} aria-hidden="true">
        {connecting ? <StateDot state="ongoing" /> : <IconRefreshOutlineRegular size={14} />}
      </span>
      <span className={css.label}>
        {connecting ? (
          <>
            {connectingLabel}
            <span className={css.dots} aria-hidden="true">
              <span>.</span>
              <span className={css.secondDot}>.</span>
              <span className={css.thirdDot}>.</span>
            </span>
          </>
        ) : (
          disconnectedLabel
        )}
      </span>
    </button>
  );
}
