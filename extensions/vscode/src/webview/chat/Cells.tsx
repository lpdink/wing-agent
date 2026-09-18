/**
 * Transcript cells.
 *
 * Every component here is a *pure renderer*: it takes a `CellModel` from the
 * host and local interaction state, and never re-derives business facts (no
 * partial-JSON parsing, no diff computation). Everything a renderer needs is
 * already in the model — see `src/shared/cells.ts`.
 *
 * Layout follows VS Code's chat rows (`CHAT:3771-3774`): one row per cell with
 * `padding: 5px 16px`, no extra gap between rows.
 */

import type { ReactElement } from 'react';

import type {
  AssistantCellModel,
  DiffCellModel,
  DiffLineKind,
  MetricsCellModel,
  SeparatorCellModel,
  SessionId,
  SystemCellModel,
  ThinkingCellModel,
  TodoCellModel,
  ToolCallCellModel,
  ToolCallResultModel,
  ToolCallStatus,
  UserCellModel,
} from '../../shared';
import { postToHost } from '../bridge/channel';
import styles from '../styles/chat.module.css';
import { FileReference, parseFileReference } from './FileReference';
import { useCollapsible } from './interaction';
import { MarkdownStream, MarkdownText } from './Markdown';

// ── user ──────────────────────────────────────────────────────────────

export function UserCell({ cell }: { readonly cell: UserCellModel }): ReactElement {
  const pending = cell.state === 'pending';
  return (
    <article className={styles.row} data-cell-id={cell.id} data-cell-kind="user" data-cell-state={cell.state}>
      {/* Right-aligned bubble: CHAT:3792-3805. */}
      <div className={styles.userBubble} data-state={cell.state}>
        <MarkdownText text={cell.text} />
        {pending ? <StreamEllipsis /> : null}
      </div>
    </article>
  );
}

// ── assistant ─────────────────────────────────────────────────────────

export function AssistantCell({ cell }: { readonly cell: AssistantCellModel }): ReactElement {
  return (
    <article
      className={styles.row}
      data-cell-id={cell.id}
      data-cell-kind="assistant"
      data-streaming={cell.streaming ? 'true' : 'false'}
    >
      <MarkdownStream text={cell.text} streaming={cell.streaming} />
    </article>
  );
}

// ── thinking ──────────────────────────────────────────────────────────

/**
 * Collapsible reasoning.
 *
 * While the model is still thinking the block stays open (that is the only
 * feedback the user gets); once the stream ends it collapses to a single line —
 * the rule Copilot uses. A user toggle wins over that default forever.
 */
export function ThinkingCell({ cell }: { readonly cell: ThinkingCellModel }): ReactElement {
  const { collapsed, toggle } = useCollapsible(`${cell.id}:thinking`, !cell.streaming);

  return (
    <article
      className={styles.row}
      data-cell-id={cell.id}
      data-cell-kind="thinking"
      data-collapsed={collapsed ? 'true' : 'false'}
      data-streaming={cell.streaming ? 'true' : 'false'}
    >
      <div className={styles.thinking}>
        <button type="button" className={styles.disclosureHeader} aria-expanded={!collapsed} onClick={toggle}>
          <span className={styles.chevron} data-expanded={collapsed ? 'false' : 'true'} aria-hidden="true" />
          <span className={styles.thinkingLabel} data-shimmer={cell.streaming ? 'true' : 'false'}>
            {thinkingLabel(cell)}
          </span>
        </button>
        {collapsed ? null : (
          <div className={styles.thinkingBody}>
            <MarkdownStream text={cell.text} streaming={cell.streaming} />
          </div>
        )}
      </div>
    </article>
  );
}

/** `Thinking` while streaming; `Thought for 1.2s` once the duration is known. */
function thinkingLabel(cell: ThinkingCellModel): string {
  if (cell.streaming) {
    return 'Thinking';
  }
  return cell.durationMs === null ? 'Thought' : `Thought for ${(cell.durationMs / 1000).toFixed(1)}s`;
}

// ── system ────────────────────────────────────────────────────────────

