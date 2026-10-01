// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-user-questions/tests/question-composer.client.spec.tsx
// Modified for Wing: no draft store, no countdown and no plan review — the cases cover
// the trimmed flow (pager, options, recommended suffix, free-form text with IME
// protection, submit mapping) and assert answers as this package's `AskAnswerModel[]`.

import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import type { AskQuestionModel } from '@wing-agent/session';
import { QuestionComposer, parseRecommendedLabel } from '../../src/index';

function option(label: string, description = ''): { label: string; description: string } {
  return { label, description };
}

function question(overrides: Partial<AskQuestionModel> & { id: string }): AskQuestionModel {
  return {
    question: `Question ${overrides.id}?`,
    header: overrides.id.toUpperCase(),
    multiSelect: false,
    options: [],
    required: false,
    ...overrides,
  };
}

const TWO_QUESTIONS: readonly AskQuestionModel[] = [
  question({ id: 'q1', options: [option('Fast (recommended)'), option('Safe', 'Checks everything')] }),
  question({ id: 'q2', multiSelect: true, options: [option('Alpha'), option('Beta')] }),
];

describe('parseRecommendedLabel', () => {
  it.each([
    ['Fast (recommended)', 'Fast', true],
    ['Fast（推荐）', 'Fast', true],
    ['Fast (RECOMMENDED)', 'Fast', true],
    ['Fast (Recommended) ', 'Fast', true],
    ['Fast', 'Fast', false],
    ['Fast (optional)', 'Fast (optional)', false],
  ])('parses %j', (label, display, recommended) => {
    expect(parseRecommendedLabel(label)).toEqual({ label: display, recommended });
  });
});

