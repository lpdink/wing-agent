/**
 * Streaming Markdown block splitting.
 *
 * The transcript re-renders the streaming cell on every `append_text` patch. If
 * that meant re-parsing the whole answer, a long reply would cost O(n²). Split
 * the text into *blocks* instead: everything before the last blank line outside a
 * code fence is finished, and only the trailing block ("the active tail") can
 * still change. `MarkdownBlock` memoizes on the block source, so appending is
 * proportional to the tail, not to the answer.
 *
 * This mirrors VS Code's chat renderer, which decides whether a fenced block is
 * complete by looking for its closing fence (`chatMarkdownContentPart.ts`, the
 * `codeblockHasClosingBackticks(raw)` check) instead of trusting the parser with
 * a half-finished document.
 *
 * Known limitation: a blank line inside a loose list splits it into two `<ul>`s.
 * LLM output almost never writes lists that way, and the alternative (re-parse
 * everything on every chunk) is exactly what this module exists to avoid.
 */

/** Open code fence state while scanning. */
interface FenceState {
  readonly char: '`' | '~';
  readonly size: number;
}

/** A line made of at least three backticks or tildes, optionally with an info string. */
const FENCE_OPEN = /^(`{3,}|~{3,})/;

export interface StreamBlocks {
  /** Finished blocks, in order. Each is stable: it never changes again. */
  readonly stable: readonly string[];
  /** The trailing block still being streamed; `''` when the text ends on a boundary. */
  readonly tail: string;
}

const EMPTY: StreamBlocks = { stable: [], tail: '' };

/**
 * Split streaming Markdown into finished blocks plus the active tail.
 *
 * Pure and allocation-light: one pass over the lines, no parsing.
 */
export function splitStreamingBlocks(text: string): StreamBlocks {
  if (text === '') {
    return EMPTY;
  }

  const stable: string[] = [];
  let blockStart = 0;
  let fence: FenceState | null = null;

  let lineStart = 0;
  for (;;) {
    const newline = text.indexOf('\n', lineStart);
    const lineEnd = newline === -1 ? text.length : newline;
    const line = text.slice(lineStart, lineEnd);
    const trimmed = line.trim();

    if (fence === null) {
      const opener = FENCE_OPEN.exec(trimmed);
      if (opener !== null) {
        const marker = opener[1] ?? '';
        fence = { char: marker.charAt(0) === '`' ? '`' : '~', size: marker.length };
      } else if (trimmed === '') {
        // Boundary between finished blocks (outside fences only).
        const block = text.slice(blockStart, lineStart);
        if (block.trim() !== '') {
          stable.push(block);
        }
        blockStart = lineEnd + 1;
      }
    } else if (isClosingFence(trimmed, fence)) {
      fence = null;
    }

    if (newline === -1) {
      break;
    }
    lineStart = newline + 1;
  }

  const tail = text.slice(blockStart);
  return { stable, tail: tail.trim() === '' ? '' : tail };
}

/**
 * True when `line` closes `fence`: same marker character, at least as long, and
 * nothing else on the line (CommonMark allows trailing whitespace only).
 */
function isClosingFence(line: string, fence: FenceState): boolean {
  if (line === '' || line.charAt(0) !== fence.char) {
    return false;
  }
  let size = 0;
  while (size < line.length && line.charAt(size) === fence.char) {
    size += 1;
  }
  return size >= fence.size && line.slice(size).trim() === '';
}
