/**
 * Markdown parsing for the transcript.
 *
 * `markdown-it` parses to a token stream; we convert it to a small AST that the
 * renderer walks. Two reasons not to render the token stream directly:
 *
 * 1. the AST is a value (not a mutable token stream), so `MarkdownBlock` can be
 *    memoized on `source` and the parse result can be asserted in tests;
 * 2. `html: false` keeps model-produced HTML out of the DOM entirely — the
 *    renderer only ever creates elements we chose.
 *
 * Options mirror VS Code's chat markdown renderer
 * (`workbench/contrib/chat/browser/widget/chatContentParts/chatMarkdownContentPart.ts`,
 * `{ gfm: true, breaks: true }` — single newlines break, GFM tables/strikethrough),
 * with `linkify: true` so bare URLs become links.
 */

import MarkdownIt from 'markdown-it';
import type Token from 'markdown-it/lib/token.mjs';
import type StateBlock from 'markdown-it/lib/rules_block/state_block.mjs';
import type StateInline from 'markdown-it/lib/rules_inline/state_inline.mjs';

// ── AST ───────────────────────────────────────────────────────────────

export type MarkdownInline =
  | { readonly kind: 'text'; readonly text: string }
  | { readonly kind: 'code'; readonly text: string }
  | { readonly kind: 'strong'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'em'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'del'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'link'; readonly href: string; readonly children: readonly MarkdownInline[] }
  /**
   * A markdown image. A local file inside the workspace is rendered once the host
   * has resolved it to a webview URI (`chat/markdown/image.ts`); everything else —
   * remote URLs, paths outside the workspace, non-images — stays a link, which is
   * what the transcript always showed (see `MarkdownImage`).
   */
  | { readonly kind: 'image'; readonly src: string; readonly alt: string }
  | MathNode
  | { readonly kind: 'break'; readonly hard: boolean };

/** A formula; see {@link MathNode}. */
export type MarkdownMath = Extract<MarkdownInline, { kind: 'math' }>;

/**
 * One formula, in either union (`MarkdownInline` *and* `MarkdownNode` share this
 * exact shape — a display formula on its own is a block, the same formula inside a
 * sentence is inline, and both render identically).
 *
 * `tex` is what KaTeX gets; `source` is the original text *including* the
 * delimiters, which is what a failed render falls back to (a formula must never
 * disappear silently). `display` only selects KaTeX's display layout.
 */
export interface MathNode {
  readonly kind: 'math';
  readonly tex: string;
  readonly source: string;
  readonly display: boolean;
}

export type MarkdownNode =
  | { readonly kind: 'paragraph'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'heading'; readonly level: number; readonly children: readonly MarkdownInline[] }
  | MathNode
  | {
      readonly kind: 'code';
      readonly lang: string;
      readonly code: string;
      /**
       * True when the source carried the closing fence. An *open* fence is still
       * being streamed, so its content can grow — the code renderer uses this to
       * defer highlighting until the code is final (see `CodeBlock`).
       */
      readonly closed: boolean;
    }
  | { readonly kind: 'quote'; readonly children: readonly MarkdownNode[] }
  | {
      readonly kind: 'list';
      readonly ordered: boolean;
      readonly start: number;
      readonly items: readonly (readonly MarkdownNode[])[];
    }
  | {
      readonly kind: 'table';
      readonly aligns: readonly (string | null)[];
      readonly head: readonly TableRow[];
      readonly rows: readonly TableRow[];
    }
  | { readonly kind: 'rule' };

/** One table row: its cells, each a run of inline nodes. */
export type TableRow = readonly (readonly MarkdownInline[])[];

// ── parser ────────────────────────────────────────────────────────────

const markdown = new MarkdownIt({ html: false, linkify: true, breaks: true });

