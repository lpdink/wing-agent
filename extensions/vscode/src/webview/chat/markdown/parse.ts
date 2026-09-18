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

// ── AST ───────────────────────────────────────────────────────────────

export type MarkdownInline =
  | { readonly kind: 'text'; readonly text: string }
  | { readonly kind: 'code'; readonly text: string }
  | { readonly kind: 'strong'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'em'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'del'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'link'; readonly href: string; readonly children: readonly MarkdownInline[] }
  /** Remote images cannot load under the webview CSP (`img-src` excludes http), so they render as links. */
  | { readonly kind: 'image'; readonly src: string; readonly alt: string }
  | { readonly kind: 'break'; readonly hard: boolean };

export type MarkdownNode =
  | { readonly kind: 'paragraph'; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'heading'; readonly level: number; readonly children: readonly MarkdownInline[] }
  | { readonly kind: 'code'; readonly lang: string; readonly code: string }
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

/** Parse one Markdown block into render-ready nodes. Pure: same source → same AST. */
export function parseMarkdown(source: string): readonly MarkdownNode[] {
  return parseBlocks(markdown.parse(source, {}), { index: 0 }, null);
}

/** Mutable position in the token stream. */
interface Cursor {
  index: number;
}

/**
 * Parse block tokens until `stop` (a closing token type; `null` = end of input).
 * The stop token itself is consumed.
 */
function parseBlocks(tokens: readonly Token[], cursor: Cursor, stop: string | null): MarkdownNode[] {
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
        nodes.push({ kind: 'code', lang: languageOf(token.info), code: token.content });
        break;
      case 'code_block':
        cursor.index += 1;
        nodes.push({ kind: 'code', lang: '', code: token.content });
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
function parseInlineAfter(tokens: readonly Token[], cursor: Cursor): readonly MarkdownInline[] {
  const inline = tokens[cursor.index + 1];
  const children = inline !== undefined && inline.type === 'inline' ? (inline.children ?? []) : [];
  cursor.index += inline === undefined ? 1 : 3;
  return parseInline(children);
}

function parseList(tokens: readonly Token[], cursor: Cursor, open: Token): MarkdownNode {
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

function parseTable(tokens: readonly Token[], cursor: Cursor): MarkdownNode {
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
