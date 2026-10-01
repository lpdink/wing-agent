/**
 * The main pane: the open session's transcript, plus its live runtime state.
 *
 * The transcript (step 08) is the pane: the shared renderer's scroller fills the card
 * and owns the scrolling. The runtime facts step 07 showed here — identity, workspace,
 * model knobs, context usage, message count, last error, all read straight off the
 * `SessionRecord` — are still real data (they are what `wing info` prints and what
 * answers "did my session actually come back after a reconnect"), so they stay, folded
 * into one `<details>` whose summary names the two things worth a glance (context
 * usage and message count).
 *
 * `key={sessionId}` on the transcript is load-bearing: the renderer's image cache is
 * document-level and keyed by markdown source, so a session switch has to remount the
 * rows for their image effects to run again (see `useWebBridge.ts`).
 */

import type { ReactElement } from 'react';

import type { RuntimeSnapshot } from '../connection/runtime';
import { rowStatusFromModel } from '../sessions/rows';
import { Transcript } from '../transcript/Transcript';

import { contextPercent, formatTokens, statusLabel } from './labels';

export interface SessionPaneProps {
  readonly snapshot: RuntimeSnapshot;
  readonly onNewSession: () => void;
}

export function SessionPane({ snapshot, onNewSession }: SessionPaneProps): ReactElement {
  const record = snapshot.record;

  if (record === null) {
    const hasSessions = snapshot.sessions.length > 0;
    return (
      <section className="pane pane--empty">
        <h2>No session open</h2>
        <p>
          {hasSessions
            ? 'Pick a session from the list, or start a new one.'
            : 'Start a session to talk to the agent — it is created on the gateway and shows up in the list.'}
        </p>
        <button type="button" className="button button--primary" onClick={onNewSession}>
          New session
        </button>
      </section>
    );
  }

  const status = rowStatusFromModel(record.status);
  const workspace = record.meta.workspace === '' ? null : record.meta.workspace;
  const percent = contextPercent(record.context.usedTokens, record.context.windowTokens);
  const thinking = record.meta.thinking
    ? record.meta.reasoningEffort === ''
      ? 'on'
      : `on (${record.meta.reasoningEffort})`
    : 'off';

  return (
    <section className="pane">
      <header className="pane__head">
        <h2 className="pane__title">{record.title}</h2>
        <span className={`status status--${status}`}>{statusLabel(status)}</span>
        {record.turn.active ? <span className="pane__turn">turn running…</span> : null}
      </header>

      <details className="pane__details">
        <summary className="pane__summary">
          <span>Session details</span>
          <span className="pane__summary-value">
            {formatTokens(record.context.usedTokens)} / {formatTokens(record.context.windowTokens)} tokens
            {percent === null ? '' : ` (${percent}%)`} · {record.context.messageCount} messages
          </span>
        </summary>

        <dl className="facts">
          <div className="facts__row">
            <dt>Session</dt>
            <dd className="facts__mono">{record.sessionId}</dd>
          </div>
          <div className="facts__row">
            <dt>Workspace</dt>
            <dd className="facts__mono">{workspace ?? '—'}</dd>
          </div>
          <div className="facts__row">
            <dt>Model</dt>
            <dd>{record.meta.model === '' ? '—' : record.meta.model}</dd>
          </div>
          <div className="facts__row">
            <dt>Agent</dt>
            <dd>{record.meta.agent === '' ? 'default' : record.meta.agent}</dd>
          </div>
          <div className="facts__row">
            <dt>Thinking</dt>
            <dd>{thinking}</dd>
          </div>
          <div className="facts__row">
            <dt>YOLO</dt>
            <dd>{record.meta.yolo ? 'on' : 'off'}</dd>
          </div>
          <div className="facts__row">
            <dt>Total tokens</dt>
            <dd>
              {formatTokens(record.totals.promptTokens)} in / {formatTokens(record.totals.completionTokens)}{' '}
              out
            </dd>
          </div>
        </dl>

        {record.lastError === null ? null : (
          <p className="pane__error" role="alert">
            {record.lastError}
          </p>
        )}
      </details>

      <Transcript key={record.sessionId} record={record} version={snapshot.recordVersion} />
    </section>
  );
}
