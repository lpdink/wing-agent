// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-user-questions/src/client/QuestionComposer.tsx (+ .module.css)
// Modified for Wing: the draft-store (`useStore`), the countdown/wait channel
// (`useQuestionCard`), the plan-review takeover, minimize/close and the
// MarkdownText detail block are dropped — Wing's ask model carries no detail, no
// timer and no dismissal intent. What stays is the ported flow: one question card at
// a time with a pager, options (radio / checkbox), the `(recommended)` label
// parsing, the auto-growing free-form field with IME protection, and the
// submit-time completeness check. `t()` is replaced by `QuestionComposerLabels`;
// answers are this package's `AskAnswerModel[]` (one per question, in order).

import { memo, useCallback, useEffect, useId, useState, type ChangeEvent, type KeyboardEvent } from 'react';
import clsx from 'clsx';

import type { AskAnswerModel, AskQuestionModel, AskState } from '@wing-agent/session';
import { Button } from '../components/Button';
import {
  IconCheckOutlineRegular,
  IconChevronLeftOutlineRegular,
  IconChevronRightOutlineRegular,
  IconEditOutlineRegular,
} from '../icons';
import css from './QuestionComposer.module.css';

/**
 * Split the conventional recommendation suffix without changing the answer value.
 * Both the English and the Chinese suffix of the upstream contract are accepted.
 * @param label - Original option label returned if selected.
 * @returns Display label plus recommendation state.
 */
export function parseRecommendedLabel(label: string): { label: string; recommended: boolean } {
  const suffix = /\s*(?:\((?:recommended|推荐)\)|（(?:recommended|推荐)）)\s*$/i;
  return suffix.test(label)
    ? { label: label.replace(suffix, ''), recommended: true }
    : { label, recommended: false };
}

/** Copy of one question composer; every string is a prop (Wing has no runtime locale). */
export interface QuestionComposerLabels {
  /** Shown as the eyebrow when a question carries no header. */
  readonly fallbackHeader: string;
  /** Badge next to an option whose label ends in `(recommended)`. */
  readonly recommended: string;
  /** Empty-field prompt of the free-form answer. */
  readonly customPlaceholder: string;
  readonly skip: string;
  readonly next: string;
  readonly submit: string;
  readonly prevAria: string;
  readonly nextAria: string;
  /** Feedback when the current question still needs an answer. */
  readonly unanswered: string;
}

/** The renderer's default question copy (English, like every UI string it ships). */
export const DEFAULT_QUESTION_COMPOSER_LABELS: QuestionComposerLabels = {
  fallbackHeader: 'Question',
  recommended: 'Recommended',
  customPlaceholder: 'Type your answer',
  skip: 'Skip',
  next: 'Next',
  submit: 'Submit',
  prevAria: 'Previous question',
  nextAria: 'Next question',
  unanswered: 'Please select an option or enter a custom answer.',
};

export interface QuestionComposerProps {
  /** Correlation id of the ask: the DOM key and the draft reset key. */
  readonly requestId: string;
  /** Questions of the ask, in order. */
  readonly questions: readonly AskQuestionModel[];
  /** Lifecycle: the form only accepts input while `awaiting`. */
  readonly state: AskState;
  readonly labels?: QuestionComposerLabels | undefined;
  /** Submit the batch — one answer per question, in question order. */
  readonly onSubmit: (answers: readonly AskAnswerModel[]) => void;
  readonly className?: string | undefined;
}

/** Local draft of one question's answer. */
interface DraftAnswer {
  readonly selected: readonly string[];
  readonly text: string;
}

/** The draft of a question the reader has not touched yet. */
const EMPTY_DRAFT: DraftAnswer = { selected: [], text: '' };

/** Accept persisted progress only when it still describes the current questions. */
function emptyDrafts(questions: readonly AskQuestionModel[]): Record<string, DraftAnswer> {
  const drafts: Record<string, DraftAnswer> = {};
  for (const question of questions) {
    drafts[question.id] = { selected: [], text: '' };
  }
  return drafts;
}

/**
 * Return whether a text-field key event belongs to an active IME composition.
 *
 * `keyCode 229` is the legacy signal engines emit without `isComposing`; both are
 * checked so a composition never submits the batch mid-character (Chinese, Japanese
 * and Korean input rely on it).
 */
function isComposing(event: KeyboardEvent<HTMLTextAreaElement>): boolean {
  const native = event.nativeEvent as KeyboardEvent['nativeEvent'] & { keyCode?: number };
  return native.isComposing || native.keyCode === 229;
}

/**
 * Only *required* questions gate the submit button: they are the option-only ones,
 * where "no answer" is not a valid response. Free-form questions may be left empty
 * (the gateway turns a missing answer into `(user did not answer)`).
 */
