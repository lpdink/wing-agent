// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-user-questions/src/client/question-reply.ts
// Modified for Wing: the upstream suite goes through the session/reply projections
// (`replyPairsOf` / `replySource`) this package does not port, so the cases spell the
// trimmed projections out directly — over `AskQuestionModel` / `AskAnswerModel` and
// with the labels as arguments instead of a locale seat. The render half of the
// bubble lives beside this file, in `question-reply-view.test.tsx` — named after the
// component rather than after this module, because a `question-reply.test.tsx`
// sibling would collide with this file's basename and typescript-eslint's
// projectService refuses to resolve the `.tsx` half (`was not found by the project
// service`). Keep the two basenames distinct.

import { describe, expect, it } from 'vitest';

import type { AskAnswerModel, AskQuestionModel } from '@wing-agent/session';
import { replyAnswerValues, replyClipboardText } from '../../src/index';

const QUESTIONS: readonly AskQuestionModel[] = [
  {
    id: 'q1',
    question: 'Which database?',
    header: 'DB',
    multiSelect: false,
    options: [{ label: 'Postgres', description: 'Managed' }],
    required: false,
  },
  {
    id: 'q2',
    question: 'Ship it?',
    header: '',
    multiSelect: false,
    options: [],
    required: false,
  },
];

const ANSWERS: readonly AskAnswerModel[] = [
  { questionId: 'q1', selected: ['Postgres'], text: '' },
  { questionId: 'q2', selected: [], text: 'Yes, after lunch' },
];

const LABELS = { answerLabel: 'Answer: ', skipped: 'Skipped' };

describe('replyAnswerValues', () => {
  it('returns the selected labels followed by a non-blank free-form answer', () => {
    expect(replyAnswerValues(ANSWERS, 'q1')).toEqual(['Postgres']);
    expect(replyAnswerValues(ANSWERS, 'q2')).toEqual(['Yes, after lunch']);
  });

  it('keeps the selection order and trims the free-form answer', () => {
    expect(replyAnswerValues([{ questionId: 'q1', selected: ['b', 'a'], text: '  both  ' }], 'q1')).toEqual([
      'b',
      'a',
      'both',
    ]);
  });

  it('drops a blank free-form answer and an unknown question', () => {
    expect(replyAnswerValues([{ questionId: 'q1', selected: [], text: '   ' }], 'q1')).toEqual([]);
    expect(replyAnswerValues(ANSWERS, 'q3')).toEqual([]);
    expect(replyAnswerValues([], 'q1')).toEqual([]);
  });
});

describe('replyClipboardText', () => {
  it('writes one block per question, in the order the ask asked them', () => {
    expect(replyClipboardText(QUESTIONS, ANSWERS, LABELS)).toBe(
      'DB — Which database?\nAnswer: Postgres\n\nShip it?\nAnswer: Yes, after lunch',
    );
  });

  it('repeats the question when the header adds nothing, and marks a skipped answer', () => {
    // `header === question` must not print the question twice.
    const duplicated: AskQuestionModel = { ...(QUESTIONS[0] as AskQuestionModel), header: 'Which database?' };
    expect(replyClipboardText([duplicated], [], LABELS)).toBe('Which database?\nSkipped');
    expect(replyClipboardText([QUESTIONS[1] as AskQuestionModel], [], LABELS)).toBe('Ship it?\nSkipped');
  });

  it('joins every selected label of a multi-select answer', () => {
    const answers: readonly AskAnswerModel[] = [
      { questionId: 'extras', selected: ['Metrics', 'Tracing'], text: '' },
    ];
    const question: AskQuestionModel = {
      id: 'extras',
      question: 'Which extras?',
      header: 'Extras',
      multiSelect: true,
      options: [],
      required: false,
    };
    expect(replyClipboardText([question], answers, LABELS)).toBe(
      'Extras — Which extras?\nAnswer: Metrics, Tracing',
    );
  });
});
