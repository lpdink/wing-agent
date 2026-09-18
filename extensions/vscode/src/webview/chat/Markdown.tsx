/**
 * Streaming Markdown rendering.
 *
 * `splitStreamingBlocks` cuts the text into finished blocks plus one active tail;
 * every finished block is a memoized `MarkdownBlock`, so an `append_text` patch
 * re-renders (and re-parses) only the tail. That is the "stable prefix" rule from
 * the step contract, made structural rather than best-effort.
 *
 * The streaming caret follows VS Code's inline progress marker: an empty inline
 * span whose `::after` cycles `'' → '.' → '..' → '...'` with
 * `steps(4, end) 1s infinite` (CHAT:493-537, :134-140).
 */

import { memo, useMemo } from 'react';
import type { ReactElement } from 'react';

import styles from '../styles/markdown.module.css';
import { parseMarkdown } from './markdown/parse';
import { MarkdownNodes } from './markdown/render';
import { splitStreamingBlocks } from './markdown/split';

/**
 * One finished Markdown block.
 *
 * `memo` is the whole point: `source` is a string and `trailing` is stable, so
 * blocks that did not change never re-render (and never re-parse).
 */
export const MarkdownBlock = memo(function MarkdownBlock({
  source,
  trailing = null,
}: {
  readonly source: string;
  readonly trailing?: ReactElement | null;
}): ReactElement {
  const nodes = useMemo(() => parseMarkdown(source), [source]);
  return <MarkdownNodes nodes={nodes} trailing={trailing} />;
});

/**
 * A Markdown region that may still be growing.
 *
 * While `streaming`, the last block renders an animated ellipsis so the user sees
 * where the text is arriving; the caret is inline when the tail ends in a
 * paragraph, otherwise it follows the block.
 */
export function MarkdownStream({
  text,
  streaming,
}: {
  readonly text: string;
  readonly streaming: boolean;
}): ReactElement {
  const { stable, tail } = useMemo(() => splitStreamingBlocks(text), [text]);
  const nodes = useMemo(() => (tail === '' ? [] : parseMarkdown(tail)), [tail]);
  const caret = streaming ? <StreamCaret /> : null;
  const tailEndsInParagraph = nodes.length > 0 && nodes[nodes.length - 1]?.kind === 'paragraph';

  return (
    <div className={styles.markdown} data-streaming={streaming ? 'true' : 'false'}>
      {stable.map((block, index) => (
        <MarkdownBlock key={index} source={block} />
      ))}
      {nodes.length === 0 ? null : <MarkdownNodes nodes={nodes} trailing={caret} />}
      {streaming && (nodes.length === 0 || !tailEndsInParagraph) ? (
        <p className={styles.paragraph}>{caret}</p>
      ) : null}
    </div>
  );
}

/**
 * Static Markdown (no streaming): one block for the whole text.
 *
 * Used for content that can no longer change (a finished assistant message is
 * still streamed once, so the transcript uses {@link MarkdownStream} whenever the
 * cell reports `streaming`, and this for everything else).
 */
export function MarkdownText({ text }: { readonly text: string }): ReactElement {
  const nodes = useMemo(() => parseMarkdown(text), [text]);
  return (
    <div className={styles.markdown}>
      <MarkdownNodes nodes={nodes} />
    </div>
  );
}

function StreamCaret(): ReactElement {
  return <span className={styles.streamCaret} data-testid="stream-caret" aria-hidden="true" />;
}
