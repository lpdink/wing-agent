/**
 * Draft state for one ask question, and the transitions that keep it honest.
 *
 * The webview owns the in-progress answer until Submit; the TUI owns the same
 * state in `AskPanel`'s `QuestionState`. Both share one invariant the host's
 * `askAnswerValue` relies on: on a **single-select** question the fixed option
 * and the free-form text are mutually exclusive — the last explicit choice
 * wins (TUI `commit_option` / `confirm_editing`; §5 of
 * `docs/dev/vscode-extension.md` notes the same semantics). Without it, a
 * draft carrying both is representable and the option-first host rule
 * silently drops the text (#113).
 *
 * Multi-select questions combine toggles and text at submission (the TUI does
 * the same), so the two fields stay orthogonal there — the component tests in
 * `tests/webview/cells.test.tsx` pin both sides.
 */

import type { AskQuestionModel } from '../../shared';

/** Local draft of one question's answer. */
export interface AskDraft {
  readonly selected: readonly string[];
  readonly text: string;
}

/**
 * Draft after the user clicks a fixed option.
 *
 * Multi-select toggles the option (accumulating, in option order at read-back);
 * single-select replaces the selection with it — and, being the latest explicit
 * choice, clears the free-form text.
 */
export function draftAfterOptionClick(
  question: AskQuestionModel,
  draft: AskDraft | undefined,
  optionLabel: string,
): AskDraft {
  const current = draft?.selected ?? [];
  if (question.multiSelect) {
    const selected = current.includes(optionLabel)
      ? current.filter((label) => label !== optionLabel)
      : [...current, optionLabel];
    // Toggles and text combine at submission — keep the text draft.
    return { selected, text: draft?.text ?? '' };
  }
  return { selected: [optionLabel], text: '' };
}

/**
 * Draft after the user edits the free-form field.
 *
 * Single-select: typing is the latest explicit choice, so it clears the
 * committed option. Multi-select: keep the toggles.
 */
export function draftAfterTextEdit(
  question: AskQuestionModel,
  draft: AskDraft | undefined,
  text: string,
): AskDraft {
  return {
    selected: question.multiSelect ? (draft?.selected ?? []) : [],
    text,
  };
}