export function SystemCell({ cell }: { readonly cell: SystemCellModel }): ReactElement {
  return (
    <article
      className={styles.row}
      data-cell-id={cell.id}
      data-cell-kind="system"
      data-cell-level={cell.level}
    >
      <p className={styles.systemMessage} data-level={cell.level}>
        {cell.text}
      </p>
    </article>
  );
}

// ── tool call ─────────────────────────────────────────────────────────

/**
 * One tool call: a compact row by default, expandable to arguments + result.
 *
 * A failed call opens by itself — an error the user cannot see is a bug report
 * waiting to happen.
 */
export function ToolCallCell({ cell }: { readonly cell: ToolCallCellModel }): ReactElement {
  const { collapsed, toggle } = useCollapsible(`${cell.id}:tool`, cell.status !== 'failed');
  const subject = parseFileReference(cell.display.subject);
  const args = argsText(cell);

  return (
    <article
      className={styles.row}
      data-cell-id={cell.id}
      data-cell-kind="tool_call"
      data-cell-status={cell.status}
      data-collapsed={collapsed ? 'true' : 'false'}
    >
      <div className={styles.toolRow}>
        <div
          className={styles.toolHeader}
          role="button"
          tabIndex={0}
          aria-expanded={!collapsed}
          onClick={toggle}
          onKeyDown={(event) => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              toggle();
            }
          }}
        >
          <ToolStatusGlyph status={cell.status} />
          <span className={styles.toolTitle}>{cell.display.title}</span>
          <span className={styles.toolSubject}>
            {subject === null ? (
              cell.display.subject
            ) : (
              // A path is interactive: stop the row toggle, ask the host to open it.
              <span onClick={(event) => event.stopPropagation()}>
                <FileReference path={subject.path} line={subject.line} />
              </span>
            )}
          </span>
          <span className={styles.chevron} data-expanded={collapsed ? 'false' : 'true'} aria-hidden="true" />
        </div>

        {collapsed ? null : (
          <div className={styles.toolBody}>
            {args === null ? null : <ToolCard title="Arguments" text={args} language="json" />}
            {cell.result === null ? null : <ToolResultCard result={cell.result} />}
          </div>
        )}
      </div>
    </article>
  );
}

/** Prefer the parsed arguments (host-provided); fall back to the raw stream. */
function argsText(cell: ToolCallCellModel): string | null {
  if (cell.args !== null) {
    return JSON.stringify(cell.args, null, 2);
  }
  return cell.argsText === '' ? null : cell.argsText;
}

/** Running / done / failed marker — CHAT:93-140 uses an animated ellipsis while working. */
function ToolStatusGlyph({ status }: { readonly status: ToolCallStatus }): ReactElement {
  if (status === 'streaming' || status === 'pending') {
    return <span className={styles.toolGlyph} data-status={status} aria-hidden="true" />;
  }
  return (
    <span className={`${styles.toolGlyph} ${styles.toolGlyphDone}`} data-status={status} aria-hidden="true" />
  );
}

function ToolCard({
  title,
  text,
  language,
}: {
  readonly title: string;
  readonly text: string;
  readonly language: string;
}): ReactElement {
  return (
    <div className={styles.toolCard} data-card={title.toLowerCase()}>
      <div className={styles.toolCardTitle}>{title}</div>
      <pre className={styles.toolCardBody} data-language={language}>
        {text}
      </pre>
    </div>
  );
}

function ToolResultCard({ result }: { readonly result: ToolCallResultModel }): ReactElement {
  return (
    <div className={styles.toolCard} data-card="result">
      <div className={styles.toolCardTitle}>{result.isError ? 'Error' : 'Result'}</div>
      <pre className={styles.toolCardBody} data-error={result.isError ? 'true' : 'false'}>
        {result.text}
      </pre>
      {result.truncated ? <div className={styles.toolCardNote}>Output truncated</div> : null}
    </div>
  );
}

// ── diff ──────────────────────────────────────────────────────────────

