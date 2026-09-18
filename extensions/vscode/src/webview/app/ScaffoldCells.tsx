import type { ReactElement } from 'react';

import type {
  AskCellModel,
  AssistantCellModel,
  CellModel,
  DiffCellModel,
  MetricsCellModel,
  SeparatorCellModel,
  SystemCellModel,
  ThinkingCellModel,
  TodoCellModel,
  ToolCallCellModel,
  UserCellModel,
} from '../../shared';
import { assertNever } from '../../shared';
import styles from '../styles/cells.module.css';

/**
 * SCAFFOLD(01): a deliberately plain cell renderer.
 *
 * Its job is to prove that every kind of the shared union can be rendered from
 * host-produced data (and that the union is exhaustively handled at runtime).
 * Step 04 replaces it with the real chat rendering (Copilot-derived spacing,
 * markdown streaming, syntax highlighting) — grep for `SCAFFOLD(01)`.
 */

export function ScaffoldCell({ cell }: { readonly cell: CellModel }): ReactElement {
  switch (cell.kind) {
    case 'user':
      return <UserCell cell={cell} />;
    case 'assistant':
      return <AssistantCell cell={cell} />;
    case 'thinking':
      return <ThinkingCell cell={cell} />;
    case 'system':
      return <SystemCell cell={cell} />;
    case 'tool_call':
      return <ToolCallCell cell={cell} />;
    case 'diff':
      return <DiffCell cell={cell} />;
    case 'todo':
      return <TodoCell cell={cell} />;
    case 'ask':
      return <AskCell cell={cell} />;
    case 'metrics':
      return <MetricsCell cell={cell} />;
    case 'separator':
      return <SeparatorCell cell={cell} />;
    default:
      return assertNever(cell, 'ScaffoldCell');
  }
}

function UserCell({ cell }: { readonly cell: UserCellModel }): ReactElement {
  const stateClass =
    cell.state === 'pending' ? styles.pending : cell.state === 'discarded' ? styles.discarded : '';
  return (
    <div
      className={`${styles.cell} ${styles.user} ${stateClass ?? ''}`}
      data-cell-id={cell.id}
      data-cell-kind="user"
      data-cell-state={cell.state}
    >
      <span className={styles.label}>User</span>
      <p className={styles.text}>{cell.text}</p>
    </div>
  );
}

function AssistantCell({ cell }: { readonly cell: AssistantCellModel }): ReactElement {
  return (
    <div className={styles.cell} data-cell-id={cell.id} data-cell-kind="assistant">
      <span className={styles.label}>Assistant</span>
      <p className={`${styles.text} ${cell.streaming ? (styles.streaming ?? '') : ''}`}>{cell.text}</p>
    </div>
  );
}

function ThinkingCell({ cell }: { readonly cell: ThinkingCellModel }): ReactElement {
  return (
    <div className={`${styles.cell} ${styles.thinking}`} data-cell-id={cell.id} data-cell-kind="thinking">
      <span className={styles.label}>
        Thinking{cell.durationMs === null ? '' : ` · ${(cell.durationMs / 1000).toFixed(1)}s`}
      </span>
      <p className={styles.text}>{cell.text}</p>
    </div>
  );
}

function SystemCell({ cell }: { readonly cell: SystemCellModel }): ReactElement {
  const levelClass =
    cell.level === 'warning'
      ? styles.systemWarning
      : cell.level === 'error'
        ? styles.systemError
        : cell.level === 'notice'
          ? styles.systemNotice
          : styles.systemInfo;
  return (
    <div
      className={`${styles.cell} ${levelClass ?? ''}`}
      data-cell-id={cell.id}
      data-cell-kind="system"
      data-cell-level={cell.level}
    >
      <p className={styles.text}>{cell.text}</p>
    </div>
  );
}

