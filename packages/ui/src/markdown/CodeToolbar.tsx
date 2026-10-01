// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/CodeToolbar.tsx
// Modified for Wing: the `Tooltip` dependency (a component this batch does not port)
// is replaced by the native `title` attribute; `status` widens from `string` to
// `ReactNode` so a card can pass coloured counters (DiffBlock's `+n/−m`); imports
// point at this package's icons and highlight module.

/** Shared language, wrapping, and clipboard controls for code cards. */
import type { ReactNode } from 'react';

import {
  IconCheckOutlineRegular,
  IconCopyOutlineRegular,
  IconNowrapFillRegular,
  IconWrapFillRegular,
} from '../icons';
import { supportsHighlighting } from '../chat/markdown/highlight';
import css from './CodeCard.module.css';

/** Localized language fallback and wrapping actions supplied by the card owner. */
export interface CodeToolbarLabels {
  /** Title for an absent or unsupported language. */
  codeLabel: string;
  /** Action that enables wrapping. */
  wrapLabel: string;
  /** Action that preserves source columns with horizontal scrolling. */
  unwrapLabel: string;
}

/** Display state and callbacks for a code card's toolbar. */
export interface CodeToolbarProps {
  lang?: string | undefined;
  /** Secondary label after the language, e.g. a file path (ellipsized). */
  title?: string | undefined;
  /** Supplementary slot before the actions, e.g. diff counters. */
  status?: ReactNode;
  labels: CodeToolbarLabels;
  copyLabel: string;
  copiedLabel: string;
  copied: boolean;
  wrapped: boolean;
  onCopy?: (() => void) | undefined;
  onWrap?: (() => void) | undefined;
}

/**
 * Render a language label and keyboard-accessible icon actions.
 * @param props - Localized labels, current state, and card-owned actions.
 * @returns The shared code-card header.
 */
export function CodeToolbar({
  lang,
  title,
  status,
  labels,
  copyLabel,
  copiedLabel,
  copied,
  wrapped,
  onCopy,
  onWrap,
}: CodeToolbarProps) {
  const wrapLabel = wrapped ? labels.unwrapLabel : labels.wrapLabel;
  const clipboardLabel = copied ? copiedLabel : copyLabel;
  return (
    <div className={css.header} data-code-block-banner>
      <div className={css.heading}>
        <span className={css.language}>{supportsHighlighting(lang) ? lang : labels.codeLabel}</span>
        {title !== undefined && (
          <span className={css.title} title={title}>
            {title}
          </span>
        )}
      </div>
      <div className={css.actions}>
        {status !== undefined && <span className={css.status}>{status}</span>}
        {onWrap !== undefined && (
          <button
            type="button"
            className={css.action}
            title={wrapLabel}
            aria-label={labels.wrapLabel}
            aria-pressed={wrapped}
            onClick={onWrap}
          >
            {wrapped ? <IconNowrapFillRegular size={14} /> : <IconWrapFillRegular size={14} />}
          </button>
        )}
        {onCopy !== undefined && (
          <button
            type="button"
            className={css.action}
            title={clipboardLabel}
            aria-label={clipboardLabel}
            onClick={onCopy}
          >
            {copied ? <IconCheckOutlineRegular size={14} /> : <IconCopyOutlineRegular size={14} />}
          </button>
        )}
      </div>
    </div>
  );
}