/** A windowed diff, plus the escape hatch to the real diff editor. */
export function DiffCell({
  cell,
  sessionId,
}: {
  readonly cell: DiffCellModel;
  readonly sessionId: SessionId;
}): ReactElement {
  return (
    <article className={styles.row} data-cell-id={cell.id} data-cell-kind="diff">
      <div className={styles.diffCard}>
        <div className={styles.diffHeader}>
          <FileReference path={cell.path} line={null} />
          <span className={styles.diffCounts}>
            <span className={styles.diffAdded}>{`+${cell.added}`}</span>
            <span className={styles.diffRemoved}>{`−${cell.removed}`}</span>
          </span>
          <button
            type="button"
            className={styles.diffOpen}
            onClick={() => {
              postToHost({ type: 'openDiff', sessionId, cellId: cell.id });
            }}
          >
            Open diff
          </button>
        </div>
        <div className={styles.diffBody} data-testid="diff-body">
          {cell.lines.map((line, index) => (
            // Diff rows have no stable identity; the window is replaced wholesale.
            <div key={index} className={styles.diffLine} data-diff-kind={line.kind}>
              <span className={styles.diffLineNumber}>{line.oldLine ?? ''}</span>
              <span className={styles.diffLineNumber}>{line.newLine ?? ''}</span>
              <span className={styles.diffMarker}>{diffMarker(line.kind)}</span>
              <span className={styles.diffText}>{line.text}</span>
            </div>
          ))}
          {cell.truncated ? <div className={styles.diffTruncated}>…</div> : null}
        </div>
      </div>
    </article>
  );
}

function diffMarker(kind: DiffLineKind): string {
  switch (kind) {
    case 'add':
      return '+';
    case 'del':
      return '-';
    case 'hunk':
      return '';
    default:
      return ' ';
  }
}

// ── todo ──────────────────────────────────────────────────────────────

export function TodoCell({ cell }: { readonly cell: TodoCellModel }): ReactElement {
  return (
    <article className={styles.row} data-cell-id={cell.id} data-cell-kind="todo">
      <ul className={styles.todoList}>
        {cell.items.map((item, index) => (
          <li key={index} className={styles.todoItem} data-todo-status={item.status}>
            <span className={styles.todoGlyph} data-status={item.status} aria-hidden="true" />
            <span className={styles.todoText}>{item.content}</span>
          </li>
        ))}
      </ul>
    </article>
  );
}

// ── metrics / separator ───────────────────────────────────────────────

/** One line of turn accounting — the TUI keeps the same numbers in its status bar. */
export function MetricsCell({ cell }: { readonly cell: MetricsCellModel }): ReactElement {
  const { usage } = cell;
  const parts = [
    `${formatTokens(usage.promptTokens)} in`,
    `${formatTokens(usage.completionTokens)} out`,
    cacheLabel(usage.cachedTokens, usage.promptTokens),
    usage.ttftMs > 0 ? `${Math.round(usage.ttftMs)}ms TTFT` : null,
    cell.durationMs === null ? null : `${(cell.durationMs / 1000).toFixed(1)}s`,
    cell.model === '' ? null : cell.model,
  ].filter((part): part is string => part !== null);

  return (
    <article className={styles.row} data-cell-id={cell.id} data-cell-kind="metrics">
      <div className={styles.metrics}>{parts.join(' · ')}</div>
    </article>
  );
}

function cacheLabel(cached: number, prompt: number): string {
  if (prompt <= 0) {
    return '0% cache';
  }
  return `${Math.round((cached / prompt) * 100)}% cache`;
}

function formatTokens(value: number): string {
  return value >= 1000 ? `${(value / 1000).toFixed(1)}k` : `${value}`;
}

export function SeparatorCell({ cell }: { readonly cell: SeparatorCellModel }): ReactElement {
  return (
    <article className={styles.row} data-cell-id={cell.id} data-cell-kind="separator">
      <div className={styles.separator}>{cell.label === '' ? null : <span>{cell.label}</span>}</div>
    </article>
  );
}

// ── shared ────────────────────────────────────────────────────────────

/** The inline streaming marker (CHAT:493-537 + :134-140). */
function StreamEllipsis(): ReactElement {
  return <span className={styles.streamCaret} data-testid="stream-caret" aria-hidden="true" />;
}
