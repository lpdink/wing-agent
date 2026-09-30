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

import { MAX_MATH_CHARS } from './math';

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
 * Formulas: `$…$`, `$$…$$`, `\(…\)`, `\[…\]` and bare AMS environments
 * (`\begin{align}…\end{align}`).
 *
 * The rules are aligned with the TUI lane, whose implementation is authoritative:
 * `crates/wing/src/render/markdown/math.rs` and the tables in
 * `docs/dev/tui-rendering.md` §2.5. The two pipelines apply them differently —
 * the TUI rewrites the source text before parsing (pulldown-cmark knows nothing
 * about `\(`, `\[` or a bare environment), while here the rules recognize the
 * same spans *without touching the input*: an unrecognized span is ordinary
 * text, so nothing can be rewritten or lost. This rule is safe in a markdown-it
 * plugin for four reasons:
 *
 * 1. **`$` and `\` are already terminator characters** for markdown-it's `text`
 *    rule, so this rule is reached at every candidate position;
 * 2. **code spans never reach it** — the `backticks` rule consumes a whole span
 *    (and fenced/indented code is a block token), so `` `$x$` `` stays literal;
 * 3. **`\(` is only reachable because this rule runs before `escape`** (see the
 *    registration above) — otherwise markdown's own escape handling would eat
 *    the backslash;
 * 4. **link destinations and titles never reach it either**: the `link` /
 *    `image` rules consume `](url "title")` as a whole, so a `\(` inside a URL
 *    cannot be mistaken for a formula (pinned by a test).
 *
 * Everything else on the TUI's opaque list is handled by {@link isOpaque}: HTML
 * blocks, inline HTML tags / autolinks, and link reference definition lines.
 *
 * Guards, mirroring the TUI normalizer rule by rule:
 *
 * - a single `$` follows pulldown's two rules: the opener must not be followed
 *   by whitespace, the closer must not be preceded by one. That is what keeps
 *   `costs $100 and $200` text; there is deliberately **no** "closer must not be
 *   followed by a digit" guard — pulldown has none (`$x$1` *is* the formula `x`
 *   followed by `1`), and pandoc's extra guard would be narrower than the TUI;
 * - `\(…\)`, `\[…\]` and a bare environment must not carry code or `$`-math of
 *   their own, must not touch an existing `$`, and must close within
 *   {@link MAX_MATH_CHARS} (the TUI's `bail_on_nested` / `fuses_with_dollar` /
 *   `MAX_SPAN` rules);
 * - they also stop being recognized after an unpaired `$$` in the same block,
 *   and their closer searches are charged against a per-block budget (the TUI's
 *   positional and work-budget rules — {@link disarmed} and
 *   {@link MAX_SCAN_WORK}).
 *
 * Degradation is identical on both sides: a span the rules do not recognize is
 * ordinary markdown text (with markdown's own escaping — exactly what the TUI
 * shows for a span its normalizer refuses), and a recognized formula that KaTeX
 * refuses or cannot fit falls back to its **complete source** (`math.ts`) —
 * never to half a formula.
 */

/** Delimiters whose formulas are display formulas. */
const DISPLAY_MARKUP: ReadonlySet<string> = new Set(['$$', '\\[']);

/**
 * Environments a bare `\begin{ENV}` is recognized for — the TUI's `MATH_ENVS`
 * list (a trailing `*` is a variant of the name, stripped before the lookup).
 * An environment outside this list stays exactly as the model wrote it, which is
 * what its unrenderable content would degrade to anyway.
 */
const MATH_ENVS: ReadonlySet<string> = new Set([
  // Multiline environments adapted by the TUI's engine.
  'align',
  'aligned',
  'alignat',
  'alignedat',
  'flalign',
  'split',
  'eqnarray',
  'gather',
  'multline',
  'center',
  'equation',
  'displaymath',
  'array',
  // Environments the vendored parser renders.
  'cases',
  'matrix',
  'pmatrix',
  'bmatrix',
  'Bmatrix',
  'vmatrix',
  'Vmatrix',
]);

/**
 * Bytes of closer-searching one inline block may spend — the TUI normalizer's
 * `MAX_SCAN_WORK`. Without it a block made of thousands of unterminated `\(`
 * would scan to the span limit at every one of them (quadratically); past the
 * budget the remaining LaTeX delimiters in that block stay literal, which is
 * always an allowed degradation. `$…$` / `$$…$$` are not charged: they are
 * unbounded in pulldown too, and the pattern cannot blow up (measured).
 */
const MAX_SCAN_WORK = 1 << 20;

/** Bytes charged per inline block, keyed by the (per-block) parser state. */
const work = new WeakMap<StateInline, number>();

/**
 * Inline blocks in which an unpaired `$$` has been met.
 *
 * From that point on the LaTeX-delimited forms stay literal: an inserted `$$`
 * would pair with the stray one, so the TUI normalizer refuses to rewrite there
 * and this side refuses to recognize there — both frontends then show the same
 * literal text. The rule is positional on purpose (it depends on what the scan
 * has already seen, never on a look-ahead) and a *single* `$` does not trigger
 * it: `$100` in prose is common and cannot pair with a `$$`.
 */
const disarmed = new WeakSet<StateInline>();

function mathInline(state: StateInline, silent: boolean): boolean {
  if (isOpaque(state, state.pos)) {
    return false;
  }
  const code = state.src.charCodeAt(state.pos);
  if (code === 0x5c /* \ */) {
    return latexFormula(state, silent);
  }
  if (code === 0x24 /* $ */) {
    return dollarFormula(state, silent);
  }
  return false;
}

/**
 * `$…$` (inline) and `$$…$$` (display).
 *
 * Display delimiters pair on their own rules — no whitespace constraints:
 * `$$ \frac{a}{b} $$` is a legitimate spelling, and pulldown agrees.
 */
function dollarFormula(state: StateInline, silent: boolean): boolean {
  const src = state.src;
  const pos = state.pos;
  const display = src.charCodeAt(pos + 1) === 0x24; /* $ */
  const closer = display ? '$$' : '$';
  const contentStart = pos + closer.length;
  if (!display && isWhitespace(src.charCodeAt(contentStart))) {
    return false; // `$ x$`, `$100 …`: pulldown opens only after a non-blank
  }
  const close = findCloser(src, contentStart, state.posMax, closer);
  if (close < contentStart) {
    // Never closed in this block. An unpaired `$$` disarms the rest of the block
    // for the LaTeX forms (see `disarmed`); a lone `$` does not.
    if (display) {
      disarm(state);
    }
    return false;
  }
  const tex = src.slice(contentStart, close);
  if (tex.trim() === '') {
    return false; // `$$`, `$$$$`, `$$ $$` — an empty formula is not a formula
  }
  if (!display && isWhitespace(src.charCodeAt(close - 1))) {
    return false; // `$100 and $200`: the closer follows the last character
  }
  const end = close + closer.length;
  pushMath(state, silent, src.slice(pos, end), tex, display ? '$$' : '$');
  state.pos = end;
  return true;
}

/**
 * The three dialects pulldown does not know and the TUI normalizes into `$…$` /
 * `$$…$$`: `\(…\)` (inline), `\[…\]` (display) and a bare environment (display).
 */
function latexFormula(state: StateInline, silent: boolean): boolean {
  if (disarmed.has(state)) {
    return false;
  }
  const src = state.src;
  const pos = state.pos;
  const next = src.charCodeAt(pos + 1);
  if (next === 0x28 /* ( */) {
    return delimitedFormula(state, silent, '\\)', false);
  }
  if (next === 0x5b /* [ */) {
    return delimitedFormula(state, silent, '\\]', true);
  }
  if (next === 0x62 /* b */ && src.startsWith('\\begin{', pos)) {
    return bareEnvironment(state, silent);
  }
  return false;
}

/** `\(…\)` / `\[…\]` — the same rules, one closing delimiter apart. */
function delimitedFormula(state: StateInline, silent: boolean, closer: string, display: boolean): boolean {
  const src = state.src;
  const pos = state.pos;
  const contentStart = pos + 2;
  const limit = searchLimit(state, contentStart);
  const close = limit < 0 ? -1 : findCloser(src, contentStart, limit, closer);
  if (close < 0) {
    return false; // never closed within the block / the span budget
  }
  const inner = src.slice(contentStart, close);
  if (inner.trim() === '') {
    return false; // `\(   \)` has nothing to render (the TUI leaves it alone)
  }
  if (spansCodeOrMath(inner)) {
    // A code span or a formula of its own: the TUI bails there, and the outcome
    // is the same — the delimiters stay literal and the inner `` `…` `` / `$…$`
    // renders as what it is.
    return false;
  }
  const end = close + 2;
  if (fusesWithDollar(src, pos, end)) {
    return false; // `\(x\)$$`: the delimiters would fuse into the existing `$`
  }
  pushMath(state, silent, src.slice(pos, end), inner, display ? '\\[' : '\\(');
  state.pos = end;
  return true;
}

/** A bare `\begin{ENV}…\end{ENV}` — display math, exactly as in the TUI. */
function bareEnvironment(state: StateInline, silent: boolean): boolean {
  const src = state.src;
  const pos = state.pos;
  const nameStart = pos + '\\begin{'.length;
  const nameEnd = src.indexOf('}', nameStart);
  if (nameEnd === -1) {
    return false;
  }
  const name = src.slice(nameStart, nameEnd);
  if (!MATH_ENVS.has(name.replace(/\*+$/, ''))) {
    return false; // an unknown environment stays exactly as it was written
  }
  const close = findEnvironmentEnd(state, nameEnd + 1, name);
  if (close < 0) {
    return false;
  }
  const end = close + `\\end{${name}}`.length;
  if (fusesWithDollar(src, pos, end)) {
    return false;
  }
  const source = src.slice(pos, end);
  // The environment *is* a display formula (that is how the TUI spells it after
  // normalization), so it carries the display markup for the AST mapper.
  pushMath(state, silent, source, source, '$$');
  state.pos = end;
  return true;
}

/**
 * Offset of the `\end{name}` matching the `\begin{name}` whose body starts at
 * `from`, or `-1` — the TUI's `find_env_end`.
 *
 * Same-name nesting is counted (`array` inside `array`); the walk stays inside
 * the span budget and refuses a body carrying code (`` ` ``) or `$`-math of its
 * own. Escaped characters are skipped, so an `\\end{name}` is not the closer.
 */
function findEnvironmentEnd(state: StateInline, from: number, name: string): number {
  const src = state.src;
  const limit = searchLimit(state, from);
  if (limit < 0) {
    return -1;
  }
  const begin = `\\begin{${name}}`;
  const end = `\\end{${name}}`;
  let depth = 0;
  let at = from;
  while (at < limit) {
    if (src.startsWith(end, at)) {
      if (depth === 0) {
        return at;
      }
      depth -= 1;
      at += end.length;
      continue;
    }
    if (src.startsWith(begin, at)) {
      depth += 1;
      at += begin.length;
      continue;
    }
    const code = src.charCodeAt(at);
    if (code === 0x60 /* ` */ || code === 0x24 /* $ */) {
      return -1;
    }
    at += code === 0x5c /* \ */ ? 2 : 1;
  }
  return -1;
}

/**
 * End of the window a closer may be found in: the end of the block, or
 * {@link MAX_MATH_CHARS} characters on.
 *
 * `8192` is the render gate's own budget (`math.ts`, the "show the source
 * instead" threshold) and the TUI normalizer's `MAX_SPAN`: a span that would not
 * fit it could not be laid out anyway, so it stays literal text — the same thing
 * the TUI shows for it. The window is charged against {@link MAX_SCAN_WORK};
 * `-1` means the block's budget is spent and nothing more is recognized.
 */
function searchLimit(state: StateInline, from: number): number {
  const limit = Math.min(state.posMax, from + MAX_MATH_CHARS);
  return charge(state, limit - from) ? limit : -1;
}

/** Pay `bytes` from the block's search budget; `false` once it is spent. */
function charge(state: StateInline, bytes: number): boolean {
  const spent = (work.get(state) ?? 0) + bytes;
  work.set(state, spent);
  return spent <= MAX_SCAN_WORK;
}

/** Remember that this block met an unpaired `$$` (see {@link disarmed}). */
function disarm(state: StateInline): void {
  disarmed.add(state);
}

/**
 * Whether the delimiters a rewrite would insert at `[start, end)` would fuse
 * with a `$` that is already there — the TUI's `fuses_with_dollar`. Sources that
 * put a `$` right next to a delimiter are left alone instead; showing the source
 * is always allowed.
 */
function fusesWithDollar(src: string, start: number, end: number): boolean {
  return (start > 0 && src.charCodeAt(start - 1) === 0x24) || src.charCodeAt(end) === 0x24;
}

/** True when a span carries code or `$`-math of its own (the TUI bails there). */
function spansCodeOrMath(inner: string): boolean {
  return inner.includes('`') || inner.includes('$');
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
  const opening = lineEquals(state, startLine, '$$')
    ? '$$'
    : lineEquals(state, startLine, '\\[')
      ? '\\['
      : null;
  if (opening === null) {
    return false;
  }
  if (insideHtmlBlock(state, startLine)) {
    // `<div>` … blank line is HTML: markdown is not parsed there, so the TUI
    // shows these lines verbatim and so do we.
    return false;
  }
  const closer = opening === '$$' ? '$$' : '\\]';
  if (!mightHaveCloser(state, closer)) {
    return false; // no line can close it — do not walk the block at all
  }

  let closeLine = -1;
  for (let line = startLine + 1; line < endLine; line += 1) {
    if (lineEquals(state, line, closer)) {
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

/**
 * Whether `line` is inside an HTML block: the walk goes back to the previous
 * blank line looking for the line that starts one (a block rule only sees the
 * line it is asked about, hence the backward direction). A blank line ends the
 * block, so text after it is markdown again — the TUI's rule.
 *
 * Three things keep it cheap: nothing is walked when the document has no `<`
 * (the common case), the walk is allocation-free and gives up after
 * {@link MAX_MATH_CHARS} characters, and a query for the line right after the
 * last one only has to look at that line ({@link HtmlWalk}) — which is what
 * makes a document full of `$$` lines linear instead of quadratic. Past a bound
 * the line counts as ordinary text: at worst a formula is recognized where the
 * TUI would have shown it literally, and no content is ever lost either way.
 */
function insideHtmlBlock(state: StateBlock, line: number): boolean {
  if (!hasTagStart(state)) {
    return false; // no `<` anywhere: nothing can start an HTML block
  }
  const src = state.src;
  const start = lineStart(state, line);
  const end = lineEnd(state, line);
  const cached = WALKS.get(state);
  if (cached !== undefined && cached.line === line - 1) {
    // The paragraph terminator probes ask about consecutive lines. The cached
    // `tagLine` covers the lines above; only the new one is unknown.
    if (isBlankRange(src, start, end)) {
      WALKS.set(state, { line, tagLine: -1 }); // a blank line starts a new run
      return false;
    }
    const tagLine = startsHtmlBlock(src, start, end) ? line : cached.tagLine;
    WALKS.set(state, { line, tagLine });
    return tagLine !== -1;
  }

  let budget = MAX_MATH_CHARS;
  let tagLine = -1;
  for (let at = line; at >= 0 && budget > 0; at -= 1) {
    const from = lineStart(state, at);
    const to = lineEnd(state, at);
    budget -= to - from;
    if (isBlankRange(src, from, to)) {
      break; // the run starts above this line; whatever is above is another block
    }
    if (tagLine === -1 && startsHtmlBlock(src, from, to)) {
      tagLine = at;
    }
  }
  WALKS.set(state, { line, tagLine });
  return tagLine !== -1;
}

/**
 * The last HTML-block walk, so the next line can be checked on its own.
 *
 * `tagLine` is the nearest line at or above `line`, in the same run, that starts
 * an HTML block (`-1` when there is none). Extending it assumes the lines above
 * are still read the same way — they are, unless the parser entered another
 * container between two *consecutive* queries, which cannot happen (a container
 * start is itself a line).
 */
interface HtmlWalk {
  readonly line: number;
  readonly tagLine: number;
}

const WALKS = new WeakMap<StateBlock, HtmlWalk>();

/** Whether `[start, end)` holds only whitespace. */
function isBlankRange(src: string, start: number, end: number): boolean {
  for (let at = start; at < end; at += 1) {
    if (!isWhitespace(src.charCodeAt(at))) {
      return false;
    }
  }
  return true;
}

/** Whether the document contains a `<` at all (constant per parser state). */
function hasTagStart(state: StateBlock): boolean {
  const cached = TAGS.get(state);
  if (cached !== undefined) {
    return cached;
  }
  const value = state.src.includes('<');
  TAGS.set(state, value);
  return value;
}

/**
 * Whether a line *could* equal `closer` (ignoring surrounding whitespace): a
 * *necessary* condition, used to skip the line walk when the closer does not
 * occur in the source at all — a document of thousands of never-closed `\[`
 * lines must not walk itself once per line. The answer depends only on the
 * untouched source, which never changes while a document is parsed.
 */
function mightHaveCloser(state: StateBlock, closer: string): boolean {
  let missing = CLOSERS.get(state);
  if (missing === undefined) {
    missing = new Set<string>();
    CLOSERS.set(state, missing);
  }
  if (missing.has(closer)) {
    return false;
  }
  if (state.src.includes(closer)) {
    return true;
  }
  missing.add(closer);
  return false;
}

const TAGS = new WeakMap<StateBlock, boolean>();
const CLOSERS = new WeakMap<StateBlock, Set<string>>();

/**
 * Whether line `line`'s content is exactly `text`, ignoring surrounding
 * whitespace — allocation-free, because the block rule walks lines with it.
 */
function lineEquals(state: StateBlock, line: number, text: string): boolean {
  const src = state.src;
  let start = lineStart(state, line);
  let end = lineEnd(state, line);
  while (start < end && isWhitespace(src.charCodeAt(start))) {
    start += 1;
  }
  while (end > start && isWhitespace(src.charCodeAt(end - 1))) {
    end -= 1;
  }
  if (end - start !== text.length) {
    return false;
  }
  for (let at = 0; at < text.length; at += 1) {
    if (src.charCodeAt(start + at) !== text.charCodeAt(at)) {
      return false;
    }
  }
  return true;
}

/** Push the `math_inline` token the AST mapper turns into a `math` node. */
function pushMath(state: StateInline, silent: boolean, source: string, tex: string, markup: string): void {
  if (silent) {
    return;
  }
  const token = state.push('math_inline', 'math', 0);
  token.markup = markup;
  token.content = tex;
  token.info = source;
}

/**
 * Index of the closing delimiter in `[from, limit)`, or `-1`.
 *
 * Inside a formula a backslash escapes the next character (`\$` is a dollar sign,
 * `\\` is LaTeX's line break), so those pairs are skipped — but the closing
 * delimiter is matched *first*, otherwise `\)` (which starts with a backslash)
 * could never close `\(`.
 */
function findCloser(src: string, from: number, limit: number, closer: string): number {
  for (let pos = from; pos < limit; pos += 1) {
    if (src.startsWith(closer, pos)) {
      return pos;
    }
    if (src.charCodeAt(pos) === 0x5c /* \ */) {
      pos += 1;
    }
  }
  return -1;
}

/**
 * Space, tab, newline or form feed — pulldown's `is_ascii_whitespace` (the two
 * `$` guards come from there). `charCodeAt` past the end is `NaN`, which is not
 * one.
 */
function isWhitespace(code: number): boolean {
  return code === 0x20 || code === 0x09 || code === 0x0a || code === 0x0c || code === 0x0d;
}

// ── opaque regions ────────────────────────────────────────────────────

/**
 * Regions of one inline block (a paragraph, a heading, a table cell …) where
 * math must not be recognized — the TUI normalizer's opaque list, expressed
 * against markdown-it's own token boundaries:
 *
 * - **HTML block**: a line whose content starts with a tag (`<div>`, `<!--`,
 *   `<?…`, `<!DOCTYPE`) is opaque to the end of the block, which is the next
 *   blank line — markdown-it hands the inline parser one block at a time, so
 *   "the rest of the block" is the whole tail here. `html: false` means this is
 *   about *text that looks like* HTML; nothing is parsed as markup.
 * - **Inline HTML tag / autolink**: `<a href="…">`, `<http://…>` — everything
 *   between the `<` and its `>` is opaque, while the prose between two tags is
 *   not (`before <b>\(x\)</b> after` still recognizes `\(x\)`, as the TUI does).
 * - **Link reference definition line**: `[label]: url "title"` — the destination
 *   is a URL, not prose. The block parser consumes a *real* definition before any
 *   inline rule runs; this covers the definition-shaped line inside a paragraph,
 *   where the TUI's check is line-based too.
 *
 * Code spans and fenced/indented code need no entry (markdown-it turns them into
 * `code_inline` / `fence` / `code_block` tokens before any inline rule runs), and
 * neither do link destinations and titles (the `link` / `image` rules consume
 * them whole). Tests pin both.
 */
interface OpaqueRegions {
  /** Offset each line of the block starts at (ascending). */
  readonly lineStarts: readonly number[];
  /** Per line: `true` when math must not be recognized anywhere on that line. */
  readonly lines: readonly boolean[];
  /** `[start, end)` of every inline HTML tag / autolink in the block. */
  readonly tags: readonly (readonly [number, number])[];
}

/**
 * One entry per inline block: `StateInline` objects are created per block and
 * short-lived, so the cache costs nothing and the callers stay pure.
 */
const REGIONS = new WeakMap<StateInline, OpaqueRegions>();

/** Whether `pos` sits in one of the regions above. */
function isOpaque(state: StateInline, pos: number): boolean {
  const regions = regionsOf(state);
  if (inTag(regions, pos)) {
    return true;
  }
  return regions.lines[lineAt(regions, pos)] === true;
}

function regionsOf(state: StateInline): OpaqueRegions {
  const cached = REGIONS.get(state);
  if (cached !== undefined) {
    return cached;
  }

  const src = state.src;
  const lineStarts: number[] = [];
  const lines: boolean[] = [];
  let inHtmlBlock = false;
  let at = 0;
  for (;;) {
    lineStarts.push(at);
    const newline = src.indexOf('\n', at);
    const end = newline === -1 ? src.length : newline;
    const content = src.slice(at, end).replace(/^ {0,3}/, '');
    if (content.trim() === '') {
      inHtmlBlock = false; // a blank line ends the HTML block
      lines.push(false);
    } else if (inHtmlBlock) {
      lines.push(true);
    } else if (startsHtmlBlock(src, at, end)) {
      inHtmlBlock = true;
      lines.push(true);
    } else {
      lines.push(isReferenceDefinition(content));
    }
    if (newline === -1) {
      break;
    }
    at = newline + 1;
  }

  const regions: OpaqueRegions = { lineStarts, lines, tags: tagSpans(src) };
  REGIONS.set(state, regions);
  return regions;
}

/**
 * `[start, end)` of every inline HTML tag or autolink in `src`.
 *
 * A `<` only opens a region when a tag character follows (so `a <- b`, `x < y`
 * and an unclosed `<` are ordinary text — the TUI's rule), and the region runs to
 * the first `>`, which is what makes an attribute value opaque too.
 */
function tagSpans(src: string): readonly (readonly [number, number])[] {
  const spans: (readonly [number, number])[] = [];
  let cursor = 0;
  for (;;) {
    const open = src.indexOf('<', cursor);
    if (open === -1) {
      return spans;
    }
    const close = isTagStart(src.charCodeAt(open + 1)) ? src.indexOf('>', open + 1) : -1;
    if (close === -1) {
      cursor = open + 1;
      continue;
    }
    spans.push([open, close + 1]);
    cursor = close + 1;
  }
}

/** Index of the line containing `pos` (binary search over the line starts). */
function lineAt(regions: OpaqueRegions, pos: number): number {
  const starts = regions.lineStarts;
  let low = 0;
  let high = starts.length - 1;
  while (low < high) {
    const mid = (low + high + 1) >> 1;
    if ((starts[mid] ?? 0) <= pos) {
      low = mid;
    } else {
      high = mid - 1;
    }
  }
  return low;
}

/** Whether `pos` is inside one of the tag spans (binary search). */
function inTag(regions: OpaqueRegions, pos: number): boolean {
  const spans = regions.tags;
  let low = 0;
  let high = spans.length - 1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    const span = spans[mid];
    if (span === undefined) {
      return false;
    }
    if (pos < span[0]) {
      high = mid - 1;
    } else if (pos >= span[1]) {
      low = mid + 1;
    } else {
      return true;
    }
  }
  return false;
}

/** `<` + a tag character opens an HTML tag: `<div>`, `</p>`, `<!--`, `<?xml`. */
function isTagStart(code: number): boolean {
  return (
    (code >= 0x30 && code <= 0x39) /* 0-9 */ ||
    (code >= 0x41 && code <= 0x5a) /* A-Z */ ||
    (code >= 0x61 && code <= 0x7a) /* a-z */ ||
    code === 0x2f /* / */ ||
    code === 0x21 /* ! */ ||
    code === 0x3f /* ? */
  );
}

/**
 * Whether a line's content (`[start, end)`) starts an HTML block: `<` + a tag
 * character, with up to three leading spaces (CommonMark's allowance). This
 * mirrors the TUI's `is_html_block_start`; the prefixes a block can carry (`> `,
 * list markers) are stripped by markdown-it before any rule sees the content.
 */
function startsHtmlBlock(src: string, start: number, end: number): boolean {
  let at = start;
  let spaces = 0;
  while (at < end && spaces < 3 && src.charCodeAt(at) === 0x20) {
    at += 1;
    spaces += 1;
  }
  return src.charCodeAt(at) === 0x3c /* < */ && isTagStart(src.charCodeAt(at + 1));
}

/**
 * Whether the line is a link reference definition (`[label]: url "title"`) — the
 * TUI's `is_reference_definition`, line-based like there. Only the line itself is
 * opaque: a title wrapped onto the next line is rare, and treating the whole
 * block as opaque would cost more than it protects.
 */
function isReferenceDefinition(content: string): boolean {
  const text = content.replace(/^ {0,3}/, '');
  if (text.charAt(0) !== '[') {
    return false;
  }
  const close = text.indexOf(']');
  // `]` missing, or a label carrying whitespace: not a definition.
  if (close === -1 || /\s/.test(text.slice(1, close))) {
    return false;
  }
  return text.charAt(close + 1) === ':';
}
