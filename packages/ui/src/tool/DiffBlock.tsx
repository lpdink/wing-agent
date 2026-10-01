// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/DiffBlock.tsx (+ DiffBlock.module.css)
// Modified for Wing: the data source is Wing's `DiffCellModel` — the host already
// computed and windowed the rows, so the `diff` package (structuredPatch), the
// `DiffHunk` contract and `diffTotals` are gone; rows map to the card's line classes
// one-to-one, a windowed (`truncated`) payload appends the `…` row, `+n/−m`
// counters ride the toolbar's status slot, and the language hint comes from a
// local extension table instead of the Harness workspace helper (that table is
// exported for the test pinning it to the grammars the highlighter ships). Wing
// also added two DOM hooks the tests read: `data-testid="diff-body"` on the body
// and `data-diff-kind` on every row.

import { useCallback, useMemo, useState } from 'react';
import clsx from 'clsx';

import type { DiffCellModel } from '@wing-agent/session';
import { FoldToggle } from './FoldToggle';
import { headTailCap } from './head-tail-cap';
import { writeClipboard } from './clipboard';
import { CodeToolbar, type CodeToolbarLabels } from '../markdown/CodeToolbar';
import cardCss from '../markdown/CodeCard.module.css';
import css from './DiffBlock.module.css';

/** Output lines shown before the height cap collapses the middle. */
export const DEFAULT_DIFF_MAX_LINES = 16;

/** Localized chrome supplied by the owning render site. */
export interface DiffBlockLabels extends CodeToolbarLabels {
  copy: string;
  copied: string;
  collapseAria: string;
  expandAria: (hidden: number) => string;
  collapse: string;
  expand: (hidden: number) => string;
}

export interface DiffBlockProps {
  /** The host-derived diff: path, windowed rows and the `+n/−m` counters. */
  cell: DiffCellModel;
  /** Localized chrome supplied by the owning render site. */
  labels: DiffBlockLabels;
  /** Height cap in body lines before the middle collapses (default {@link DEFAULT_DIFF_MAX_LINES}). */
  maxLines?: number | undefined;
  /** Extra class merged onto the wrapper (callers position; this component draws). */
  className?: string | undefined;
}

/** A single rendered body line and its role, so the height cap slices a flat list. */
interface DiffRow {
  kind: 'hunk' | 'del' | 'add' | 'context' | 'gap';
  text: string;
}

/** The dim class per row kind (path/gap chrome vs the diff's own +/- colours). */
const ROW_CLASS: Record<DiffRow['kind'], string | undefined> = {
  hunk: css.hunk,
  del: css.del,
  add: css.add,
  context: css.context,
  gap: css.gap,
};

/**
 * Flatten the cell's windowed rows.
 *
 * A windowed payload (`truncated`) ends in one `…` row: the host cut the payload,
 * and a reader must not mistake the window for the whole change. The row kinds map
 * one-to-one onto the card's line classes (a `hunk` header renders dim, never
 * coloured as a change).
 * @param cell - the host-derived diff.
 * @returns the body rows.
 */
function buildRows(cell: DiffCellModel): DiffRow[] {
  const rows: DiffRow[] = cell.lines.map((line) => ({ kind: line.kind, text: line.text }));
  if (cell.truncated) {
    rows.push({ kind: 'gap', text: '…' });
  }
  return rows;
}

/**
 * Copy the full windowed diff, including folded rows: removed/added lines have
 * `- `/`+ ` prefixes, context two spaces, and the path header leads the text.
 * @param path - the changed file's path.
 * @param rows - the flattened body rows.
 * @returns the diff as plain text.
 */
function copyText(path: string, rows: readonly DiffRow[]): string {
  return [
    path,
    ...rows.map((row) => {
      switch (row.kind) {
        case 'del':
          return `- ${row.text}`;
        case 'add':
          return `+ ${row.text}`;
        case 'context':
          return `  ${row.text}`;
        default:
          // hunk headers and the truncation marker stay verbatim.
          return row.text;
      }
    }),
  ].join('\n');
}

