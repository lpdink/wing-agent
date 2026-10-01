// Preview harness for the 06b ports (dev-only; not part of any product build).
//
// Renders `CodeBlock` / `CodeToolbar`, `TerminalBlock` and `DiffBlock` — plus a
// little chrome so light/dark can both be screenshotted — against fake data. The
// imports reach into `src/` by path on purpose: this page lives inside the package
// and is not a consumer contract.

import { useState } from 'react';
import { createRoot } from 'react-dom/client';

import type { DiffCellModel } from '@wing-agent/session';

import { CodeBlock } from '../../src/markdown/CodeBlock';
import { DiffBlock } from '../../src/tool/DiffBlock';
import { TerminalBlock } from '../../src/tool/TerminalBlock';
import type { DiffBlockLabels } from '../../src/tool/DiffBlock';
import type { TerminalBlockLabels } from '../../src/tool/TerminalBlock';

import '../../src/styles/design-platform.css';
import '../../src/styles/base.css';
import '../../src/styles/scrollbar.css';
import '../../src/styles/focus.css';
import '../../src/styles/shiki.css';
import './preview.css';

const ESC = '\u001b';

const CODE_TOOLBAR_LABELS = { codeLabel: 'Code', wrapLabel: 'Wrap lines', unwrapLabel: 'Do not wrap' };

const TERMINAL_LABELS: TerminalBlockLabels = {
  signal: (signal) => `killed by ${signal}`,
  exitCode: (code) => `exit ${code}`,
  noExitCode: 'no exit code',
  running: 'Running',
  failed: 'Failed',
  done: 'Done',
  copy: 'Copy',
  copied: 'Copied',
  noOutput: 'No output',
  collapseAria: 'Collapse output',
  collapse: 'Collapse',
  expandAria: (hidden) => `Show ${hidden} more lines`,
  expand: (hidden) => `… ${hidden} more lines`,
};

const DIFF_LABELS: DiffBlockLabels = {
  ...CODE_TOOLBAR_LABELS,
  copy: 'Copy',
  copied: 'Copied',
  collapseAria: 'Collapse diff',
  collapse: 'Collapse',
  expandAria: (hidden) => `Show ${hidden} more lines`,
  expand: (hidden) => `… ${hidden} more lines`,
};

const TYPESCRIPT_SAMPLE = `export function selectActiveSession(state: AppState): SessionViewModel | null {
  const id = state.activeSessionId;
  if (id === null) return null;
  return state.sessions.find((session) => session.sessionId === id) ?? null;
}

// A deliberately long line so the wrap toggle has something to do:
export const LONG_LINE = 'the quick brown fox jumps over the lazy dog, twice, because once was not enough for a demo';`;

const PLAIN_SAMPLE = `$ wing ps
ID                                    NAME             STATUS
a2f0c1e8-7b31-4d5a-9f01-3c6d2e5b8a44  fix the parser   waiting-for-input`;

const ANSI_OUTPUT = [
  `${ESC}[1m==> Working tree status${ESC}[0m`,
  `${ESC}[36mOn branch${ESC}[0m ${ESC}[1mwing-app/l1${ESC}[0m`,
  `  ${ESC}[32mmodified:   packages/ui/src/index.ts${ESC}[0m`,
  `  ${ESC}[32mmodified:   packages/ui/src/markdown/CodeBlock.tsx${ESC}[0m`,
  `  ${ESC}[31mdeleted:    packages/ui/src/tool/old.ts${ESC}[0m`,
  `${ESC}[33mwarning:${ESC}[0m LF will be replaced by CRLF the next time Git touches it`,
  `${ESC}[90mhint: use "git restore" to discard changes in working directory${ESC}[0m`,
].join('\n');

const LONG_OUTPUT = Array.from({ length: 40 }, (_value, index) =>
  index === 12
    ? `${ESC}[31mline ${String(index + 1).padStart(2, '0')}: E   assertion failed${ESC}[0m`
    : `line ${String(index + 1).padStart(2, '0')}: ok`,
).join('\n');