// Formulas are not part of markdown-it, so the two rules below are ours (see the
// "math" section at the bottom of this file). Registration order is load-bearing:
//
// - inline: *before* `escape`, because markdown-it would otherwise turn `\(x\)`
//   into an escaped paren (`(x)`) — the exact bug the TUI lane normalizes away;
// - block: the `alt` chain is what lets `$$` interrupt a paragraph (i.e. start
//   display math right after a line of text, without a blank line).
markdown.inline.ruler.before('escape', 'math_inline', mathInline);
markdown.block.ruler.before('fence', 'math_block', mathBlock, {
  alt: ['paragraph', 'reference', 'blockquote', 'list'],
});

/** Parse one Markdown block into render-ready nodes. Pure: same source → same AST. */
export function parseMarkdown(source: string): readonly MarkdownNode[] {
  return parseBlocks(markdown.parse(source, {}), { index: 0, lines: source.split('\n') }, null);
}

/** Mutable position in a token stream. */
interface Cursor {
  index: number;
}

/** Block-level cursor: it also carries the source lines for fence completeness checks. */
interface BlockCursor extends Cursor {
  readonly lines: readonly string[];
}

/**
 * Parse block tokens until `stop` (a closing token type; `null` = end of input).
 * The stop token itself is consumed.
 */
function parseBlocks(tokens: readonly Token[], cursor: BlockCursor, stop: string | null): MarkdownNode[] {
  const nodes: MarkdownNode[] = [];

  while (cursor.index < tokens.length) {
    const token = tokens[cursor.index];
    if (token === undefined) {
      break;
    }
    if (stop !== null && token.type === stop) {
      cursor.index += 1;
      break;
    }

    switch (token.type) {
      case 'paragraph_open':
        nodes.push({ kind: 'paragraph', children: parseInlineAfter(tokens, cursor) });
        break;
      case 'heading_open':
        nodes.push({
          kind: 'heading',
          level: Number.parseInt(token.tag.slice(1), 10),
          children: parseInlineAfter(tokens, cursor),
        });
        break;
      case 'fence':
        cursor.index += 1;
        nodes.push({
          kind: 'code',
          lang: languageOf(token.info),
          // A fence token is emitted for an unterminated fence too (the content
          // then runs to the end of the input) — look for the closing line.
          closed: hasClosingFence(cursor.lines, token.map),
          code: token.content,
        });
        break;
      case 'code_block':
        cursor.index += 1;
        // Indented code blocks have no fence to close.
        nodes.push({ kind: 'code', lang: '', code: token.content, closed: true });
        break;
      case 'math_block':
        cursor.index += 1;
        nodes.push({ kind: 'math', tex: token.content, source: token.info, display: true });
        break;
      case 'hr':
        cursor.index += 1;
        nodes.push({ kind: 'rule' });
        break;
      case 'blockquote_open':
        cursor.index += 1;
        nodes.push({ kind: 'quote', children: parseBlocks(tokens, cursor, 'blockquote_close') });
        break;
      case 'bullet_list_open':
      case 'ordered_list_open':
        nodes.push(parseList(tokens, cursor, token));
        break;
      case 'table_open':
        nodes.push(parseTable(tokens, cursor));
        break;
      case 'inline':
        // Only reachable for a stray inline token (markdown-it emits them inside
        // block constructs, which are handled above).
        cursor.index += 1;
        nodes.push({ kind: 'paragraph', children: parseInline(token.children ?? []) });
        break;
      default:
        cursor.index += 1;
        break;
    }
  }

  return nodes;
}

/** `paragraph_open inline paragraph_close` / `heading_open inline heading_close`. */
function parseInlineAfter(tokens: readonly Token[], cursor: BlockCursor): readonly MarkdownInline[] {
  const inline = tokens[cursor.index + 1];
  const children = inline !== undefined && inline.type === 'inline' ? (inline.children ?? []) : [];
  cursor.index += inline === undefined ? 1 : 3;
  return parseInline(children);
}