/**
 * Language hint for a path's extension, or undefined.
 *
 * The source component reads a full table from the Harness workspace package,
 * which is not a dependency here; this is the short extension map the code cards
 * need. Every value must be a grammar the shared highlighter actually registers
 * (`src/chat/markdown/highlight.ts`) — a hint the highlighter cannot resolve would
 * silently fall back to the generic label, which is why the table exists at all.
 * `tests/tool/diff-block.test.tsx` pins that invariant against
 * `supportsHighlighting`; an extension the table does not list (or lists to an
 * unsupported language, which the test rejects) simply yields "no label".
 * @param path - the changed file's path.
 * @returns the language hint, or undefined.
 */
export function languageForPath(path: string): string | undefined {
  const extension = path.slice(path.lastIndexOf('.') + 1).toLowerCase();
  return LANGUAGE_BY_EXTENSION[extension];
}

/**
 * Extensions mapped to grammars the shared highlighter ships.
 *
 * Exported for the invariant test only (it is not part of the card's props
 * contract): `.jsx` and `.md` are deliberately absent — the bundled grammars
 * register no `jsx` alias (the TSX grammar handles `.tsx` only) and no markdown
 * grammar is loaded, so listing them would be a silent failure.
 */
export const LANGUAGE_BY_EXTENSION: Record<string, string> = {
  bash: 'bash',
  css: 'css',
  diff: 'diff',
  html: 'html',
  htm: 'html',
  javascript: 'js',
  js: 'js',
  json: 'json',
  py: 'python',
  rs: 'rust',
  sh: 'bash',
  sql: 'sql',
  toml: 'toml',
  ts: 'ts',
  tsx: 'tsx',
  yaml: 'yaml',
  yml: 'yaml',
  zsh: 'bash',
};

/**
 * Render a file mutation as an inline diff surface.
 * @param props - see {@link DiffBlockProps}.
 * @returns the diff block element.
 */
export function DiffBlock({ cell, labels, maxLines = DEFAULT_DIFF_MAX_LINES, className }: DiffBlockProps) {
  const rows = useMemo(() => buildRows(cell), [cell]);
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const [wrapped, setWrapped] = useState(false);

  const onCopy = useCallback(() => {
    if (copied) return;
    void writeClipboard(copyText(cell.path, rows)).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => {
        setCopied(false);
      }, 1000);
    });
  }, [copied, cell.path, rows]);

  const onToggle = useCallback(() => {
    setExpanded((value) => !value);
  }, []);

  if (rows.length === 0) return null;

  // Same split arithmetic as TerminalBlock and the VS Code transcript's collapsed
  // card, so a body's head and tail slices agree across the front ends.
  const { hidden, capped, headLines, tailLines } = headTailCap(rows.length, maxLines, expanded);
  const head = capped ? rows.slice(0, headLines) : rows;
  const tail = capped ? rows.slice(rows.length - tailLines) : [];

  return (
    <div className={clsx(cardCss.card, css.block, className)} data-diff="" data-code-wrap={wrapped}>
      <CodeToolbar
        lang={languageForPath(cell.path)}
        title={cell.path}
        status={
          <>
            <span className={css.added}>{`+${cell.added}`}</span>
            <span className={css.removed}>{`−${cell.removed}`}</span>
          </>
        }
        labels={labels}
        copyLabel={labels.copy}
        copiedLabel={labels.copied}
        copied={copied}
        wrapped={wrapped}
        onCopy={onCopy}
        onWrap={() => {
          setWrapped((value) => !value);
        }}
      />
      <div className={css.body} data-testid="diff-body">
        {head.map((row, index) => (
          // Diff rows have no stable identity; the window is replaced wholesale.
          <div key={index} className={clsx(css.line, ROW_CLASS[row.kind])} data-diff-kind={row.kind}>
            {row.text}
          </div>
        ))}
        {hidden > 0 && (
          <FoldToggle
            className={css.expand}
            expanded={expanded}
            hidden={hidden}
            labels={labels}
            onToggle={onToggle}
          />
        )}
        {tail.map((row, index) => (
          <div key={index} className={clsx(css.line, ROW_CLASS[row.kind])} data-diff-kind={row.kind}>
            {row.text}
          </div>
        ))}
      </div>
    </div>
  );
}
