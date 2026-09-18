/**
 * Markdown AST → React.
 *
 * The renderer creates every element itself: no `dangerouslySetInnerHTML`, no
 * model-supplied markup. Links hand the URL to the *host* (`openLink`) instead
 * of navigating the webview; code blocks get the copy affordance.
 */

import { memo, useMemo } from 'react';
import type { ReactElement, ReactNode } from 'react';

import { postToHost } from '../../bridge/channel';
import { useCopyFeedback } from '../interaction';
import styles from '../../styles/markdown.module.css';
import type { MarkdownInline, MarkdownNode } from './parse';
import { highlightCode } from './highlight';

const HEADINGS = ['h1', 'h2', 'h3', 'h4', 'h5', 'h6'] as const;

/** Block-level children. */
export function MarkdownNodes({
  nodes,
  live = false,
  trailing = null,
}: {
  readonly nodes: readonly MarkdownNode[];
  /** True while this content is still growing (see `MarkdownBlock`). */
  readonly live?: boolean;
  /**
   * Rendered inside the last paragraph when there is one (the streaming caret
   * belongs to the text that is still arriving), otherwise after the last block —
   * a block-level caret would add a paragraph margin that the follow-up chunk
   * would then remove again.
   */
  readonly trailing?: ReactNode;
}): ReactElement {
  const lastIndex = nodes.length - 1;
  const inline = trailing !== null && nodes[lastIndex]?.kind === 'paragraph';

  return (
    <>
      {nodes.map((node, index) => (
        <MarkdownNodeView
          key={index}
          node={node}
          live={live}
          trailing={index === lastIndex && inline ? trailing : null}
        />
      ))}
      {trailing !== null && !inline ? trailing : null}
    </>
  );
}

