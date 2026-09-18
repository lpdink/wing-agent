/**
 * The `ask` cell — two shapes, one component.
 *
 * - **Question form** (`approval: false`): one block per question, options as
 *   radios (single select) or checkboxes (multi select), plus a free-form field
 *   when the question allows one. Submitting answers *all* questions at once.
 * - **Approval form** (`approval: true`): the Bash dangerous-command confirmation.
 *   The host normalizes it into a single required question ("Approve" / "Deny");
 *   we render it compactly and answer through the approval intent instead.
 *
 * Answers are never invented locally: the webview sends the intent and waits for
 * the host to update the cell to `answered`.
 */

import { useMemo, useState } from 'react';
import type { ReactElement } from 'react';

import type { AskAnswerModel, AskCellModel, AskQuestionModel, SessionId } from '../../shared';
import { postToHost } from '../bridge/channel';
import styles from '../styles/chat.module.css';

export function AskCell({
  cell,
  sessionId,
}: {
  readonly cell: AskCellModel;
  readonly sessionId: SessionId;
}): ReactElement {
  const awaiting = cell.state === 'awaiting';
  const [answers, setAnswers] = useState<Readonly<Record<string, DraftAnswer>>>({});
  const drafts = answers;

  const complete = useMemo(
    () => cell.questions.every((question) => isAnswered(question, drafts[question.id])),
    [cell.questions, drafts],
  );

  const update = (questionId: string, next: DraftAnswer): void => {
    setAnswers((previous) => ({ ...previous, [questionId]: next }));
  };

  const submit = (): void => {
    postToHost({
      type: 'answerAsk',
      sessionId,
      requestId: cell.requestId,
      answers: cell.questions.map((question) => toAnswer(question, drafts[question.id])),
    });
  };

  if (cell.approval) {
    return (
      <article
        className={styles.row}
        data-cell-id={cell.id}
        data-cell-kind="ask"
        data-ask-state={cell.state}
        data-ask-form="approval"
      >
        <div className={styles.askCard}>
          <div className={styles.askHeader}>
            <span className={styles.askTitle}>Approval required</span>
          </div>
          <div className={styles.askBody}>
            {cell.questions.map((question) => (
              <p key={question.id} className={styles.askQuestion}>
                {question.question}
              </p>
            ))}
          </div>
          <div className={styles.askFooter}>
            <span className={styles.askHint}>{approvalHint(cell)}</span>
            <div className={styles.askButtons}>
              <button
                type="button"
                className={styles.buttonPrimary}
                disabled={!awaiting}
                onClick={() => {
                  postToHost({
                    type: 'approveTool',
                    sessionId,
                    requestId: cell.requestId,
                    decision: 'approve',
                  });
                }}
              >
                Approve
              </button>
              <button
                type="button"
                className={styles.buttonSecondary}
                disabled={!awaiting}
                onClick={() => {
                  postToHost({ type: 'approveTool', sessionId, requestId: cell.requestId, decision: 'deny' });
                }}
              >
                Deny
              </button>
            </div>
          </div>
        </div>
      </article>
    );
  }

  return (
    <article
      className={styles.row}
      data-cell-id={cell.id}
      data-cell-kind="ask"
      data-ask-state={cell.state}
      data-ask-form="question"
    >
      <div className={styles.askCard}>
        {cell.questions.map((question) => (
          <div key={question.id} className={styles.askQuestionBlock}>
            <div className={styles.askHeader}>
              <span className={styles.askTitle}>{question.header === '' ? 'Question' : question.header}</span>
            </div>
            <div className={styles.askBody} data-question-id={question.id}>
              <p className={styles.askQuestion}>{question.question}</p>
              {question.options.length === 0 ? null : (
                <div className={styles.askOptions} role={question.multiSelect ? 'group' : 'radiogroup'}>
                  {question.options.map((option) => {
                    const answered = cell.answers.find((answer) => answer.questionId === question.id);
                    const draft = drafts[question.id];
                    const selected = awaiting
                      ? (draft?.selected.includes(option.label) ?? false)
                      : (answered?.selected.includes(option.label) ?? false);
                    return (
                      <label
                        key={option.label}
                        className={styles.askOption}
                        data-selected={selected ? 'true' : 'false'}
                      >
                        <input
                          type={question.multiSelect ? 'checkbox' : 'radio'}
                          className={styles.askOptionInput}
                          name={`${cell.id}-${question.id}`}
                          checked={selected}
                          disabled={!awaiting}
                          onChange={() => {
                            if (!awaiting) {
                              return;
                            }
                            const current = drafts[question.id]?.selected ?? [];
                            const next = question.multiSelect
                              ? current.includes(option.label)
                                ? current.filter((label) => label !== option.label)
                                : [...current, option.label]
                              : [option.label];
                            update(question.id, { selected: next, text: drafts[question.id]?.text ?? '' });
                          }}
                        />
                        <span className={styles.askOptionLabel}>
                          <span className={styles.askOptionTitle}>{option.label}</span>
                          {option.description === '' ? null : (
                            <span className={styles.askOptionDescription}>{option.description}</span>
                          )}
                        </span>
                      </label>
                    );
                  })}
                </div>
              )}
              {question.required ? null : (
                <textarea
                  className={styles.askInput}
                  rows={2}
                  placeholder="Type an answer…"
                  disabled={!awaiting}
                  value={
                    awaiting
                      ? (drafts[question.id]?.text ?? '')
                      : (cell.answers.find((a) => a.questionId === question.id)?.text ?? '')
                  }
                  onChange={(event) => {
                    update(question.id, {
                      selected: drafts[question.id]?.selected ?? [],
                      text: event.target.value,
                    });
                  }}
                />
              )}
            </div>
          </div>
        ))}

        <div className={styles.askFooter}>
          <span className={styles.askHint}>{awaiting ? 'Answers are sent to the agent' : 'Answered'}</span>
          <button
            type="button"
            className={styles.buttonPrimary}
            disabled={!awaiting || !complete}
            onClick={submit}
          >
            Submit
          </button>
        </div>
      </div>
    </article>
  );
}

/** Local draft of one question's answer. */
interface DraftAnswer {
  readonly selected: readonly string[];
  readonly text: string;
}

/**
 * Only *required* questions gate the submit button: they are the option-only
 * ones, where "no answer" is not a valid response. Free-form questions may be
 * left empty (the user is skipping them).
 */
function isAnswered(question: AskQuestionModel, draft: DraftAnswer | undefined): boolean {
  if (!question.required) {
    return true;
  }
  return (draft?.selected.length ?? 0) > 0;
}

function toAnswer(question: AskQuestionModel, draft: DraftAnswer | undefined): AskAnswerModel {
  return {
    questionId: question.id,
    selected: draft?.selected ?? [],
    text: draft?.text ?? '',
  };
}

function approvalHint(cell: AskCellModel): string {
  switch (cell.state) {
    case 'awaiting':
      return 'A tool wants to run a command that needs approval';
    case 'answered':
      return 'Decision sent';
    default:
      return 'Cancelled';
  }
}