function isAnswered(question: AskQuestionModel, draft: DraftAnswer | undefined): boolean {
  if (!question.required) return true;
  return (draft?.selected.length ?? 0) > 0;
}

/** The free-text answer field shared by both question variants. */
function AnswerField({
  variant,
  value,
  placeholder,
  disabled,
  onChange,
  onKeyDown,
}: {
  variant: 'inline' | 'block';
  value: string;
  placeholder: string;
  disabled: boolean;
  onChange: (event: ChangeEvent<HTMLTextAreaElement>) => void;
  onKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void;
}) {
  return (
    <div className={clsx(css.field, variant === 'inline' ? css.customInline : css.customBlock)}>
      <div aria-hidden className={css.fieldMirror}>{`${value}\n`}</div>
      <textarea
        className={css.fieldInput}
        value={value}
        disabled={disabled}
        rows={1}
        placeholder={placeholder}
        onChange={onChange}
        onKeyDown={onKeyDown}
      />
    </div>
  );
}

/**
 * The multi-question ask, one card at a time.
 *
 * The form owns only a draft: every question keeps its own selection and free-form
 * text while the reader walks the pager, and nothing leaves the component until the
 * final Submit hands the batch to `onSubmit`. The gateway is the authority on
 * whether the ask is still open (`state`); once it is not `awaiting` the form locks.
 * @param props - questions, lifecycle, copy and the submit sink.
 * @returns The question card.
 */
