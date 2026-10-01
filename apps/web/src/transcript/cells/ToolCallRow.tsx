/**
 * A tool call: the shared process row (`DisclosureRow`), with the Bash card on the body.
 *
 * The row is the ported skeleton the renderer used to hand-roll: a status dot, the
 * tool's title, the one-line subject, a chevron, and Enter/Space/click toggling —
 * `DisclosureRow` owns all of that (and keeps the shimmer on the title while the
 * call runs). The open/closed decision itself stays here, with the *same key and the
 * same default* as the swapped-out `ToolCallCell`:
 *
 * - `${cell.id}:tool` in the package's collapse-override store, so the reader's
 *   choice survives a session switch exactly as it did before;
 * - collapsed by default, **open when the call failed** — an error the user cannot
 *   see is a bug report waiting to happen.
 *
 * The body: a `Bash` call becomes a `TerminalBlock` (ANSI output, run-state dot, the
 * command as a prompt line, copy control) with the session workspace as the prompt's
 * cwd; every other tool keeps the generic Arguments / Result cards, which the port
 * has no card for yet (`JsonBlock` is P1 in the borrow audit). Nothing parses the
 * result text: the Bash exit-code marker stays text and is *not* turned into a pill
 * (the borrow card's explicit trade-off), and `result.truncated` still renders the
 * "Output truncated" note.
 */

import type { ReactElement } from 'react';

import { TOOL_NAMES, type ToolCallCellModel, type ToolCallResultModel } from '@wing-agent/session';
import {
  DisclosureRow,
  FileReferenceText,
  StateDot,
  TerminalBlock,
  useCollapsible,
  type StateDotState,
} from '@wing-agent/ui';

import { TERMINAL_LABELS } from './labels';

export interface ToolCallRowProps {
  readonly cell: ToolCallCellModel;
  /** The open session's working directory (`''` when unknown). */
  readonly workspace: string;
}

export function ToolCallRow({ cell, workspace }: ToolCallRowProps): ReactElement {
  const busy = cell.status === 'streaming' || cell.status === 'pending';
  const { collapsed, toggle } = useCollapsible(`${cell.id}:tool`, cell.status !== 'failed');
  const subject = cell.display.subject;
  const args = argsText(cell);
  const bash = cell.name === TOOL_NAMES.bash;

  return (
    <article
      className="cell"
      data-cell-id={cell.id}
      data-cell-kind="tool_call"
      data-cell-status={cell.status}
      data-collapsed={collapsed ? 'true' : 'false'}
    >
      <DisclosureRow
        icon={<StateDot state={dotState(cell.status)} />}
        title={cell.display.title}
        running={busy}
        open={!collapsed}
        expandable
        expandOnRowClick
        onToggle={toggle}
        collapsedContent={
          subject === '' ? undefined : (
            // The subject can be a file path (Read): it stays clickable on its own,
            // and the click must not also toggle the row.
            <span className="cell__tool-subject" onClick={(event) => event.stopPropagation()}>
              <FileReferenceText text={subject} />
            </span>
          )
        }
      >
        <div className="cell__tool-body">
          {bash ? (
            <div className="cell__terminal">
              <TerminalBlock
                command={bashCommand(cell)}
                cwd={workspace === '' ? undefined : workspace}
                output={cell.result?.text ?? ''}
                running={busy}
                labels={TERMINAL_LABELS}
              />
            </div>
          ) : (
            <>
              {args === null ? null : <ToolCard title="Arguments" text={args} />}
              {cell.result === null ? null : <ToolResultCard result={cell.result} />}
            </>
          )}
        </div>
      </DisclosureRow>
    </article>
  );
}

/** Run-state dot: the same three-way distinction the old glyph drew. */
function dotState(status: ToolCallCellModel['status']): StateDotState {
  switch (status) {
    case 'streaming':
    case 'pending':
      return 'ongoing';
    case 'failed':
      return 'error';
    default:
      return 'done';
  }
}

/**
 * Prefer the parsed arguments (host-provided); fall back to the raw stream.
 *
 * A streaming call has no parsed arguments yet — the renderer shows the fragment it
 * has instead of an empty card, and never parses it itself (the host's rule).
 */
function argsText(cell: ToolCallCellModel): string | null {
  if (cell.args !== null) {
    return JSON.stringify(cell.args, null, 2);
  }
  return cell.argsText === '' ? null : cell.argsText;
}

/**
 * The command line for a Bash call.
 *
 * The model's subjects are collapsed to one line (`commandOneLine`), which is right
 * for the row but loses the structure the terminal card draws (one prompt row per
 * line). The parsed arguments carry the real command; while they are still
 * streaming, the subject is the best available — it is the command so far.
 */
function bashCommand(cell: ToolCallCellModel): string {
  const args = cell.args;
  if (args !== null && typeof args === 'object' && !Array.isArray(args)) {
    const command = (args as Record<string, unknown>)['command'];
    if (typeof command === 'string' && command !== '') {
      return command;
    }
  }
  return cell.display.subject;
}

/** A titled, monospaced output card (the renderer's tool card, drawn by this app). */
function ToolCard({
  title,
  text,
  error = false,
}: {
  readonly title: string;
  readonly text: string;
  readonly error?: boolean;
}): ReactElement {
  return (
    <div className="cell__card" data-card={title.toLowerCase()}>
      <div className="cell__card-title">{title}</div>
      <pre className="cell__card-body" data-error={error ? 'true' : 'false'}>
        {text}
      </pre>
    </div>
  );
}

function ToolResultCard({ result }: { readonly result: ToolCallResultModel }): ReactElement {
  return (
    <>
      <ToolCard title={result.isError ? 'Error' : 'Result'} text={result.text} error={result.isError} />
      {result.truncated ? <div className="cell__card-note">Output truncated</div> : null}
    </>
  );
}
