// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-user-questions/src/client/QuestionReplyView.tsx (+ .module.css)
// Modified for Wing: the slot contract (`PropsRuntime` / `PropsLocale` / `node.data`)
// is replaced by plain props over this package's `AskQuestionModel` /
// `AskAnswerModel`; `Tooltip` is replaced by the native `title` + `aria-label`
// treatment the other ported cards use; `t()` is replaced by labels.

import { memo, useCallback, useState } from 'react';
import clsx from 'clsx';

import type { AskAnswerModel, AskQuestionModel } from '@wing-agent/session';
import {
  IconCheckOutlineRegular,
  IconChevronDownOutlineRegular,
  IconChevronRightOutlineRegular,
  IconCopyOutlineRegular,
} from '../icons';
import { writeClipboard } from '../tool/clipboard';
import { replyAnswerValues, replyClipboardText, type QuestionReplyLabels } from './question-reply';
import css from './QuestionReplyView.module.css';

/** Dwell time of the copied confirmation, matching the other copy controls. */
const COPIED_FEEDBACK_MS = 1000;

/** Copy of a settled reply bubble; every string is a prop. */
export interface QuestionReplyViewLabels extends QuestionReplyLabels {
  /** Header line naming what the bubble holds. */
  readonly label: string;
  /** Accessible action while the details are closed. */
  readonly open: string;
  /** Accessible action while the details are open. */
  readonly close: string;
  readonly copy: string;
  readonly copied: string;
}

/** The renderer's default reply copy (English, like every UI string it ships). */
export const DEFAULT_QUESTION_REPLY_LABELS: QuestionReplyViewLabels = {
  label: 'Reply to earlier pending questions',
  open: 'Open question details',
  close: 'Close question details',
  copy: 'Copy',
  copied: 'Copied',
  answerLabel: 'Answer: ',
  skipped: 'Skipped',
};

export interface QuestionReplyViewProps {
  /** Questions of the ask, in order. */
  readonly questions: readonly AskQuestionModel[];
  /** Answers recorded by the host. */
  readonly answers: readonly AskAnswerModel[];
  /** Epoch ms of the reply, when the host knows it; the time row is omitted without it. */
  readonly time?: number | null;
  readonly labels?: QuestionReplyViewLabels | undefined;
  readonly className?: string | undefined;
}

/**
 * Render one settled question reply as a compact, right-aligned transcript bubble:
 * a label naming the earlier pending questions, the answer summary while collapsed,
 * one question/answer pair per question when open, and a copy control.
 * @param props - questions, recorded answers and copy.
 * @returns The expandable reply bubble.
 */
export const QuestionReplyView = memo(function QuestionReplyView({
  questions,
  answers,
  time = null,
  labels = DEFAULT_QUESTION_REPLY_LABELS,
  className,
}: QuestionReplyViewProps) {
  const [open, setOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const summary = questions
    .map((question) => replyAnswerValues(answers, question.id))
    .flat()
    .join(', ');
  const copyText = replyClipboardText(questions, answers, labels);
  const onCopy = useCallback(() => {
    if (copied) return;
    void writeClipboard(copyText).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => {
        setCopied(false);
      }, COPIED_FEEDBACK_MS);
    });
  }, [copied, copyText]);

  const toggleLabel = open ? labels.close : labels.open;
  return (
    <div className={clsx(css.row, className)} data-question-reply="" role="group" aria-label={labels.label}>
      <div className={css.bubble}>
        <button
          type="button"
          className={css.toggle}
          aria-expanded={open}
          aria-label={`${labels.label} · ${toggleLabel}`}
          title={toggleLabel}
          onClick={() => {
            setOpen((value) => !value);
          }}
        >
          <span className={css.labelRow}>
            {/* The caret is the only sign the bubble opens; the label alone
                reads as plain text. Its state is on the button already. */}
            <span className={css.caret} aria-hidden="true">
              {open ? <IconChevronDownOutlineRegular /> : <IconChevronRightOutlineRegular />}
            </span>
            <span className={css.label}>{labels.label}</span>
          </span>
          {!open && <span className={css.summary}>{summary === '' ? labels.skipped : summary}</span>}
        </button>
        {open &&
          (questions.length === 0 ? (
            <p className={css.text}>{summary}</p>
          ) : (
            <dl className={css.details}>
              {questions.map((question) => {
                const values = replyAnswerValues(answers, question.id);
                return (
                  <div key={question.id}>
                    <dt className={css.question}>
                      {question.header && question.header !== question.question && (
                        <span className={css.header}>{question.header}</span>
                      )}
                      <span className={css.questionText}>{question.question}</span>
                      {question.options.length > 0 && (
                        <ul className={css.options}>
                          {question.options.map((option) => (
                            <li key={option.label}>
                              {option.label}
                              {option.description && ` — ${option.description}`}
                            </li>
                          ))}
                        </ul>
                      )}
                    </dt>
                    <dd className={css.answer}>
                      <span className={css.answerLabel}>{labels.answerLabel}</span>
                      {values.length === 0 ? labels.skipped : values.join(', ')}
                    </dd>
                  </div>
                );
              })}
            </dl>
          ))}
      </div>
      <div className={css.actions}>
        {time === null ? null : (
          <span className={css.time}>
            {new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', hour12: false }).format(
              time,
            )}
          </span>
        )}
        <button
          type="button"
          className={css.action}
          aria-label={copied ? labels.copied : labels.copy}
          title={copied ? labels.copied : labels.copy}
          data-copied={copied ? 'true' : 'false'}
          onClick={onCopy}
        >
          {copied ? <IconCheckOutlineRegular size={15} /> : <IconCopyOutlineRegular size={15} />}
        </button>
      </div>
    </div>
  );
});
