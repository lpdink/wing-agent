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
  trailing,
}: {
  readonly nodes: readonly MarkdownNode[];
  /** Rendered inside the last paragraph when there is one (the streaming caret). */
  readonly trailing?: ReactNode;
}): ReactElement {
  return (
    <>
      {nodes.map((node, index) => (
        <MarkdownNodeView key={index} node={node} trailing={index === nodes.length - 1 ? trailing : null} />
      ))}
    </>
  );
}

function MarkdownNodeView({
  node,
  trailing,
}: {
  readonly node: MarkdownNode;
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
      return <CodeBlock lang={node.lang} code={node.code} />;
    case 'quote':
      return (
        <blockquote className={styles.quote}>
          <MarkdownNodes nodes={node.children} />
        </blockquote>
      );
    case 'list':
      return node.ordered ? (
        <ol className={styles.list} start={node.start}>
          <ListItemNodes items={node.items} />
        </ol>
      ) : (
        <ul className={styles.list}>
          <ListItemNodes items={node.items} />
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

function ListItemNodes({ items }: { readonly items: readonly (readonly MarkdownNode[])[] }): ReactElement {
  return (
    <>
      {items.map((item, index) => (
        <li key={index} className={styles.listItem}>
          <MarkdownNodes nodes={item} />
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
 * Highlighting is memoized on `(code, lang)`; a failed highlight (unknown
 * language, engine unavailable) degrades to plain text, never an error.
 */
export const CodeBlock = memo(function CodeBlock({
  lang,
  code,
}: {
  readonly lang: string;
  readonly code: string;
}): ReactElement {
  const lines = useMemo(() => highlightCode(code, lang), [code, lang]);
  const [copied, reportCopied] = useCopyFeedback();

  return (
    <div className={styles.codeBlock} data-code-lang={lang}>
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
          {lines === null
            ? code
            : lines.map((line, index) => (
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
 * Per-token colours ride on CSS custom properties so the *stylesheet* picks the
 * variant that matches the active VS Code theme (`--shiki-light` / `--shiki-dark`
 * are defined by shiki's bundled `light-plus` / `dark-plus` themes, i.e. Light+ /
 * Dark+, which Light Modern / Dark Modern inherit).
 */
function tokenStyle(token: {
  readonly light: TokenDecorations;
  readonly dark: TokenDecorations;
}): Record<string, string> {
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