function parseList(tokens: readonly Token[], cursor: BlockCursor, open: Token): MarkdownNode {
  const ordered = open.type === 'ordered_list_open';
  const close = ordered ? 'ordered_list_close' : 'bullet_list_close';
  const start = Number.parseInt(open.attrGet('start') ?? '1', 10);
  const items: MarkdownNode[][] = [];

  cursor.index += 1;
  while (cursor.index < tokens.length) {
    const token = tokens[cursor.index];
    if (token === undefined) {
      break;
    }
    if (token.type === 'list_item_open') {
      cursor.index += 1;
      items.push(parseBlocks(tokens, cursor, 'list_item_close'));
      continue;
    }
    if (token.type === close) {
      cursor.index += 1;
      break;
    }
    cursor.index += 1;
  }

  return { kind: 'list', ordered, start: Number.isNaN(start) ? 1 : start, items };
}

function parseTable(tokens: readonly Token[], cursor: BlockCursor): MarkdownNode {
  const aligns: (string | null)[] = [];
  const head: MarkdownInline[][][] = [];
  const rows: MarkdownInline[][][] = [];
  let cells: MarkdownInline[][] | null = null;
  let inHead = true;
  let cell: MarkdownInline[] = [];

  cursor.index += 1;
  while (cursor.index < tokens.length) {
    const token = tokens[cursor.index];
    if (token === undefined) {
      break;
    }
    cursor.index += 1;
    switch (token.type) {
      case 'table_close':
        return { kind: 'table', aligns, head, rows };
      case 'thead_open':
        inHead = true;
        break;
      case 'tbody_open':
        inHead = false;
        break;
      case 'tr_open':
        cells = [];
        break;
      case 'tr_close':
        if (cells !== null) {
          if (inHead) {
            head.push(cells);
          } else {
            rows.push(cells);
          }
          cells = null;
        }
        break;
      case 'th_open':
        aligns.push(alignmentOf(token));
        cell = [];
        break;
      case 'td_open':
        cell = [];
        break;
      case 'th_close':
      case 'td_close':
        cells?.push(cell);
        break;
      case 'inline':
        cell = [...parseInline(token.children ?? [])];
        break;
      default:
        break;
    }
  }

  return { kind: 'table', aligns, head, rows };
}

