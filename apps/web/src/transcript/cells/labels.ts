/**
 * The copy the shared cards need, in this app's voice.
 *
 * `@wing-agent/ui` carries no locale (every card takes its strings as props), so the
 * English labels live here — the same wording the prototype harness
 * (`packages/ui/tools/port-preview/main.tsx`) renders them with and the same
 * vocabulary the transcript used before the swap. One home, so the two shells cannot
 * drift into "Copy" on one card and "copy" on the next.
 */

import type {
  ApprovalPanelLabels,
  CodeToolbarLabels,
  DiffBlockLabels,
  TerminalBlockLabels,
} from '@wing-agent/ui';
import type { AskState } from '@wing-agent/session';

/** Language / wrapping / copy controls on the code cards. */
export const CODE_TOOLBAR_LABELS: CodeToolbarLabels = {
  codeLabel: 'Code',
  wrapLabel: 'Wrap lines',
  unwrapLabel: 'Do not wrap',
};

/** The Bash card (ANSI output, run state, exit status). */
export const TERMINAL_LABELS: TerminalBlockLabels = {
  signal: (signal) => `killed by ${signal}`,
  // The card only renders the pill for a *non-zero* code; Wing does not parse the
  // text marker out of the result (the host has no structured exit code yet), so
  // `exitCode` stays undefined at the call site and this only covers a future host
  // that starts sending one.
  exitCode: (code) => `exit ${code}`,
  noExitCode: 'no exit code',
  running: 'Running',
  failed: 'Failed',
  done: 'Done',
  copy: 'Copy',
  copied: 'Copied',
  noOutput: 'No output',
  collapseAria: 'Collapse output',
  collapse: 'Collapse',
  expandAria: (hidden) => `Show ${hidden} more lines`,
  expand: (hidden) => `… ${hidden} more lines`,
};

/** The diff card; mirrors the prototype harness' labels. */
export const DIFF_LABELS: DiffBlockLabels = {
  ...CODE_TOOLBAR_LABELS,
  copy: 'Copy',
  copied: 'Copied',
  collapseAria: 'Collapse diff',
  collapse: 'Collapse',
  expandAria: (hidden) => `Show ${hidden} more lines`,
  expand: (hidden) => `… ${hidden} more lines`,
};

/** Copy labels shared by every markdown fence. */
export const CODE_COPY_LABELS = { copyLabel: 'Copy', copiedLabel: 'Copied' } as const;

/**
 * The approval card's copy.
 *
 * The strip text follows the ask's lifecycle (the card's own `state` prop only
 * distinguishes pending from settled): the three words the transcript used before
 * the swap — approval required / decision sent / cancelled — are preserved by
 * building the labels from the cell's state.
 */
export function approvalLabels(state: AskState): ApprovalPanelLabels {
  return {
    waiting: 'Approval required',
    escalation: (toolName) => `Tool ${toolName} requests privileged execution`,
    settled: state === 'cancelled' ? 'Cancelled' : 'Decision sent',
    approve: 'Approve',
    deny: 'Deny',
    detailAria: 'Approval details',
  };
}
