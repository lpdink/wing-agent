import { describe, expect, it } from 'vitest';

import type { AskQuestionModel } from '../../src/shared';
import { draftAfterOptionClick, draftAfterTextEdit } from '../../src/webview/chat/askDraft';

/**
 * The draft transitions behind "the last explicit choice wins" (#113).
 *
 * The host's `askAnswerValue` reads single-select answers option-first — the
 * TUI's `AskPanel::answer_value` shape — so a draft carrying both an option and
 * text is only sound if the webview never produces one. These tests pin the
 * mutual exclusion (single-select) and the combination (multi-select) as pure
 * arithmetic; `cells.test.tsx` pins the same behavior through the real UI.
 */

/** A free-form-capable question (`required: false`), single select by default. */
function question(overrides: Partial<AskQuestionModel> = {}): AskQuestionModel {
  return {
    id: 'q-open',
    header: 'Rendering',
    question: 'Which rendering strategy should the transcript use?',
    multiSelect: false,
    required: false,
    options: [
      { label: 'Memoized cells', description: '' },
      { label: 'Full re-render', description: '' },
    ],
    ...overrides,
  };
}

describe('ask draft transitions (#113)', () => {
  it('single-select: choosing an option drops a typed answer', () => {
    const typed = draftAfterTextEdit(question(), undefined, 'a third way');
    expect(typed).toEqual({ selected: [], text: 'a third way' });

    expect(draftAfterOptionClick(question(), typed, 'Memoized cells')).toEqual({
      selected: ['Memoized cells'],
      text: '',
    });
  });

  it('single-select: typing drops the committed option', () => {
    const chosen = draftAfterOptionClick(question(), undefined, 'Memoized cells');
    expect(chosen).toEqual({ selected: ['Memoized cells'], text: '' });

    expect(draftAfterTextEdit(question(), chosen, 'a third way')).toEqual({
      selected: [],
      text: 'a third way',
    });
  });

  it('single-select: a second option replaces the first', () => {
    const first = draftAfterOptionClick(question(), undefined, 'Memoized cells');
    expect(draftAfterOptionClick(question(), first, 'Full re-render')).toEqual({
      selected: ['Full re-render'],
      text: '',
    });
  });

  it('multi-select: toggles accumulate, and text and toggles coexist', () => {
    const typed = draftAfterTextEdit(question({ multiSelect: true }), undefined, 'and this');
    const one = draftAfterOptionClick(question({ multiSelect: true }), typed, 'Memoized cells');
    expect(one).toEqual({ selected: ['Memoized cells'], text: 'and this' });

    const two = draftAfterOptionClick(question({ multiSelect: true }), one, 'Full re-render');
    expect(two).toEqual({ selected: ['Memoized cells', 'Full re-render'], text: 'and this' });

    // Untoggling keeps the text draft; typing keeps the toggles.
    expect(draftAfterOptionClick(question({ multiSelect: true }), two, 'Memoized cells')).toEqual({
      selected: ['Full re-render'],
      text: 'and this',
    });
    expect(draftAfterTextEdit(question({ multiSelect: true }), two, 'x')).toEqual({
      selected: ['Memoized cells', 'Full re-render'],
      text: 'x',
    });
  });
});