function ToolCallCell({ cell }: { readonly cell: ToolCallCellModel }): ReactElement {
  const result = cell.result;
  return (
    <div
      className={styles.cell}
      data-cell-id={cell.id}
      data-cell-kind="tool_call"
      data-cell-status={cell.status}
    >
      <div className={styles.toolHeader}>
        <span className={styles.label}>{cell.display.title}</span>
        <span className={styles.toolSubject} title={cell.display.subject}>
          {cell.display.subject}
        </span>
        <span className={styles.metrics}>{cell.status}</span>
      </div>
      {result === null ? null : (
        <pre className={`${styles.toolResult} ${result.isError ? (styles.toolFailed ?? '') : ''}`}>
          {result.text}
        </pre>
      )}
    </div>
  );
}

function DiffCell({ cell }: { readonly cell: DiffCellModel }): ReactElement {
  return (
    <div className={styles.cell} data-cell-id={cell.id} data-cell-kind="diff">
      <div className={styles.diff}>
        <div className={styles.diffHeader}>
          {cell.path} · +{cell.added} −{cell.removed}
        </div>
        {cell.lines.map((line, i) => (
          <div
            // Diff rows have no stable id of their own; the window is immutable
            // until the host replaces the cell.
            key={`${cell.id}-${i}`}
            className={`${styles.diffLine} ${
              line.kind === 'add' ? (styles.diffAdd ?? '') : line.kind === 'del' ? (styles.diffDel ?? '') : ''
            }`}
            data-diff-kind={line.kind}
          >
            <span>{line.kind === 'add' ? '+' : line.kind === 'del' ? '−' : ' '}</span>
            <span>{line.text}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function TodoCell({ cell }: { readonly cell: TodoCellModel }): ReactElement {
  return (
    <div className={styles.cell} data-cell-id={cell.id} data-cell-kind="todo">
      <span className={styles.label}>Todo</span>
      <ul className={styles.todoList}>
        {cell.items.map((item) => (
          <li
            key={item.content}
            className={
              item.status === 'completed'
                ? styles.todoCompleted
                : item.status === 'in_progress'
                  ? styles.todoInProgress
                  : undefined
            }
            data-todo-status={item.status}
          >
            {item.content}
          </li>
        ))}
      </ul>
    </div>
  );
}

function AskCell({ cell }: { readonly cell: AskCellModel }): ReactElement {
  return (
    <div
      className={`${styles.cell} ${styles.ask}`}
      data-cell-id={cell.id}
      data-cell-kind="ask"
      data-ask-state={cell.state}
    >
      <span className={styles.label}>Ask{cell.approval ? ' · approval' : ''}</span>
      {cell.questions.map((question) => (
        <div key={question.id}>
          <p className={styles.text}>{question.question}</p>
          <ul className={styles.todoList}>
            {question.options.map((option) => (
              <li key={option.label}>
                {option.description === '' ? option.label : `${option.label} — ${option.description}`}
              </li>
            ))}
          </ul>
        </div>
      ))}
    </div>
  );
}

function MetricsCell({ cell }: { readonly cell: MetricsCellModel }): ReactElement {
  const { usage } = cell;
  const cacheRate =
    usage.promptTokens > 0 ? ((usage.cachedTokens / usage.promptTokens) * 100).toFixed(1) : '0.0';
  return (
    <div className={`${styles.cell} ${styles.metrics}`} data-cell-id={cell.id} data-cell-kind="metrics">
      {`${usage.promptTokens} in · ${usage.completionTokens} out · ${cacheRate}% cache · ${usage.ttftMs.toFixed(0)}ms ttft`}
      {cell.durationMs === null ? '' : ` · ${(cell.durationMs / 1000).toFixed(1)}s`}
    </div>
  );
}

function SeparatorCell({ cell }: { readonly cell: SeparatorCellModel }): ReactElement {
  return (
    <div className={styles.separator} data-cell-id={cell.id} data-cell-kind="separator">
      {cell.label === '' ? null : <span>{cell.label}</span>}
    </div>
  );
}