describe('QuestionComposer', () => {
  it('renders one question card at a time with its pager', () => {
    const view = render(
      <QuestionComposer requestId="ask-1" questions={TWO_QUESTIONS} state="awaiting" onSubmit={() => {}} />,
    );
    expect(view.getByRole('heading', { name: 'Question q1?' })).toBeTruthy();
    expect(view.getByText('Q1')).toBeTruthy();
    expect(view.getByText('1 / 2')).toBeTruthy();
    expect(view.queryByText('Question q2?')).toBeNull();
    expect(view.container.querySelector('[data-question-key="ask-1"]')).not.toBeNull();
  });

  it('moves on by itself after a single choice and keeps the selection when walking back', () => {
    const view = render(
      <QuestionComposer requestId="ask-1" questions={TWO_QUESTIONS} state="awaiting" onSubmit={() => {}} />,
    );
    fireEvent.click(view.getByRole('radio', { name: 'Fast' }));
    expect(view.getByText('2 / 2')).toBeTruthy();
    expect(view.getByRole('heading', { name: 'Question q2?' })).toBeTruthy();

    fireEvent.click(view.getByRole('button', { name: 'Previous question' }));
    expect(view.getByText('1 / 2')).toBeTruthy();
    expect(view.getByRole('radio', { name: 'Fast' }).getAttribute('aria-checked')).toBe('true');
  });

  it('keeps the recommended suffix in the option label and strips it from the copy', () => {
    const view = render(
      <QuestionComposer requestId="ask-1" questions={TWO_QUESTIONS} state="awaiting" onSubmit={() => {}} />,
    );
    expect(view.getByText('Fast')).toBeTruthy();
    expect(view.queryByText('Fast (recommended)')).toBeNull();
    expect(view.getByText('Recommended')).toBeTruthy();
    expect(view.getByText('Checks everything')).toBeTruthy();
  });

  it('toggles several labels on a multi-select question without advancing', () => {
    const view = render(
      <QuestionComposer
        requestId="ask-1"
        questions={[TWO_QUESTIONS[1] as AskQuestionModel]}
        state="awaiting"
        onSubmit={() => {}}
      />,
    );
    const alpha = view.getByRole('checkbox', { name: 'Alpha' });
    const beta = view.getByRole('checkbox', { name: 'Beta' });
    fireEvent.click(alpha);
    fireEvent.click(beta);
    expect(alpha.getAttribute('aria-checked')).toBe('true');
    expect(beta.getAttribute('aria-checked')).toBe('true');
    fireEvent.click(alpha);
    expect(alpha.getAttribute('aria-checked')).toBe('false');
  });

  it('selects the focused option on Enter instead of submitting the batch empty', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', options: [option('One'), option('Two')] }),
      question({ id: 'q2', options: [option('X')] }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    const one = view.getByRole('radio', { name: 'One' });
    one.focus();
    fireEvent.keyDown(one, { key: 'Enter' });

    // The keyboard path is the mouse path: the focused option is *chosen* (and a
    // single choice walks on). It is never dropped, and nothing is submitted — the
    // regression this pins sent the whole batch empty instead.
    expect(onSubmit).not.toHaveBeenCalled();
    expect(view.getByText('2 / 2')).toBeTruthy();
    fireEvent.click(view.getByRole('button', { name: 'Previous question' }));
    expect(view.getByRole('radio', { name: 'One' }).getAttribute('aria-checked')).toBe('true');
  });

  it('toggles a multi-select option once per Enter and still never submits', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', multiSelect: true, options: [option('Alpha'), option('Beta')] }),
      question({ id: 'q2', options: [option('X')] }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    const alpha = view.getByRole('checkbox', { name: 'Alpha' });
    alpha.focus();
    fireEvent.keyDown(alpha, { key: 'Enter' });
    // One keypress, one toggle: the handler cancels the browser's implicit click, so
    // a real double activation cannot silently undo the choice.
    expect(alpha.getAttribute('aria-checked')).toBe('true');
    fireEvent.keyDown(alpha, { key: 'Enter' });
    expect(alpha.getAttribute('aria-checked')).toBe('false');
    expect(view.getByText('1 / 2')).toBeTruthy();
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('carries keyboard-made choices to the Submit button, the only submit path', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', options: [option('One'), option('Two')] }),
      question({ id: 'q2', options: [option('X'), option('Y')] }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    const one = view.getByRole('radio', { name: 'One' });
    one.focus();
    fireEvent.keyDown(one, { key: 'Enter' });
    const lastChoice = view.getByRole('radio', { name: 'Y' });
    lastChoice.focus();
    fireEvent.keyDown(lastChoice, { key: 'Enter' });

    // On the last question Enter selects and stays (the mouse does the same); the
    // batch leaves only when Submit is activated, with both choices intact.
    expect(onSubmit).not.toHaveBeenCalled();
    expect(view.getByText('2 / 2')).toBeTruthy();
    fireEvent.click(view.getByRole('button', { name: 'Submit' }));
    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit.mock.calls[0]?.[0]).toEqual([
      { questionId: 'q1', selected: ['One'], text: '' },
      { questionId: 'q2', selected: ['Y'], text: '' },
    ]);
  });

  it('lets Enter on a required option satisfy the gate without submitting it', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', required: true, options: [option('y'), option('n')] }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    const yes = view.getByRole('radio', { name: 'y' });
    yes.focus();
    fireEvent.keyDown(yes, { key: 'Enter' });
    expect(onSubmit).not.toHaveBeenCalled();
    expect(yes.getAttribute('aria-checked')).toBe('true');
    expect(view.getByRole('button', { name: 'Submit' })).toHaveProperty('disabled', false);
  });

  it('submits one answer per question in order, with the free-form text', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', options: [option('Fast'), option('Safe')] }),
      question({ id: 'q2' }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    fireEvent.click(view.getByRole('radio', { name: 'Safe' }));
    fireEvent.change(view.getByPlaceholderText('Type your answer'), {
      target: { value: ' Use the staging box ' },
    });
    fireEvent.click(view.getByRole('button', { name: 'Submit' }));
    expect(onSubmit).toHaveBeenCalledTimes(1);
    expect(onSubmit.mock.calls[0]?.[0]).toEqual([
      { questionId: 'q1', selected: ['Safe'], text: '' },
      { questionId: 'q2', selected: [], text: ' Use the staging box ' },
    ]);
  });

  it('clears a single-select choice when a free-form answer replaces it', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [question({ id: 'q1', options: [option('Fast')] })];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    fireEvent.click(view.getByRole('radio', { name: 'Fast' }));
    fireEvent.change(view.getByPlaceholderText('Type your answer'), { target: { value: 'Neither' } });
    fireEvent.click(view.getByRole('button', { name: 'Submit' }));
    expect(onSubmit.mock.calls[0]?.[0]).toEqual([{ questionId: 'q1', selected: [], text: 'Neither' }]);
  });

  it('advances on Enter from the free-form field but never mid-composition', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [question({ id: 'q1' }), question({ id: 'q2' })];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    const field = view.getByPlaceholderText('Type your answer');
    fireEvent.change(field, { target: { value: 'First answer' } });

    // The browser marks the keydown that confirms an IME candidate with
    // `isComposing` (and engines without it send the legacy 229); jsdom needs the flag
    // passed explicitly. Both spellings must leave the draft alone.
    fireEvent.keyDown(field, { key: 'Enter', isComposing: true });
    expect(view.getByText('1 / 2')).toBeTruthy();
    fireEvent.keyDown(field, { key: 'Enter', keyCode: 229 });
    expect(view.getByText('1 / 2')).toBeTruthy();

    fireEvent.keyDown(field, { key: 'Enter' });
    expect(view.getByText('2 / 2')).toBeTruthy();

    fireEvent.keyDown(view.getByPlaceholderText('Type your answer'), { key: 'Enter', shiftKey: true });
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('skips the current question and submits the batch from the last one', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', options: [option('Fast')] }),
      question({ id: 'q2', options: [option('Safe')] }),
      question({ id: 'q3' }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    // Skip on the first question clears it and walks on; the choice on the second
    // advances by itself, and the last question submits the whole batch.
    fireEvent.click(view.getByRole('button', { name: 'Skip' }));
    expect(view.getByText('2 / 3')).toBeTruthy();
    fireEvent.click(view.getByRole('radio', { name: 'Safe' }));
    expect(view.getByText('3 / 3')).toBeTruthy();
    fireEvent.click(view.getByRole('button', { name: 'Submit' }));
    expect(onSubmit.mock.calls[0]?.[0]).toEqual([
      { questionId: 'q1', selected: [], text: '' },
      { questionId: 'q2', selected: ['Safe'], text: '' },
      { questionId: 'q3', selected: [], text: '' },
    ]);
  });

  it('gates a required question on a choice and offers no free-form row', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', required: true, options: [option('y'), option('n')] }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    expect(view.queryByPlaceholderText('Type your answer')).toBeNull();
    expect(view.getByRole('button', { name: 'Submit' })).toHaveProperty('disabled', true);
    fireEvent.click(view.getByRole('radio', { name: 'y' }));
    expect(view.getByRole('button', { name: 'Submit' })).toHaveProperty('disabled', false);
    fireEvent.click(view.getByRole('button', { name: 'Submit' }));
    expect(onSubmit.mock.calls[0]?.[0]).toEqual([{ questionId: 'q1', selected: ['y'], text: '' }]);
  });

  it('reports why an incomplete required question could not continue', () => {
    const onSubmit = vi.fn();
    const questions: readonly AskQuestionModel[] = [
      question({ id: 'q1', required: true, options: [option('y'), option('n')] }),
    ];
    const view = render(
      <QuestionComposer requestId="ask-1" questions={questions} state="awaiting" onSubmit={onSubmit} />,
    );
    fireEvent.click(view.getByRole('button', { name: 'Skip' }));
    expect(view.getByRole('status').textContent).toBe('Please select an option or enter a custom answer.');
    expect(onSubmit).not.toHaveBeenCalled();
  });

  it('renders a free-form-only question as a framed block field', () => {
    const view = render(
      <QuestionComposer
        requestId="ask-1"
        questions={[question({ id: 'q1' })]}
        state="awaiting"
        onSubmit={() => {}}
      />,
    );
    const field = view.getByPlaceholderText('Type your answer');
    expect(field.className).toContain('fieldInput');
    expect(view.getByRole('button', { name: 'Submit' })).toBeTruthy();
  });

  it('locks every input once the ask is no longer awaiting', () => {
    const onSubmit = vi.fn();
    const view = render(
      <QuestionComposer requestId="ask-1" questions={TWO_QUESTIONS} state="answered" onSubmit={onSubmit} />,
    );
    expect(view.getByRole('radio', { name: 'Fast' })).toHaveProperty('disabled', true);
    expect(view.getByPlaceholderText('Type your answer')).toHaveProperty('disabled', true);
    expect(view.getByRole('button', { name: 'Next' })).toHaveProperty('disabled', true);
    expect(view.getByRole('button', { name: 'Skip' })).toHaveProperty('disabled', true);
    fireEvent.click(view.getByRole('radio', { name: 'Fast' }));
    fireEvent.click(view.getByRole('button', { name: 'Next' }));
    expect(onSubmit).not.toHaveBeenCalled();
    expect(view.container.querySelector('[data-question-state="answered"]')).not.toBeNull();
  });

  it('accepts copy overrides and renders nothing without questions', () => {
    const view = render(
      <QuestionComposer
        requestId="ask-1"
        questions={[question({ id: 'q1', options: [option('One')] })]}
        state="awaiting"
        onSubmit={() => {}}
        labels={{
          fallbackHeader: 'Question',
          recommended: 'Best',
          customPlaceholder: 'Else…',
          skip: 'Pass',
          next: 'Forward',
          submit: 'Send',
          prevAria: 'Back',
          nextAria: 'Forward',
          unanswered: 'Pick one',
        }}
      />,
    );
    expect(view.getByRole('button', { name: 'Pass' })).toBeTruthy();
    expect(view.getByRole('button', { name: 'Send' })).toBeTruthy();
    expect(view.getByPlaceholderText('Else…')).toBeTruthy();
    expect(view.getByRole('button', { name: 'Back' })).toHaveProperty('disabled', true);

    const empty = render(
      <QuestionComposer requestId="ask-2" questions={[]} state="awaiting" onSubmit={() => {}} />,
    );
    expect(empty.container.textContent).toBe('');
  });
});
