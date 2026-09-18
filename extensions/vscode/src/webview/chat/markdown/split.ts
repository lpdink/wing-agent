/**
 * Streaming Markdown block splitting.
 *
 * The transcript re-renders the streaming cell on every `append_text` patch. If
 * that meant re-parsing the whole answer, a long reply would cost O(n²). Split
 * the text into *blocks* instead: everything before the last complete block
 * boundary is finished, and only the trailing block ("the active tail") can still
 * change. `MarkdownBlock` memoizes on the block source, so appending is
 * proportional to the tail, not to the answer.
 *
 * ## What counts as a block boundary
 *
 * A blank line **that has been terminated by a newline**, outside a code fence.
 * The "terminated" part is essential: while text is arriving, a trailing `\n`
 * only means "the next line starts here", and that next line can still grow the
 * *current* block. Treating the unterminated blank line as a boundary made the
 * splitter hand out a block that the very next chunk extended — the renderer then
 * had to take it back and re-create the DOM (and restart its fade-in) several
 * times per paragraph. With the rule below, `stable(T)` is always a prefix of
 * `stable(T + chunk)`: a promoted block is final.
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
  /**
   * Finished blocks, in order. Each is **final**: growing the text cannot change
   * it (it is always a prefix of the next result's `stable`).
   */
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
      } else if (trimmed === '' && newline !== -1) {
        // A *terminated* blank line between finished blocks (outside fences only).
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
