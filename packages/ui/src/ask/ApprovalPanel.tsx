// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-approval/src/client/ApprovalPanel.tsx (+ ApprovalPanel.module.css)
// Modified for Wing: the slot/carrier contract (`matched`, `renderSlot`,
// `pending.answer`, promise rollback) is replaced by plain props — `detail` is a
// ReactNode, `onDecide` reports the decision and the owning cell stays authoritative
// for the lifecycle (`state`). `t()` is replaced by `ApprovalPanelLabels`.

/** The Bash dangerous-command confirmation, compact form. */
import { useEffect, useRef, type KeyboardEvent, type ReactNode } from 'react';
import clsx from 'clsx';

import type { AskState } from '@wing-agent/session';
import { Button } from '../components/Button';
import { StateDot } from '../components/StateDot';
import css from './ApprovalPanel.module.css';

/** Copy of one approval panel; every string is a prop (Wing has no runtime locale). */
export interface ApprovalPanelLabels {
  /** Strip label while the request is pending. */
  readonly waiting: string;
  /** Headline when the host gave no reason: names the tool asking for the escalation. */
  readonly escalation: (toolName: string) => string;
  /** Strip label once the request settled. */
  readonly settled: string;
  readonly approve: string;
  readonly deny: string;
  readonly detailAria: string;
}

/** The renderer's default approval copy (English, like every UI string it ships). */
export const DEFAULT_APPROVAL_PANEL_LABELS: ApprovalPanelLabels = {
  waiting: 'Waiting for approval',
  escalation: (toolName) => `Tool ${toolName} requests privileged execution`,
  settled: 'Decision sent',
  approve: 'Allow once',
  deny: 'Reject',
  detailAria: 'Approval details',
};

export interface ApprovalPanelProps {
  /** Correlation id of the ask (the row's DOM key + `tool_call_id` on the wire). */
  readonly requestId: string;
  /** Raw tool name, used by the default escalation headline. */
  readonly toolName: string;
  /** Host-provided reason; wins over the escalation headline when present. */
  readonly reason?: string | undefined;
  /** Tool-owned detail (the command, a terminal card, …). */
  readonly detail?: ReactNode;
  /** Lifecycle: the panel only accepts a decision while `awaiting`. */
  readonly state: AskState;
  readonly labels?: ApprovalPanelLabels | undefined;
  readonly onDecide: (decision: 'approve' | 'deny') => void;
  readonly className?: string | undefined;
}

/**
 * Render one pending approval and its optional tool-owned detail.
 *
 * Enter approves and Escape denies while focus is inside the card (unless focus sits
 * on a control that owns the key, and never mid-IME-composition); the two buttons do
 * the same. The component holds no in-flight state: it hands the decision to
 * `onDecide` and the owning cell's `state` is what disables it afterwards. Upstream
 * additionally tracked the promise/carrier round-trip (`waiting` + `active` refs) and
 * rolled back on rejection; that guard belongs to the carrier contract this port
 * dropped, so a host that is slow to settle `state` must settle it (or dedupe the
 * decision) itself — repeating the key before the echo arrives calls `onDecide` again.
 * @param props - request identity, copy, detail and the decision sink.
 * @returns The approval card.
 */
export function ApprovalPanel({
  requestId,
  toolName,
  reason,
  detail,
  state,
  labels = DEFAULT_APPROVAL_PANEL_LABELS,
  onDecide,
  className,
}: ApprovalPanelProps) {
  const pending = state === 'awaiting';
  const composing = useRef(false);
  const compositionEnded = useRef(false);
  useEffect(() => {
    // A new request re-arms the IME guards (the panel is keyed by request upstream).
    composing.current = false;
    compositionEnded.current = false;
  }, [requestId]);

  const answer = (outcome: 'approve' | 'deny'): void => {
    if (!pending) return;
    onDecide(outcome);
  };

  const keydown = (event: KeyboardEvent<HTMLDivElement>): void => {
    const element = event.target as Element;
    if (
      event.defaultPrevented ||
      !event.currentTarget.contains(document.activeElement) ||
      element.closest('input, textarea, select, [contenteditable="true"], [contenteditable=""]') !== null
    ) {
      return;
    }
    if (event.key !== 'Enter' && event.key !== 'Escape') return;
    if (event.key === 'Enter' && element.closest('button, a[href], [role="button"]') !== null) return;
    if (event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return;
    event.preventDefault();
    event.stopPropagation();
    // `keyCode 229` is the legacy IME-composition signal engines emit without isComposing.
    const native = event.nativeEvent as KeyboardEvent['nativeEvent'] & { keyCode?: number };
    if (
      event.repeat ||
      composing.current ||
      compositionEnded.current ||
      native.isComposing ||
      native.keyCode === 229
    ) {
      return;
    }
    answer(event.key === 'Enter' ? 'approve' : 'deny');
  };

  return (
    <div
      className={clsx(css.root, className)}
      data-approval-key={requestId}
      aria-busy={!pending}
      onKeyDown={keydown}
      onKeyUpCapture={() => {
        compositionEnded.current = false;
      }}
      onCompositionStartCapture={() => {
        composing.current = true;
      }}
      onCompositionEndCapture={() => {
        composing.current = false;
        compositionEnded.current = true;
      }}
    >
      <div className={css.card}>
        <div className={css.strip}>
          <StateDot state={pending ? 'warning' : state === 'answered' ? 'done' : 'idle'} />
          {pending ? labels.waiting : labels.settled}
        </div>
        <div
          className={css.body}
          data-approval-scroll=""
          tabIndex={0}
          role="group"
          aria-label={labels.detailAria}
        >
          <div className={css.headline}>{reason ?? labels.escalation(toolName)}</div>
          {detail != null && <div className={css.command}>{detail}</div>}
        </div>
        <div className={css.actionRow}>
          <Button
            variant="outline"
            className={css.reject}
            disabled={!pending}
            onClick={() => {
              answer('deny');
            }}
          >
            {labels.deny}
          </Button>
          <Button
            variant="primary"
            disabled={!pending}
            onClick={() => {
              answer('approve');
            }}
          >
            {labels.approve}
          </Button>
        </div>
      </div>
    </div>
  );
}