function MarkdownNodeView({
  node,
  live,
  trailing,
}: {
  readonly node: MarkdownNode;
  readonly live: boolean;
  readonly trailing?: ReactNode | undefined;
}): ReactElement | null {
  switch (node.kind) {
    case 'paragraph':
      return (
        <p className={styles.paragraph}>
          <InlineNodes nodes={node.children} />
          {trailing}
        </p>
      );
    case 'heading': {
      // VS Code's chat markdown styles h1..h3 (xxl / xl / l, CHAT:457-477); deeper
      // levels fall back to the body size, which is what the markup already does.
      const level = Math.min(Math.max(node.level, 1), 6);
      const Heading = HEADINGS[level - 1] ?? 'h1';
      return (
        <Heading className={styles.heading} data-level={level}>
          <InlineNodes nodes={node.children} />
        </Heading>
      );
    }
    case 'code':
      return <CodeBlock lang={node.lang} code={node.code} live={live} closed={node.closed} />;
    case 'quote':
      return (
        <blockquote className={styles.quote}>
          <MarkdownNodes nodes={node.children} live={live} />
        </blockquote>
      );
    case 'list':
      return node.ordered ? (
        <ol className={styles.list} start={node.start}>
          <ListItemNodes items={node.items} live={live} />
        </ol>
      ) : (
        <ul className={styles.list}>
          <ListItemNodes items={node.items} live={live} />
        </ul>
      );
    case 'table':
      return (
        <div className={styles.tableScroll}>
          <table className={styles.table} data-testid="md-table">
            <thead>
              {node.head.map((row, rowIndex) => (
                <tr key={rowIndex}>
                  {row.map((cell, cellIndex) => (
                    <th key={cellIndex} data-align={node.aligns[cellIndex] ?? undefined}>
                      <InlineNodes nodes={cell} />
                    </th>
                  ))}
                </tr>
              ))}
            </thead>
            <tbody>
              {node.rows.map((row, rowIndex) => (
                <tr key={rowIndex}>
                  {row.map((cell, cellIndex) => (
                    <td key={cellIndex} data-align={node.aligns[cellIndex] ?? undefined}>
                      <InlineNodes nodes={cell} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    case 'rule':
      return <hr className={styles.rule} />;
    default:
      return null;
  }
}

function ListItemNodes({
  items,
  live,
}: {
  readonly items: readonly (readonly MarkdownNode[])[];
  readonly live: boolean;
}): ReactElement {
  return (
    <>
      {items.map((item, index) => (
        <li key={index} className={styles.listItem}>
          <MarkdownNodes nodes={item} live={live} />
        </li>
      ))}
    </>
  );
}

function InlineNodes({ nodes }: { readonly nodes: readonly MarkdownInline[] }): ReactElement {
  return (
    <>
      {nodes.map((node, index) => (
        <InlineNodeView key={index} node={node} />
      ))}
    </>
  );
}

function InlineNodeView({ node }: { readonly node: MarkdownInline }): ReactElement | null {
  switch (node.kind) {
    case 'text':
      return <>{node.text}</>;
    case 'code':
      return <code className={styles.inlineCode}>{node.text}</code>;
    case 'strong':
      return (
        <strong>
          <InlineNodes nodes={node.children} />
        </strong>
      );
    case 'em':
      return (
        <em>
          <InlineNodes nodes={node.children} />
        </em>
      );
    case 'del':
      return (
        <del>
          <InlineNodes nodes={node.children} />
        </del>
      );
    case 'link':
      return (
        <a
          className={styles.link}
          href={node.href}
          onClick={(event) => {
            event.preventDefault();
            postToHost({ type: 'openLink', href: node.href });
          }}
        >
          <InlineNodes nodes={node.children} />
        </a>
      );
    case 'image':
      // Remote images are blocked by the webview CSP (`img-src` covers the webview
      // origin + data:), so an image becomes a link to itself — the alt text is
      // still visible and the user can open it in a browser.
      return (
        <a
          className={styles.link}
          href={node.src}
          onClick={(event) => {
            event.preventDefault();
            postToHost({ type: 'openLink', href: node.src });
          }}
        >
          {node.alt === '' ? node.src : node.alt}
        </a>
      );
    case 'break':
      return <br />;
    default:
      return null;
  }
}

// ── code blocks ───────────────────────────────────────────────────────

/**
 * One fenced (or indented) code block.
 *
 * ## Highlighting policy
 *
 * Highlighting is memoized on `(code, lang, live)`, and it is **skipped while the
 * block is still growing** (`live`) or when the block is larger than
 * {@link HIGHLIGHT_MAX_CHARS}:
 *
 * - A live block is re-rendered on every `append_text` chunk. Highlighting it each
 *   time re-tokenizes the whole growing block — measured at ~50 ms per pass for a
 *   500-line block, i.e. **2.2 s of CPU** for the 100 chunks that produce it (and
 *   ~450 ms for a single 5000-line pass). Rendering it as plain text while it
 *   grows costs nothing; the block is highlighted exactly once, when it becomes
 *   stable.
 * - The size cap keeps that single pass bounded: shiki needs ~53 ms for 30 KB and
 *   ~447 ms for 300 KB in this bundle, so above {@link HIGHLIGHT_MAX_CHARS} the
 *   one-off cost is not worth the colour.
 *
 * Either way the DOM shape is identical (one `<span>` per token, one `<span>` per
 * line), so promoting a block only changes colours — no reflow, no jump.
 *
 * A failed highlight (unknown language, engine unavailable) degrades to plain
 * text, never an error.
 */
export const CodeBlock = memo(function CodeBlock({
  lang,
  code,
  live = false,
  closed = true,
}: {
  readonly lang: string;
  readonly code: string;
  /** True while the block is still being streamed. */
  readonly live?: boolean;
  /** False while the closing fence has not been written yet. */
  readonly closed?: boolean;
}): ReactElement {
  // Only a code block that can still grow is deferred; a closed fence inside a
  // live block is final. `growing` (not `live`) is the memo key: promoting the
  // block to stable changes `live` but not `growing`, so the highlight is computed
  // exactly once per code content.
  const growing = live && !closed;
  const lines = useMemo(() => codeLines(code, lang, growing), [code, lang, growing]);
  const [copied, reportCopied] = useCopyFeedback();

  return (
    <div className={styles.codeBlock} data-code-lang={lang} data-code-live={live ? 'true' : 'false'}>
      <div className={styles.codeHeader}>
        {lang === '' ? null : <span className={styles.codeLang}>{lang}</span>}
        <button
          type="button"
          className={styles.codeCopy}
          data-copied={copied ? 'true' : 'false'}
          onClick={() => {
            postToHost({ type: 'copyText', text: code });
            reportCopied();
          }}
        >
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
      <pre className={styles.codePre} data-testid="md-code">
        <code>
          {lines.map((line, index) => (
            // Code lines are positional by nature.
            <span key={index} className={styles.codeLine}>
              {line.map((token, tokenIndex) => (
                <span key={tokenIndex} className={styles.codeToken} style={tokenStyle(token)}>
                  {token.content}
                </span>
              ))}
              {'\n'}
            </span>
          ))}
        </code>
      </pre>
    </div>
  );
});

/**
 * Largest block we are willing to tokenize in one pass: shiki's JS engine
 * measures ~53 ms / 30 KB and ~447 ms / 300 KB in this bundle, and a single pass
 * must not be noticeable.
 */
const HIGHLIGHT_MAX_CHARS = 32 * 1024;

/** Per-theme token style, or none for plain text. */
const PLAIN_STYLE: TokenDecorations = { color: null, italic: false, bold: false, underline: false };

interface CodeLine {
  readonly content: string;
  readonly light: TokenDecorations;
  readonly dark: TokenDecorations;
}

/** Highlight a finished block, or fall back to plain lines. */
function codeLines(code: string, lang: string, growing: boolean): readonly (readonly CodeLine[])[] {
  if (!growing && code.length <= HIGHLIGHT_MAX_CHARS) {
    const highlighted = highlightCode(code, lang);
    if (highlighted !== null) {
      return highlighted;
    }
  }
  return plainLines(code);
}

/**
 * The same line structure without colours — keeps the DOM shape (and therefore
 * the layout) identical to the highlighted rendering.
 */
function plainLines(code: string): readonly (readonly CodeLine[])[] {
  const lines = code.split('\n');
  // Mirror shiki's `splitLines`: a trailing newline does not create an extra line.
  if (lines.length > 1 && lines[lines.length - 1] === '') {
    lines.pop();
  }
  return lines.map((content) => [{ content, light: PLAIN_STYLE, dark: PLAIN_STYLE }]);
}

/**
 * Per-token colours ride on CSS custom properties so the *stylesheet* picks the
 * variant that matches the active VS Code theme (`--shiki-light` / `--shiki-dark`
 * are defined by shiki's bundled `light-plus` / `dark-plus` themes, i.e. Light+ /
 * Dark+, which Light Modern / Dark Modern inherit).
 */
function tokenStyle(token: CodeLine): Record<string, string> {
  const style: Record<string, string> = {};
  if (token.light.color !== null) {
    style['--shiki-light'] = token.light.color;
  }
  if (token.dark.color !== null) {
    style['--shiki-dark'] = token.dark.color;
  }
  const decorations = [
    ['italic', token.light.italic || token.dark.italic],
    ['bold', token.light.bold || token.dark.bold],
    ['underline', token.light.underline || token.dark.underline],
  ] as const;
  for (const [decoration, on] of decorations) {
    if (on) {
      style[`--shiki-${decoration}`] = decoration;
    }
  }
  return style;
}

interface TokenDecorations {
  readonly color: string | null;
  readonly italic: boolean;
  readonly bold: boolean;
  readonly underline: boolean;
}