const DIFF_CELL: DiffCellModel = {
  kind: 'diff',
  id: 'diff-1',
  createdAt: 0,
  path: 'packages/ui/src/markdown/CodeBlock.tsx',
  oldStartLine: 118,
  newStartLine: 118,
  added: 3,
  removed: 2,
  truncated: false,
  toolCallId: null,
  lines: [
    { kind: 'hunk', text: '@@ -118,7 +118,8 @@ export function CodeBlock({', oldLine: null, newLine: null },
    { kind: 'context', text: '  const rootRef = useRef<HTMLDivElement>(null);', oldLine: 118, newLine: 118 },
    {
      kind: 'context',
      text: '  const highlighting = useViewportHighlighting(rootRef, lang);',
      oldLine: 119,
      newLine: 119,
    },
    {
      kind: 'del',
      text: '  const html = useMemo(() => highlightToHtml(trimmed, lang), [trimmed, lang]);',
      oldLine: 120,
      newLine: null,
    },
    { kind: 'add', text: '  const html = useMemo(', oldLine: null, newLine: 120 },
    {
      kind: 'add',
      text: '    () => (streaming === true ? null : highlightToHtml(trimmed, lang)),',
      oldLine: null,
      newLine: 121,
    },
    { kind: 'context', text: '    [streaming, trimmed, lang],', oldLine: 121, newLine: 122 },
    { kind: 'del', text: '  );', oldLine: 122, newLine: null },
    { kind: 'add', text: '  );', oldLine: null, newLine: 123 },
    { kind: 'context', text: '', oldLine: 123, newLine: 124 },
    { kind: 'context', text: '  const [copied, setCopied] = useState(false);', oldLine: 124, newLine: 125 },
  ],
};

const WINDOWED_DIFF_CELL: DiffCellModel = {
  ...DIFF_CELL,
  id: 'diff-2',
  path: 'packages/ui/src/tool/TerminalBlock.tsx',
  added: 24,
  removed: 9,
  truncated: true,
  lines: Array.from({ length: 20 }, (_value, index) => ({
    kind: index === 5 ? ('hunk' as const) : index % 7 === 0 ? ('del' as const) : ('context' as const),
    text: `row ${index + 1} of a windowed diff payload`,
    oldLine: 40 + index,
    newLine: index === 5 ? null : 40 + index,
  })),
};

function Section({ id, title, children }: { id: string; title: string; children: React.ReactNode }) {
  return (
    // The id is a screenshot affordance: `#diff` scrolls the section into view for
    // a headless capture of one card at a time.
    <section className="section" id={id}>
      <h2>{title}</h2>
      {children}
    </section>
  );
}

function App() {
  const [dark, setDark] = useState(
    () =>
      document.documentElement.dataset.bootDark === 'true' ||
      new URLSearchParams(location.search).get('theme') === 'dark',
  );
  document.body.toggleAttribute('data-ds-dark-theme', dark);

  return (
    <div className="page">
      <header className="topbar">
        <strong>06b port preview</strong>
        <span className="hint">CodeBlock · CodeToolbar · TerminalBlock · DiffBlock</span>
        <button
          type="button"
          className="themeButton"
          onClick={() => {
            setDark((value) => !value);
          }}
        >
          {dark ? 'Light' : 'Dark'}
        </button>
      </header>

      <Section id="code" title="CodeBlock (toolbar labels, wrap, copy, line numbers)">
        <CodeBlock
          code={TYPESCRIPT_SAMPLE}
          lang="ts"
          copyLabel="Copy"
          copiedLabel="Copied"
          toolbarLabels={CODE_TOOLBAR_LABELS}
        />
        <CodeBlock
          code={PLAIN_SAMPLE}
          lang=""
          lineNumbers
          copyLabel="Copy"
          copiedLabel="Copied"
          toolbarLabels={CODE_TOOLBAR_LABELS}
        />
      </Section>

      <Section id="terminal" title="TerminalBlock (ANSI, folding, states)">
        <TerminalBlock
          command="git status"
          cwd="/Users/abiter/ws/wing-worktree/wing-app-l1"
          home="/Users/abiter"
          output={ANSI_OUTPUT}
          labels={TERMINAL_LABELS}
        />
        <TerminalBlock
          command="pnpm --filter @wing-agent/ui test "
          output={LONG_OUTPUT}
          exitCode={1}
          labels={TERMINAL_LABELS}
        />
        <TerminalBlock
          command="pnpm build --watch"
          running
          output={`${ESC}[32mready${ESC}[0m compile ok`}
          labels={TERMINAL_LABELS}
        />
      </Section>

      <Section id="diff" title="DiffBlock (host-windowed rows, counters, wrap, copy)">
        <DiffBlock cell={DIFF_CELL} labels={DIFF_LABELS} />
        <DiffBlock cell={WINDOWED_DIFF_CELL} labels={DIFF_LABELS} />
      </Section>
    </div>
  );
}

const container = document.getElementById('root');
if (container === null) {
  throw new Error('preview root missing');
}
createRoot(container).render(<App />);
