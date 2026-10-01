/**
 * Markdown for the web transcript — the shared node renderer, with the fences swapped.
 *
 * The renderer's markdown pipeline belongs to `@wing-agent/ui` and this app keeps
 * using it: `splitStreamingBlocks` + `parseMarkdown` + `MarkdownNodes` come straight
 * from the package, so paragraphs, headings, quotes, lists, tables, links, images and
 * KaTeX render through exactly the code the VS Code webview runs. What is *not*
 * reusable is one branch inside `MarkdownNodes`: its `code` case renders the old
 * tool-strip code block, and step 08b's whole point is that the transcript uses the
 * ported `CodeBlock` card (wrap toggle, line gutter, viewport highlighting) instead.
 *
 * So this module does the smallest thing that can express that: it walks the parsed
 * **top-level nodes**, renders runs of non-code nodes through `MarkdownNodes`
 * unchanged, and hands each code node to the shared card. The package's own
 * container class (`.markdown`) is transcribed in `cells.css`, because the wrapper
 * this module needs is not reachable across the boundary.
 *
 * Known boundary: a fence nested inside a quote or a list item stays the old card —
 * it is a child node of that block, and the block is rendered by `MarkdownNodes`.
 * Wing's transcripts put fences at the top level; the boundary is pinned by a test
 * rather than hidden.
 *
 * The streaming rules are the package's: one memoized block per `splitStreamingBlocks`
 * chunk (so appending a token re-parses only the tail), and the caret follows the
 * last paragraph inline, or the block when the text ended on a boundary.
 */

import { Fragment, memo, useMemo, type ReactElement, type ReactNode } from 'react';

import {
  CodeBlock,
  MarkdownNodes,
  parseMarkdown,
  splitStreamingBlocks,
  type MarkdownNode,
} from '@wing-agent/ui';

import { CODE_COPY_LABELS, CODE_TOOLBAR_LABELS } from './labels';

/**
 * A Markdown region that may still be growing.
 *
 * @param props.text - the complete or streaming markdown source.
 * @param props.streaming - true while more text is coming (the caret).
 * @returns the rendered region.
 */
export function WebMarkdownStream({
  text,
  streaming,
}: {
  readonly text: string;
  readonly streaming: boolean;
}): ReactElement {
  const { stable, tail } = useMemo(() => splitStreamingBlocks(text), [text]);
  const blocks = useMemo(() => (tail === '' ? stable : [...stable, tail]), [stable, tail]);
  const lastIndex = blocks.length - 1;
  const liveCaret = streaming ? <StreamCaret /> : null;

  return (
    <div className="cell__markdown" data-streaming={streaming ? 'true' : 'false'}>
      {blocks.map((block, index) => {
        // Only the still-growing tail is live; a promoted block is finished.
        const live = streaming && index === lastIndex && tail !== '';
        return <MarkdownBlock key={index} source={block} live={live} trailing={live ? liveCaret : null} />;
      })}
      {/* The text ended on a boundary but more is coming: the caret gets its own line
       * rather than a paragraph (which would add a paragraph margin). */}
      {streaming && tail === '' ? <StreamCaret /> : null}
    </div>
  );
}

/**
 * One markdown block.
 *
 * `memo` is the whole point (`MarkdownStream`'s rule in the package): `source` is a
 * string and `live` flips at most once, so a block that did not change neither
 * re-renders nor re-parses.
 */
const MarkdownBlock = memo(function MarkdownBlock({
  source,
  live,
  trailing,
}: {
  readonly source: string;
  readonly live: boolean;
  readonly trailing: ReactNode;
}): ReactElement {
  const nodes = useMemo(() => parseMarkdown(source), [source]);
  return <WebMarkdownNodes nodes={nodes} live={live} trailing={trailing} />;
});

/**
 * Block-level nodes, with the top-level fences drawn by the shared card.
 *
 * Every non-code node goes through the package's `MarkdownNodes` — including the
 * `trailing` caret contract, which that component already implements (inline in the
 * last paragraph, after the last block otherwise).
 */
function WebMarkdownNodes({
  nodes,
  live,
  trailing,
}: {
  readonly nodes: readonly MarkdownNode[];
  readonly live: boolean;
  readonly trailing: ReactNode;
}): ReactElement {
  const lastIndex = nodes.length - 1;
  const parts: ReactElement[] = [];
  let run: MarkdownNode[] = [];

  const flush = (runTrailing: ReactNode): void => {
    if (run.length === 0) {
      return;
    }
    const nodesInRun = run;
    run = [];
    parts.push(<MarkdownNodes key={parts.length} nodes={nodesInRun} live={live} trailing={runTrailing} />);
  };

  nodes.forEach((node, index) => {
    const isLast = index === lastIndex;
    if (node.kind !== 'code') {
      run.push(node);
      if (isLast) {
        // The package's rule: an inline caret when the text ends in a paragraph,
        // otherwise a block-level one after it.
        flush(trailing);
      }
      return;
    }
    flush(null);
    parts.push(
      <Fragment key={parts.length}>
        <CodeBlock
          code={node.code}
          lang={node.lang}
          // A fence that is still being written is never highlighted (the card's
          // documented rule, and the renderer's — re-tokenizing per chunk costs
          // seconds); it is highlighted once, when `closed` flips.
          streaming={live && !node.closed}
          {...CODE_COPY_LABELS}
          toolbarLabels={CODE_TOOLBAR_LABELS}
        />
        {isLast ? trailing : null}
      </Fragment>,
    );
  });
  flush(null);

  return <>{parts}</>;
}

/** The inline streaming marker (`data-testid` kept from the renderer's own caret). */
function StreamCaret(): ReactElement {
  return <span className="cell__caret" data-testid="stream-caret" aria-hidden="true" />;
}
