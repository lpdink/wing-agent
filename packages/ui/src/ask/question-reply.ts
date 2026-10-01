// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-user-questions/src/client/question-reply.ts
// Modified for Wing: trimmed to the two pure projections over *this* package's
// `AskQuestionModel` / `AskAnswerModel` (the upstream session projection —
// `replyPairsOf`, `replySource`, the conversation-node definition — belongs to a
// runtime Wing does not have); `t()` is replaced by `QuestionReplyLabels`.

import type { AskAnswerModel, AskQuestionModel } from '@wing-agent/session';

/**
 * Answer values of one question in display order: the selected option labels
 * followed by a non-blank free-form answer.
 * @param answers - Recorded answers (host echo).
 * @param questionId - Question id to read.
 * @returns The values, empty when the user skipped that question.
 */
export function replyAnswerValues(answers: readonly AskAnswerModel[], questionId: string): string[] {
  const answer = answers.find((item) => item.questionId === questionId);
  const custom = answer?.text.trim() ?? '';
  return [...(answer?.selected ?? []), ...(custom === '' ? [] : [custom])];
}

/** Copy of one reply bubble; every string is a prop (Wing has no runtime locale). */
export interface QuestionReplyLabels {
  /** Label of the answer line inside one block. */
  readonly answerLabel: string;
  /** Value written for a question the user left empty. */
  readonly skipped: string;
}

/**
 * Clipboard text of one reply: every question with the answer the user gave, in the
 * layout the open bubble shows, without the options nobody chose.
 * @param questions - Questions of the ask, in order.
 * @param answers - Recorded answers (host echo).
 * @param labels - Localized answer markers.
 * @returns One block per question.
 */
export function replyClipboardText(
  questions: readonly AskQuestionModel[],
  answers: readonly AskAnswerModel[],
  labels: QuestionReplyLabels,
): string {
  return questions
    .map((question) => {
      const values = replyAnswerValues(answers, question.id);
      const heading =
        question.header && question.header !== question.question
          ? `${question.header} — ${question.question}`
          : question.question;
      const answer = values.length === 0 ? labels.skipped : `${labels.answerLabel}${values.join(', ')}`;
      return `${heading}\n${answer}`;
    })
    .join('\n\n');
}
