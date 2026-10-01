/**
 * An ask, in the shape the ported cards draw it.
 *
 * The three components mirror the three states of the cell, and the *reply path is
 * the one step 08 wired*: `postToHost` → the bridge controller → `GatewayRuntime`
 * (`answerAsk` / `approveTool`) → `buildAskReply` → a `ClientRequest` on the socket.
 * Nothing here invents an answer or a session id (`sessionId` is the cell's own).
 *
 * - an **approval** (the Bash dangerous-command confirmation) → `ApprovalPanel`,
 *   with the normalized question as its reason line and Enter/Esc as the shortcuts;
 * - an **awaiting** question form → `QuestionComposer` (one card at a time, pager,
 *   recommended badge, free-form field, IME guard, submit-time completeness check);
 * - anything else → `QuestionReplyView`, the settled reply bubble (summary, details,
 *   copy) — the old renderer left the disabled form on screen instead.
 *
 * The wrapper keeps the `data-ask-form` / `data-ask-state` hooks the screenshot
 * scenes and the tests read.
 */

import type { ReactElement } from 'react';

import type { AskCellModel } from '@wing-agent/session';
import { ApprovalPanel, QuestionComposer, QuestionReplyView, postToHost } from '@wing-agent/ui';

import { approvalLabels } from './labels';

export function AskRow({ cell }: { readonly cell: AskCellModel }): ReactElement {
  return (
    <article
      className="cell"
      data-cell-id={cell.id}
      data-cell-kind="ask"
      data-ask-form={cell.approval ? 'approval' : 'question'}
      data-ask-state={cell.state}
    >
      {cell.approval ? (
        <ApprovalPanel
          requestId={cell.requestId}
          // The only producer of this shape is the Bash confirmation.
          toolName="Bash"
          reason={cell.questions[0]?.question}
          state={cell.state}
          labels={approvalLabels(cell.state)}
          onDecide={(decision) => {
            postToHost({
              type: 'approveTool',
              sessionId: cell.sessionId,
              requestId: cell.requestId,
              decision,
            });
          }}
        />
      ) : cell.state === 'awaiting' ? (
        <QuestionComposer
          requestId={cell.requestId}
          questions={cell.questions}
          state={cell.state}
          onSubmit={(answers) => {
            postToHost({
              type: 'answerAsk',
              sessionId: cell.sessionId,
              requestId: cell.requestId,
              answers,
            });
          }}
        />
      ) : (
        <QuestionReplyView questions={cell.questions} answers={cell.answers} />
      )}
    </article>
  );
}