/** A line made of at least three backticks or tildes, optionally with an info string. */
const FENCE_MARKER = /^(`{3,}|~{3,})/;

/**
 * True when the fence that starts at `map[0]` is closed later in the source.
 *
 * `map` is markdown-it's `[startLine, endLine]` (end exclusive) and is `null` for
 * tokens without a source position, which only happens for injected tokens.
 */
function hasClosingFence(lines: readonly string[], map: readonly [number, number] | null): boolean {
  if (map === null) {
    return true;
  }
  const opening = FENCE_MARKER.exec((lines[map[0]] ?? '').trim());
  if (opening === null) {
    return true;
  }
  const marker = opening[1] ?? '';
  const char = marker.charAt(0);
  for (let index = map[0] + 1; index < lines.length; index += 1) {
    const line = (lines[index] ?? '').trim();
    if (line.length >= marker.length && line.split('').every((character) => character === char)) {
      return true;
    }
  }
  return false;
}

/** `style="text-align:center"` — the only attribute markdown-it puts on a `th` token. */
function alignmentOf(token: Token): string | null {
  const style = token.attrGet('style');
  return style === null ? null : (/text-align:\s*(\w+)/.exec(style)?.[1] ?? null);
}

/** First word of a fence info string (`ts title="x.ts"` → `ts`). */
function languageOf(info: string): string {
  const trimmed = info.trim();
  const space = trimmed.indexOf(' ');
  return space === -1 ? trimmed : trimmed.slice(0, space);
}

// ── inline ────────────────────────────────────────────────────────────

function parseInline(tokens: readonly Token[]): readonly MarkdownInline[] {
  return parseInlineRange(tokens, { index: 0 });
}

function parseInlineRange(tokens: readonly Token[], cursor: Cursor): readonly MarkdownInline[] {
  const nodes: MarkdownInline[] = [];

  while (cursor.index < tokens.length) {
    const token = tokens[cursor.index];
    if (token === undefined) {
      break;
    }

    if (token.nesting === -1) {
      cursor.index += 1;
      break;
    }

    switch (token.type) {
      case 'text':
        cursor.index += 1;
        nodes.push({ kind: 'text', text: token.content });
        break;
      case 'code_inline':
        cursor.index += 1;
        nodes.push({ kind: 'code', text: token.content });
        break;
      case 'strong_open':
      case 'em_open':
      case 's_open': {
        const kind = token.type === 'strong_open' ? 'strong' : token.type === 'em_open' ? 'em' : 'del';
        cursor.index += 1;
        // The recursive call stops at (and consumes) the matching close token.
        nodes.push({ kind, children: parseInlineRange(tokens, cursor) });
        break;
      }
      case 'link_open': {
        const href = token.attrGet('href') ?? '';
        cursor.index += 1;
        nodes.push({ kind: 'link', href, children: parseInlineRange(tokens, cursor) });
        break;
      }
      case 'image':
        cursor.index += 1;
        nodes.push({ kind: 'image', src: token.attrGet('src') ?? '', alt: token.content });
        break;
      case 'math_inline':
        cursor.index += 1;
        nodes.push({
          kind: 'math',
          tex: token.content,
          source: token.info,
          display: DISPLAY_MARKUP.has(token.markup),
        });
        break;
      case 'softbreak':
        cursor.index += 1;
        nodes.push({ kind: 'break', hard: false });
        break;
      case 'hardbreak':
        cursor.index += 1;
        nodes.push({ kind: 'break', hard: true });
        break;
      default:
        cursor.index += 1;
        break;
    }
  }

  return nodes;
}

// ── math ──────────────────────────────────────────────────────────────

/**
 * Inline formulas: `$…$`, `$$…$$`, `\(…\)`, `\[…\]`.
 *
 * Three things make this safe in a markdown-it plugin:
 *
 * 1. **`$` and `\` are already terminator characters** for markdown-it's `text`
 *    rule, so this rule is reached at every candidate position;
 * 2. **code spans never reach it** — the `backticks` rule consumes a whole span
 *    (and fenced/indented code is a block token), so `` `$x$` `` stays literal;
 * 3. **`\(` is only reachable because this rule runs before `escape`** (see the
 *    registration above).
 *
 * Guards: a single `$` follows pandoc's `tex_math_dollars` rules (the opener must
 * not be followed by whitespace, the closer must not be preceded by whitespace and
 * must not be followed by a digit), which is what keeps prose like
 * `costs $100 and $200` text. The two-character delimiters are unambiguous enough
 * to skip those guards — `$$ \frac{a}{b} $$` is a legitimate spelling.
 */
function mathInline(state: StateInline, silent: boolean): boolean {
  const opening = mathOpeningAt(state.src, state.pos);
  if (opening === null) {
    return false;
  }

  const contentStart = state.pos + opening.open.length;
  if (contentStart >= state.posMax) {
    return false;
  }
  if (opening.open === '$' && isWhitespace(state.src.charCodeAt(contentStart))) {
    return false;
  }

  const close = findMathClose(state.src, contentStart, state.posMax, opening);
  if (close <= contentStart) {
    return false; // no closer
  }
  const tex = state.src.slice(contentStart, close);
  if (tex.trim() === '') {
    return false; // `$$`, `$$$$`, `$$ $$` — an empty formula is not a formula
  }
  if (opening.open === '$') {
    if (isWhitespace(state.src.charCodeAt(close - 1))) {
      return false;
    }
    if (isDigit(state.src.charCodeAt(close + opening.close.length))) {
      return false;
    }
  }

  const end = close + opening.close.length;
  if (!silent) {
    const token = state.push('math_inline', 'math', 0);
    token.markup = opening.open;
    token.content = tex;
    token.info = state.src.slice(state.pos, end);
  }
  state.pos = end;
  return true;
}

/**
 * Display formula as a block: a line that is exactly `$$` (or `\[`) opens it, a
 * line that is exactly `$$` (or `\]`) closes it.
 *
 * A single-line `$$…$$` is deliberately *not* handled here: it goes through the
 * paragraph and the inline rule picks it up, so "display math inside a sentence"
 * has one code path instead of two competing ones. An opener without a closer
 * returns `false`, which leaves the text to the paragraph rule — a half-typed
 * formula renders as text while it streams, and its content is never dropped.
 */
function mathBlock(state: StateBlock, startLine: number, endLine: number, silent: boolean): boolean {
  const opener = lineText(state, startLine).trim();
  const opening = opener === '$$' ? '$$' : opener === '\\[' ? '\\[' : null;
  if (opening === null) {
    return false;
  }
  const closer = opening === '$$' ? '$$' : '\\]';

  let closeLine = -1;
  for (let line = startLine + 1; line < endLine; line += 1) {
    if (lineText(state, line).trim() === closer) {
      closeLine = line;
      break;
    }
  }
  // Unterminated, or empty content: not a formula (yet).
  if (closeLine <= startLine + 1) {
    return false;
  }
  if (silent) {
    return true;
  }

  const lines: string[] = [];
  for (let line = startLine + 1; line < closeLine; line += 1) {
    lines.push(lineText(state, line));
  }
  const tex = lines.join('\n').trim();
  if (tex === '') {
    return false;
  }

  const token = state.push('math_block', 'math', 0);
  token.markup = opening;
  token.content = tex;
  // The literal source, delimiters included — the fallback when KaTeX refuses it.
  token.info = state.src.slice(lineStart(state, startLine), lineEnd(state, closeLine));
  token.map = [startLine, closeLine + 1];
  state.line = closeLine + 1;
  return true;
}

/**
 * Where one line's text starts (past its indentation) and ends.
 *
 * markdown-it fills `bMarks`/`tShift`/`eMarks` for every line it tokenizes; the
 * fallbacks only exist because indexed access is `number | undefined` under
 * `noUncheckedIndexedAccess`.
 */
function lineStart(state: StateBlock, line: number): number {
  return (state.bMarks[line] ?? 0) + (state.tShift[line] ?? 0);
}

function lineEnd(state: StateBlock, line: number): number {
  return state.eMarks[line] ?? state.src.length;
}

/** One line of the block source, without its leading indentation. */
function lineText(state: StateBlock, line: number): string {
  return state.src.slice(lineStart(state, line), lineEnd(state, line));
}

/** Delimiters whose formulas are display formulas. */
const DISPLAY_MARKUP: ReadonlySet<string> = new Set(['$$', '\\[']);

interface MathOpening {
  /** The delimiter as written (`$`, `$$`, `\(`, `\[`). */
  readonly open: string;
  readonly close: string;
}

/** The formula opening at `pos`, or `null` when this position does not start one. */
function mathOpeningAt(src: string, pos: number): MathOpening | null {
  if (src.charCodeAt(pos) === 0x24 /* $ */) {
    return src.charCodeAt(pos + 1) === 0x24 ? { open: '$$', close: '$$' } : { open: '$', close: '$' };
  }
  if (src.charCodeAt(pos) === 0x5c /* \ */) {
    const next = src.charCodeAt(pos + 1);
    if (next === 0x28 /* ( */) {
      return { open: '\\(', close: '\\)' };
    }
    if (next === 0x5b /* [ */) {
      return { open: '\\[', close: '\\]' };
    }
  }
  return null;
}

/**
 * Index of the closing delimiter in `[from, max)`, or `-1`.
 *
 * Inside a formula a backslash escapes the next character (`\$` is a dollar sign,
 * `\\` is LaTeX's line break), so those pairs are skipped — but the closing
 * delimiter is matched *first*, otherwise `\)` (which starts with a backslash)
 * could never close `\(`.
 */
function findMathClose(src: string, from: number, max: number, opening: MathOpening): number {
  for (let pos = from; pos < max; pos += 1) {
    if (src.startsWith(opening.close, pos)) {
      return pos;
    }
    if (src.charCodeAt(pos) === 0x5c /* \ */) {
      pos += 1;
    }
  }
  return -1;
}

/** Space, tab or newline — `charCodeAt` past the end is `NaN`, which is not one. */
function isWhitespace(code: number): boolean {
  return code === 0x20 || code === 0x09 || code === 0x0a || code === 0x0d;
}

function isDigit(code: number): boolean {
  return code >= 0x30 && code <= 0x39;
}
