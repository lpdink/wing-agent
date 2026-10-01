// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/markdown/CodeBlock.tsx
// Modified for Wing: the streaming highlight session (and its per-line React
// caching) is not ported — a still-growing fence renders as plain text and is
// highlighted once, when it settles (the strategy the VS Code renderer already
// uses); highlighting goes through this package's `highlightToHtml`; the copy
// control writes the browser clipboard through this package's `clipboard.ts`;
// imports point at this package's modules.

import { Fragment, useCallback, useMemo, useRef, useState } from 'react';
import type { CSSProperties, ReactNode, Ref } from 'react';
import clsx from 'clsx';

import { highlightToHtml } from '../chat/markdown/highlight';
import { writeClipboard } from '../tool/clipboard';
import { CodeToolbar, type CodeToolbarLabels } from './CodeToolbar';
import { useViewportHighlighting } from './useViewportHighlighting';
import css from './CodeBlock.module.css';

/** How long the copied confirmation stays on, in ms. */
const COPIED_FEEDBACK_MS = 1000;

export interface CodeBlockProps {
  /** The source text, rendered verbatim (trailing newline trimmed for display). */
  code: string;
  /** Grammar hint (markdown fence info string or a fixed caller id); unknown = plain. */
  lang?: string | undefined;
  /**
   * The code is still growing (a streaming markdown fence). A growing fence renders
   * as plain text — highlighting it on every chunk would re-tokenize the whole block
   * per chunk — and is highlighted once, when the caller flips this back to false.
   */
  streaming?: boolean | undefined;
  /** Extra class merged onto the wrapper (callers position; this component draws). */
  className?: string | undefined;
  /** Ref for the stable source-content wrapper, for owners that use it as a scrollport. */
  contentRef?: Ref<HTMLDivElement> | undefined;
  /** Show a numbered gutter without adding numbers to copied source. Defaults to false. */
  lineNumbers?: boolean | undefined;
  /** Show the language and copy header; false when the caller supplies a toolbar. Defaults to true. */
  showHeader?: boolean | undefined;
  /** Copy-button idle label; the owner passes the copy (this package carries no locale). */
  copyLabel: string;
  /** Copy-button label during the post-copy confirmation window. */
  copiedLabel: string;
  /** Enable the shared card toolbar and spacing; omit for custom toolbar layouts. */
  toolbarLabels?: CodeToolbarLabels | undefined;
  /** With toolbarLabels, use the owner's wrapping preference and omit the toolbar's local wrap action. */
  wrap?: boolean | undefined;
}

/**
 * Largest block we are willing to tokenize in one pass: shiki's JS engine measures
 * ~53 ms / 30 KB and ~447 ms / 300 KB in this bundle, and a single pass must not be
 * noticeable (the same bound the VS Code renderer applies).
 */
const HIGHLIGHT_MAX_CHARS = 32 * 1024;

function renderPlainLine(line: string, index: number): ReactNode {
  return (
    <Fragment key={index}>
      {index > 0 && '\n'}
      <span className="line">{line}</span>
    </Fragment>
  );
}

export function CodeBlock({
  code,
  lang,
  streaming,
  className,
  contentRef,
  lineNumbers = false,
  showHeader = true,
  copyLabel,
  copiedLabel,
  toolbarLabels,
  wrap,
}: CodeBlockProps) {
  const trimmed = code.endsWith('\n') ? code.slice(0, -1) : code;
  const sourceLines = lineNumbers ? trimmed.split('\n') : undefined;
  const rootRef = useRef<HTMLDivElement>(null);
  const highlighting = useViewportHighlighting(rootRef, lang);
  // A growing fence is never highlighted, and an oversized block is not worth the
  // one-off pass (the same rule the VS Code renderer documents). Unknown languages
  // and an unavailable engine fall back to plain text too — `highlightToHtml`
  // returns null and the plain branch below renders the identical line structure.
  const html = useMemo(
    () =>
      !highlighting || streaming === true || trimmed.length > HIGHLIGHT_MAX_CHARS
        ? null
        : highlightToHtml(trimmed, lang),
    [highlighting, streaming, trimmed, lang],
  );
  const [copied, setCopied] = useState(false);
  const [localWrapped, setLocalWrapped] = useState(true);
  const wrapped = wrap ?? localWrapped;

  const onCopy = useCallback(() => {
    if (copied) return;
    // The pre's text content, never the raw prop: it is the same text the user sees
    // (a settled fence's shiki tree carries no extra characters).
    const text = rootRef.current?.querySelector('pre')?.textContent ?? trimmed;
    void writeClipboard(text).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => {
        setCopied(false);
      }, COPIED_FEEDBACK_MS);
    });
  }, [copied, trimmed]);

  // shiki's HTML is generated from `code` (no model-authored markup passes
  // through), the sanctioned innerHTML consumption path per shiki's own docs.
  const body =
    html !== null ? (
      <div dangerouslySetInnerHTML={{ __html: html }} />
    ) : (
      <pre className={css.plain}>
        <code>{sourceLines === undefined ? trimmed : sourceLines.map(renderPlainLine)}</code>
      </pre>
    );

  return (
    <div
      ref={rootRef}
      className={clsx(
        css.block,
        'md-code-block',
        lineNumbers && css.numbered,
        toolbarLabels !== undefined && css.card,
        className,
      )}
      data-line-numbers={lineNumbers || undefined}
      data-code-wrap={toolbarLabels === undefined ? undefined : wrapped}
      style={
        sourceLines === undefined
          ? undefined
          : ({
              '--dsl-code-block-line-number-width': `${Math.max(2, String(sourceLines.length).length)}ch`,
            } as CSSProperties)
      }
    >
      {/* These paired attributes are stable semantic hooks for owner styling and DOM tests. */}
      {showHeader && (
        <div className={css.bannerWrap}>
          {toolbarLabels !== undefined ? (
            <CodeToolbar
              lang={lang}
              labels={toolbarLabels}
              copyLabel={copyLabel}
              copiedLabel={copiedLabel}
              copied={copied}
              wrapped={wrapped}
              onCopy={onCopy}
              onWrap={
                wrap === undefined
                  ? () => {
                      setLocalWrapped((value) => !value);
                    }
                  : undefined
              }
            />
          ) : (
            <div className={css.banner} data-code-block-banner>
              <div className={css.infostring}>{lang ?? ''}</div>
              <div className={css.action}>
                <button type="button" className={css.copyButton} onClick={onCopy}>
                  {copied ? copiedLabel : copyLabel}
                </button>
              </div>
            </div>
          )}
        </div>
      )}
      <div ref={contentRef} className={css.content} data-code-block-content>
        {body}
      </div>
    </div>
  );
}
