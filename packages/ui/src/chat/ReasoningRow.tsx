// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-chat/src/client/chat/ReasoningRow.tsx
// Modified for Wing: the expanded body renders this package's `MarkdownStream` (the
// upstream `MarkdownText` belongs to a mdast pipeline this package does not ship);
// the slot contract (`useDisclosure` / `usePresentation` / `t()`) is replaced by a
// local disclosure state — optionally backed by the package's remembered collapse
// overrides through `collapseKey` — a `previewEnabled` prop and literal labels.

/** Assistant reasoning disclosure, independent of tool-call presentation. */
import { memo, useCallback, useMemo, useState } from 'react';

import { useCollapsible, type CollapsibleState } from './interaction';
import { DisclosureRow } from './DisclosureRow';
import { TextShimmer } from './TextShimmer';
import { MarkdownStream } from './Markdown';
import { IconThinkOutlineRegular } from '../icons';
import a11yCss from './accessibility.module.css';
import css from './ReasoningRow.module.css';

const THINK_ICON = <IconThinkOutlineRegular size={14} />;

/**
 * Copy of a reasoning row. Defaults read as the renderer's English UI (Wing does not
 * localize at runtime); a shell with its own phrasing passes its own labels.
 */
export interface ReasoningRowLabels {
  /** Title while the reasoning text is still streaming. */
  readonly running: string;
  /** Title once the block settled; receives the known duration (or `null`). */
  readonly settled: (durationMs: number | null) => string;
  /** Visually hidden announcement while the block is running. */
  readonly announcement: string;
}

/** The renderer's default reasoning copy (same wording as the thinking cell). */
export const DEFAULT_REASONING_LABELS: ReasoningRowLabels = {
  running: 'Thinking',
  settled: (durationMs) =>
    durationMs === null ? 'Thought' : `Thought for ${(durationMs / 1000).toFixed(1)}s`,
  announcement: 'Running',
};

/** The first line of `text`, or `''` for an empty string. */
export function firstLine(text: string): string {
  const newline = text.indexOf('\n');
  return newline === -1 ? text : text.slice(0, newline);
}

/**
 * First line of the latest paragraph whose first line is complete.
 *
 * A streaming block's last paragraph may be mid-word — the caller only shows a
 * finished one, so the preview never flickers with half a sentence. Paragraphs are
 * separated by one or more blank lines (leading whitespace allowed).
 * @param text - complete or streaming reasoning text.
 * @returns The latest completed paragraph's first line, or `''` when none is complete.
 */
export function latestCompletedParagraphFirstLine(text: string): string {
  let summary = '';
  let paragraphStart = 0;
  const separator = /\r?\n(?:[\t ]*\r?\n)+/g;
  while (true) {
    const nextParagraph = separator.exec(text);
    const paragraphEnd =
      nextParagraph === null ? text.length : nextParagraph.index + nextParagraph[0].indexOf('\n');
    const newline = text.indexOf('\n', paragraphStart);
    if (newline !== -1 && newline <= paragraphEnd) {
      const candidate = text.slice(paragraphStart, newline).trim();
      if (candidate !== '') summary = candidate;
    }
    if (nextParagraph === null) return summary;
    paragraphStart = nextParagraph.index + nextParagraph[0].length;
  }
}

/**
 * Disclosure state of one reasoning row.
 *
 * With a `collapseKey` the row reuses the renderer's remembered expand/collapse
 * overrides (the same store `ThinkingCell` writes: a chosen state survives tab
 * switches). Without one — a standalone row, a preview harness — the state is local,
 * like the upstream `useDisclosure`.
 */
function useReasoningDisclosure(collapseKey: string | undefined, autoCollapsed: boolean): CollapsibleState {
  // Both hooks run unconditionally; the remembered one is simply not read without a key
  // (nothing is written to the override store unless its toggle is the active one).
  const remembered = useCollapsible(collapseKey ?? '', autoCollapsed);
  const [local, setLocal] = useState<boolean | null>(null);
  const toggleLocal = useCallback(() => {
    setLocal((previous) => !(previous ?? autoCollapsed));
  }, [autoCollapsed]);
  return collapseKey === undefined ? { collapsed: local ?? autoCollapsed, toggle: toggleLocal } : remembered;
}

export interface ReasoningRowProps {
  /** Complete or streaming reasoning text. */
  readonly text: string;
  /** True while this row is the streaming tail; the row starts open and shimmers. */
  readonly streaming: boolean;
  /** Wall-clock duration of the block, when the host knows it (settled title). */
  readonly durationMs?: number | null;
  /**
   * Remember the user's expand choice across mounts, keyed by the owning cell
   * (`useCollapsible`'s store). Omitted: local state, reset on unmount.
   */
  readonly collapseKey?: string | undefined;
  /** Show the settled one-line preview beside the title (upstream `settledReasoningPreview`). */
  readonly previewEnabled?: boolean | undefined;
  readonly labels?: ReasoningRowLabels | undefined;
}

/**
 * Render one assistant reasoning block, collapsed except while it streams.
 *
 * The collapsed summary omits double-asterisk markers; the expanded content renders
 * the complete Markdown through {@link MarkdownStream}. While streaming the row is
 * open by default (that is the only live feedback a thinking block has); once the
 * stream ends it collapses to one line unless the reader had opened it.
 * @param props - reasoning text, run state, duration and disclosure a11y settings.
 * @returns the reasoning disclosure.
 */
export const ReasoningRow = memo(function ReasoningRow({
  text,
  streaming,
  durationMs = null,
  collapseKey,
  previewEnabled = true,
  labels = DEFAULT_REASONING_LABELS,
}: ReasoningRowProps) {
  const { collapsed, toggle } = useReasoningDisclosure(collapseKey, !streaming);
  const open = !collapsed;
  const summaryText = streaming ? latestCompletedParagraphFirstLine(text) : firstLine(text);
  const summary = useMemo(() => summaryText.replaceAll('**', ''), [summaryText]);
  const preview = !open && summary !== '' && (streaming || previewEnabled);
  const title = streaming ? labels.running : labels.settled(durationMs);
  const collapsedContent = useMemo(
    () => (
      <>
        <span className={css.separator} data-shimmer-decoration aria-hidden />
        <span className={css.summary} data-streaming={streaming || undefined}>
          <span className={css.summaryText}>
            {/* Nested so the shimmer's decorative copy carries the text as a data
                attribute instead of a second text node (upstream markup). */}
            <TextShimmer>{summary}</TextShimmer>
          </span>
        </span>
      </>
    ),
    [streaming, summary],
  );

  return (
    <div
      className={css.root}
      data-variant="think"
      data-state={streaming ? 'running' : 'ok'}
      data-expanded={open || undefined}
      data-preview={preview || undefined}
    >
      {streaming && <span className={a11yCss.visuallyHidden}>{labels.announcement}</span>}
      <DisclosureRow
        rowClassName={css.row}
        leadingClassName={css.leading}
        titleClassName={css.title}
        icon={THINK_ICON}
        title={title}
        running={streaming}
        open={open}
        expandable
        expandOnRowClick
        onToggle={toggle}
        collapsedContent={collapsedContent}
      >
        <div className={css.thinkBody}>
          <MarkdownStream text={text} streaming={streaming} />
        </div>
      </DisclosureRow>
    </div>
  );
});
