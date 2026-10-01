/**
 * Streaming Markdown rendering.
 *
 * `splitStreamingBlocks` cuts the text into finished blocks plus one active tail;
 * every block is a memoized `MarkdownBlock`, rendered from **one** keyed array —
 * including the live tail. That is what makes the two performance properties
 * structural rather than best-effort:
 *
 * - appending text re-renders (and re-parses) only the tail, because every other
 *   block's props are unchanged and `memo` skips it;
 * - a tail that becomes stable is the *same* element at the same key, so React
 *   updates it in place. Nothing is unmounted, no DOM is re-created and the
 *   fade-in animation does not restart (it only runs for genuinely new blocks).
 *
 * The streaming caret follows VS Code's inline progress marker: an empty inline
 * span whose `::after` cycles `'' → '.' → '..' → '...'` with
 * `steps(4, end) 1s infinite` (CHAT:102-116 keyframes, used at CHAT:134-140).
 */

import { memo, useMemo } from 'react';
import type { ReactElement, ReactNode } from 'react';

import styles from '../styles/markdown.module.css';
import { parseMarkdown } from './markdown/parse';
import { MarkdownNodes } from './markdown/render';
import { splitStreamingBlocks } from './markdown/split';

/**
 * One Markdown block.
 *
 * `memo` is the whole point: `source` is a string, `live` flips at most once
 * (tail → stable) and `trailing` is `null` for everything but the live tail, so
 * blocks that did not change never re-render (and never re-parse).
 *
 * `live` means "this block is still growing": it is passed down to the code
 * renderer, which does not highlight a code block while it is incomplete (see
 * `CodeBlock`).
 */
export const MarkdownBlock = memo(function MarkdownBlock({
  source,
  live = false,
  trailing = null,
}: {
  readonly source: string;
  readonly live?: boolean;
  readonly trailing?: ReactNode;
}): ReactElement {
  const nodes = useMemo(() => parseMarkdown(source), [source]);
  return <MarkdownNodes nodes={nodes} live={live} trailing={trailing} />;
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
  const blocks = useMemo(() => (tail === '' ? stable : [...stable, tail]), [stable, tail]);
  const lastIndex = blocks.length - 1;
  const liveCaret = streaming ? <StreamCaret /> : null;

  return (
    <div className={styles.markdown} data-streaming={streaming ? 'true' : 'false'}>
      {blocks.map((block, index) => {
        // Only the still-growing tail is live; a promoted block is finished.
        const live = streaming && index === lastIndex && tail !== '';
        return <MarkdownBlock key={index} source={block} live={live} trailing={live ? liveCaret : null} />;
      })}
      {/* The text ended on a boundary but more is coming: the caret gets its own
       * line rather than a paragraph (which would add a 16px margin). */}
      {streaming && tail === '' ? <span className={styles.streamCaret} data-testid="stream-caret" /> : null}
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
      <MarkdownNodes nodes={nodes} live={false} trailing={null} />
    </div>
  );
}

function StreamCaret(): ReactElement {
  return <span className={styles.streamCaret} data-testid="stream-caret" aria-hidden="true" />;
}