export const QuestionComposer = memo(function QuestionComposer({
  requestId,
  questions,
  state,
  labels = DEFAULT_QUESTION_COMPOSER_LABELS,
  onSubmit,
  className,
}: QuestionComposerProps) {
  const [drafts, setDrafts] = useState<Record<string, DraftAnswer>>(() => emptyDrafts(questions));
  const [index, setIndex] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const titleId = useId();
  const locked = state !== 'awaiting';

  // A new ask (same mounted slot) starts from a clean draft.
  useEffect(() => {
    setDrafts(emptyDrafts(questions));
    setIndex(0);
    setError(null);
  }, [requestId, questions]);

  const current = Math.min(index, Math.max(questions.length - 1, 0));
  const question = questions[current];
  // Hooks stay above the empty-ask return: a component that switches between the two
  // shapes (a host that clears the questions) must keep the same hook order.
  const draft: DraftAnswer = (question === undefined ? undefined : drafts[question.id]) ?? EMPTY_DRAFT;
  const complete = questions.every((item) => isAnswered(item, drafts[item.id]));
  const last = current === questions.length - 1;

  const updateDraft = (update: (current: DraftAnswer) => DraftAnswer): void => {
    if (question === undefined) return;
    setDrafts((previous) => ({
      ...previous,
      [question.id]: update(previous[question.id] ?? EMPTY_DRAFT),
    }));
    setError(null);
  };

  const choose = (label: string, multiSelect: boolean): void => {
    updateDraft((current) => {
      if (multiSelect) {
        const selected = current.selected.includes(label)
          ? current.selected.filter((item) => item !== label)
          : [...current.selected, label];
        return { ...current, selected };
      }
      return { selected: [label], text: '' };
    });
    // A single choice moves on by itself unless it is the last question (upstream flow).
    if (!multiSelect && !last) {
      setIndex(current + 1);
    }
  };

  const submit = useCallback((): void => {
    const firstIncomplete = questions.findIndex((item) => !isAnswered(item, drafts[item.id]));
    if (firstIncomplete >= 0) {
      setIndex(firstIncomplete);
      setError(labels.unanswered);
      return;
    }
    onSubmit(
      questions.map((item) => {
        const answer = drafts[item.id] ?? EMPTY_DRAFT;
        return { questionId: item.id, selected: answer.selected, text: answer.text };
      }),
    );
  }, [drafts, labels.unanswered, onSubmit, questions]);

  const continueFlow = useCallback((): void => {
    if (question === undefined) return;
    if (!isAnswered(question, drafts[question.id])) {
      setError(labels.unanswered);
      return;
    }
    if (last) {
      submit();
      return;
    }
    setIndex(current + 1);
    setError(null);
  }, [current, drafts, labels.unanswered, last, question, submit]);

  if (question === undefined) {
    return null;
  }

  const hasOptions = question.options.length > 0;

  const skipQuestion = (): void => {
    updateDraft(() => EMPTY_DRAFT);
    if (last) {
      submit();
      return;
    }
    setIndex(current + 1);
  };

  const draftCustom = (event: ChangeEvent<HTMLTextAreaElement>): void => {
    const value = event.target.value;
    updateDraft((current) => ({
      ...current,
      // A multi-select draft retains checked labels; a single-select custom answer
      // replaces its selection.
      selected: question.multiSelect ? current.selected : [],
      text: value,
    }));
  };

  const continueFromCustom = (event: KeyboardEvent<HTMLTextAreaElement>): void => {
    if (event.key !== 'Enter' || event.shiftKey || isComposing(event)) return;
    event.preventDefault();
    continueFlow();
  };

  return (
    <div className={clsx(css.frame, className)} data-question-key={requestId}>
      <section className={css.card} aria-labelledby={titleId} data-question-state={state}>
        <header className={css.header}>
          <div className={css.headingBlock}>
            <div className={css.eyebrow}>
              {question.header === '' ? labels.fallbackHeader : question.header}
            </div>
            <h2 className={css.title} id={titleId}>
              {question.question}
            </h2>
          </div>
        </header>

        <div className={css.body} data-question-scroll>
          <div className={css.options} role={question.multiSelect ? 'group' : 'radiogroup'}>
            {question.options.map((option, optionIndex) => {
              const selected = draft.selected.includes(option.label);
              const display = parseRecommendedLabel(option.label);
              return (
                <button
                  type="button"
                  key={`${option.label}-${String(optionIndex)}`}
                  className={clsx(css.option, selected && !question.multiSelect && css.optionSelected)}
                  role={question.multiSelect ? 'checkbox' : 'radio'}
                  aria-checked={selected}
                  aria-label={display.label}
                  disabled={locked}
                  onClick={() => {
                    choose(option.label, question.multiSelect);
                  }}
                  onKeyDown={(event) => {
                    if (event.key !== 'Enter') return;
                    event.preventDefault();
                    submit();
                  }}
                >
                  {question.multiSelect ? (
                    <span className={clsx(css.checkbox, selected && css.checkboxChecked)} aria-hidden="true">
                      {selected && <IconCheckOutlineRegular size={12} />}
                    </span>
                  ) : (
                    <span className={css.number}>{optionIndex + 1}</span>
                  )}
                  <span className={css.optionCopy}>
                    <span className={css.optionLine}>
                      <span className={css.optionLabel}>{display.label}</span>
                      {display.recommended && <span className={css.badge}>{labels.recommended}</span>}
                      {option.description !== '' && (
                        <span className={css.description}>{option.description}</span>
                      )}
                    </span>
                  </span>
                </button>
              );
            })}

            {/* The free-form row: inline beside the options, framed on its own when the
                question has none. Required questions are option-only by contract. */}
            {hasOptions ? (
              !question.required && (
                <div className={clsx(css.customRow, draft.text !== '' && css.customRowActive)}>
                  {question.multiSelect ? (
                    <span
                      className={clsx(css.checkbox, draft.text !== '' && css.checkboxChecked)}
                      aria-hidden="true"
                    >
                      {draft.text !== '' && <IconCheckOutlineRegular size={12} />}
                    </span>
                  ) : (
                    <span className={css.number} aria-hidden="true">
                      <IconEditOutlineRegular size={12} />
                    </span>
                  )}
                  <AnswerField
                    variant="inline"
                    value={draft.text}
                    disabled={locked}
                    placeholder={labels.customPlaceholder}
                    onChange={draftCustom}
                    onKeyDown={continueFromCustom}
                  />
                </div>
              )
            ) : (
              <AnswerField
                variant="block"
                value={draft.text}
                disabled={locked}
                placeholder={labels.customPlaceholder}
                onChange={draftCustom}
                onKeyDown={continueFromCustom}
              />
            )}
          </div>
        </div>

        <footer className={css.footer}>
          <div className={css.pager}>
            <button
              type="button"
              className={css.iconButton}
              aria-label={labels.prevAria}
              title={labels.prevAria}
              disabled={current === 0}
              onClick={() => {
                setIndex(current - 1);
                setError(null);
              }}
            >
              <IconChevronLeftOutlineRegular />
            </button>
            <span className={css.progress}>
              {current + 1} / {questions.length}
            </span>
            <button
              type="button"
              className={css.iconButton}
              aria-label={labels.nextAria}
              title={labels.nextAria}
              disabled={last}
              onClick={() => {
                setIndex(current + 1);
                setError(null);
              }}
            >
              <IconChevronRightOutlineRegular />
            </button>
          </div>
          <div className={css.feedback} role="status">
            {error}
          </div>
          <div className={css.footerActions}>
            <Button variant="outline" disabled={locked} onClick={skipQuestion}>
              {labels.skip}
            </Button>
            <Button
              variant="primary"
              disabled={locked || (last ? !complete : !isAnswered(question, drafts[question.id]))}
              onClick={continueFlow}
            >
              {last ? labels.submit : labels.next}
            </Button>
          </div>
        </footer>
      </section>
    </div>
  );
});
