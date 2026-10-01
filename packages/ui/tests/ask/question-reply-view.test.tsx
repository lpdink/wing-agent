// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-user-questions/src/client/QuestionReplyView.tsx
// Modified for Wing: no locale seat and no session node — the cases drive the bubble
// through its props. The pure projections it renders with (`replyAnswerValues` /
// `replyClipboardText`) are covered next door, in `question-reply.test.ts`.

import { fireEvent, render, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import type { AskAnswerModel, AskQuestionModel } from '@wing-agent/session';
import { QuestionReplyView } from '../../src/index';

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

function stubClipboard(writeText: (text: string) => Promise<void>): void {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
}

describe('QuestionReplyView', () => {
  it('shows the collapsed summary of the recorded answers', () => {
    const view = render(<QuestionReplyView questions={QUESTIONS} answers={ANSWERS} />);
    expect(view.getByText('Reply to earlier pending questions')).toBeTruthy();
    expect(view.getByText('Postgres, Yes, after lunch')).toBeTruthy();
    expect(view.getByRole('button', { expanded: false })).toBeTruthy();
    expect(view.queryByText('Which database?')).toBeNull();
  });

  it('expands into one question/answer pair per question', () => {
    const view = render(<QuestionReplyView questions={QUESTIONS} answers={ANSWERS} />);
    fireEvent.click(view.getByRole('button', { expanded: false }));
    expect(view.getByText('Which database?')).toBeTruthy();
    expect(view.getByText('DB')).toBeTruthy();
    expect(view.getByText('Postgres — Managed')).toBeTruthy();
    expect(view.getAllByText('Answer:')).toHaveLength(2);
    expect(view.getByText('Yes, after lunch')).toBeTruthy();
    expect(view.queryByText('Postgres, Yes, after lunch')).toBeNull();
    expect(view.getByRole('button', { expanded: true })).toBeTruthy();
  });

  it('falls back to the skipped label for an empty answer', () => {
    const view = render(<QuestionReplyView questions={[QUESTIONS[1] as AskQuestionModel]} answers={[]} />);
    expect(view.getByText('Skipped')).toBeTruthy();
  });

  it('copies the whole reply and reports success', async () => {
    const writeText = vi.fn(async () => {});
    stubClipboard(writeText);
    const view = render(<QuestionReplyView questions={QUESTIONS} answers={ANSWERS} />);
    fireEvent.click(view.getByRole('button', { name: 'Copy' }));
    await waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(
        'DB — Which database?\nAnswer: Postgres\n\nShip it?\nAnswer: Yes, after lunch',
      );
    });
    await waitFor(() => {
      expect(view.getByRole('button', { name: 'Copied' })).toBeTruthy();
    });
  });

  it('renders the reply time only when the host knows it', () => {
    const stamped = render(<QuestionReplyView questions={QUESTIONS} answers={ANSWERS} time={0} />);
    // Locale-independent: the host formats HH:MM, the zone may be anything.
    expect(stamped.container.querySelector('[class*="time"]')?.textContent).toMatch(/^\d{2}:\d{2}$/);
    const unstamped = render(<QuestionReplyView questions={QUESTIONS} answers={ANSWERS} />);
    expect(unstamped.container.querySelector('[class*="time"]')).toBeNull();
  });
});
