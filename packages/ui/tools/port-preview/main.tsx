// Preview harness for the ported cards (dev-only; not part of any product build).
//
// Renders batch 06b (`CodeBlock` / `CodeToolbar`, `TerminalBlock`, `DiffBlock`) and
// batch 06c (the process rows, the ask trio, the connection indicator, the atoms) —
// plus a little chrome so light/dark can both be screenshotted — against fake data.
//
// The 06b cards still import their sources by path (they live inside the package and
// are not a consumer contract); the 06c components come from the **barrel**
// (`../../src/index`) on purpose: that is how a real shell consumes them, so this
// page's build is itself evidence that the barrel carries the token sheet (the
// 06c root cause: a component-only consumer used to lose every `--wing-*` in a
// production build). Nothing here imports `styles/tokens.css` directly — the barrel
// provides it, exactly like it does for `apps/web`.

import { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';

import type { AskAnswerModel, AskQuestionModel, DiffCellModel } from '@wing-agent/session';

import { CodeBlock } from '../../src/markdown/CodeBlock';
import { DiffBlock } from '../../src/tool/DiffBlock';
import { TerminalBlock } from '../../src/tool/TerminalBlock';
import type { DiffBlockLabels } from '../../src/tool/DiffBlock';
import type { TerminalBlockLabels } from '../../src/tool/TerminalBlock';
import {
  ApprovalPanel,
  ConnectionIndicator,
  DisclosureRow,
  QuestionComposer,
  QuestionReplyView,
  ReasoningRow,
  StateDot,
  TextShimmer,
} from '../../src/index';

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

// A deliberately long line so the wrap toggle has something to do: it wraps onto a
// second line while wrapping is on, and scrolls sideways (clipped at the card edge)
// once it is off — the two states the screenshots pin.
export const LONG_LINE = 'the quick brown fox jumps over the lazy dog, twice, because once was not enough for a demo; then it jumps again, and again, until the card has to decide whether to wrap or to scroll';`;

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

const ASK_QUESTIONS: readonly AskQuestionModel[] = [
  {
    id: 'database',
    header: 'Database',
    question: 'Which database should the service use in production?',
    multiSelect: false,
    required: false,
    options: [
      { label: 'Postgres (recommended)', description: 'Managed by the platform team' },
      { label: 'SQLite', description: 'Single file, no server process' },
      { label: 'DynamoDB', description: 'Serverless, per-request pricing' },
    ],
  },
  {
    id: 'extras',
    header: 'Extras',
    question: 'Which of these should I wire up as well?',
    multiSelect: true,
    required: false,
    options: [
      { label: 'Metrics', description: 'Prometheus counters' },
      { label: 'Tracing', description: 'OpenTelemetry spans' },
    ],
  },
];

const ASK_ANSWERS: readonly AskAnswerModel[] = [
  { questionId: 'database', selected: ['Postgres (recommended)'], text: '' },
  { questionId: 'extras', selected: ['Metrics'], text: 'and the dashboards' },
];

const REASONING_TEXT = `The user is asking for a preview page for the ported rows. I should
render the same components the transcript will use, with the same token tables.

Settled reasoning keeps only its first line beside the title, so this paragraph is
what a reader sees without expanding the row.`;

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

/** `?section=<id>` renders one section only, so a capture is deterministic. */
const ONLY_SECTION = new URLSearchParams(location.search).get('section');

/**
 * How many fences the rendered sections are expected to settle *highlighted*.
 *
 * Fixture knowledge, deliberately explicit: the code section's first card is
 * `lang="ts"` (the highlighter ships that grammar) while the second is a
 * `lang=""` plain-text card, and no other section renders a fence.
 */
const HIGHLIGHTED_FENCES = ONLY_SECTION === null || ONLY_SECTION === 'code' ? 1 : 0;

/**
 * Publish the readiness signal the screenshot driver waits for.
 *
 * A card highlights when it first intersects the viewport
 * (`useViewportHighlighting`: observer callback → shiki build → re-render), which
 * lands a frame or two after mount. A capture that fires before that frame shows
 * the *plain* arm — the race the 06b review caught in the delivered shots — so the
 * driver polls `document.documentElement.dataset.previewReady` instead of guessing
 * a delay. Harness chrome; nothing in the package depends on it.
 */
function useScreenshotReadiness(): void {
  useEffect(() => {
    const ready = (): boolean => document.querySelectorAll('pre.shiki').length >= HIGHLIGHTED_FENCES;
    if (ready()) {
      document.documentElement.dataset.previewReady = 'true';
      return;
    }
    const observer = new MutationObserver(() => {
      if (ready()) {
        document.documentElement.dataset.previewReady = 'true';
        observer.disconnect();
      }
    });
    observer.observe(document.body, { childList: true, subtree: true });
    return () => {
      observer.disconnect();
    };
  }, []);
}

function Section({ id, title, children }: { id: string; title: string; children: React.ReactNode }) {
  if (ONLY_SECTION !== null && ONLY_SECTION !== id) return null;
  return (
    // The id is a screenshot/documentation affordance: `?section=diff` narrows the
    // page to one card so a headless capture of it is reproducible.
    <section className="section" id={id}>
      <h2>{title}</h2>
      {children}
    </section>
  );
}

function App() {
  useScreenshotReadiness();

  // Screenshot affordance: the second reply bubble is opened once so one capture
  // shows both its collapsed and expanded shapes. Harness chrome only — nothing in
  // the package does this.
  useEffect(() => {
    document.querySelector('.expandTarget button')?.dispatchEvent(new MouseEvent('click', { bubbles: true }));
  }, []);

  const [dark, setDark] = useState(
    () =>
      document.documentElement.dataset.bootDark === 'true' ||
      new URLSearchParams(location.search).get('theme') === 'dark',
  );
  document.body.toggleAttribute('data-ds-dark-theme', dark);

  return (
    <div className="page">
      <header className="topbar">
        <strong>port preview</strong>
        <span className="hint">06b cards · 06c rows · ask · connection</span>
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

      <Section id="rows" title="Process rows (DisclosureRow · TextShimmer · ReasoningRow)">
        <div className="rowStack">
          <DisclosureRow
            icon={<StateDot state="ongoing" />}
            title="Bash"
            open={false}
            expandable
            expandOnRowClick={false}
            running
            onToggle={() => {}}
          />
          <DisclosureRow
            icon={<StateDot state="done" />}
            title="Read src/ui/chat_view/cell.rs"
            open={false}
            expandable
            expandOnRowClick={false}
            onToggle={() => {}}
          />
          <DisclosureRow
            icon={<StateDot state="warning" />}
            title="Write outside the workspace"
            open
            expandable
            expandOnRowClick
            onToggle={() => {}}
          >
            <div className="rowBody">
              The sticky header stays reachable while an uncapped body scrolls with the conversation.
            </div>
          </DisclosureRow>
          <p className="shimmerLine">
            <TextShimmer>Idle text</TextShimmer>
            {' · '}
            <TextShimmer active>Running text</TextShimmer>
          </p>
        </div>
        <ReasoningRow text={REASONING_TEXT} streaming collapseKey="preview-streaming" />
        <ReasoningRow
          text={REASONING_TEXT}
          streaming={false}
          durationMs={4200}
          collapseKey="preview-settled"
        />
        <ReasoningRow text={REASONING_TEXT} streaming={false} durationMs={null} previewEnabled={false} />
      </Section>

      <Section id="ask" title="Ask (ApprovalPanel · QuestionComposer · QuestionReplyView)">
        <ApprovalPanel
          requestId="toolu_preview"
          toolName="Bash"
          detail={<code>rm -rf build/ &amp;&amp; pnpm --filter @wing-agent/ui build</code>}
          state="awaiting"
          onDecide={() => {}}
        />
        <ApprovalPanel requestId="toolu_settled" toolName="Bash" state="answered" onDecide={() => {}} />
        <QuestionComposer
          requestId="ask-preview"
          questions={ASK_QUESTIONS}
          state="awaiting"
          onSubmit={() => {}}
        />
        <QuestionReplyView questions={ASK_QUESTIONS} answers={ASK_ANSWERS} time={0} />
        <div className="expandTarget">
          <QuestionReplyView questions={ASK_QUESTIONS} answers={ASK_ANSWERS} time={0} />
        </div>
      </Section>

      <Section id="connection" title="ConnectionIndicator (connecting · disconnected · recovered)">
        <div className="rowStack rowStackInline">
          <ConnectionIndicator
            state="connecting"
            disconnectedLabel="Disconnected — click to retry"
            connectingLabel="Reconnecting"
            recoveredLabel="Reconnected"
            reconnectActionLabel="Reconnect now"
            restartActionLabel="Restart the attempt"
            onReconnect={() => {}}
          />
          <ConnectionIndicator
            state="disconnected"
            disconnectedLabel="Disconnected — click to retry"
            connectingLabel="Reconnecting"
            recoveredLabel="Reconnected"
            reconnectActionLabel="Reconnect now"
            restartActionLabel="Restart the attempt"
            onReconnect={() => {}}
          />
          <ConnectionIndicator
            state="recovered"
            disconnectedLabel="Disconnected — click to retry"
            connectingLabel="Reconnecting"
            recoveredLabel="Reconnected"
            reconnectActionLabel="Reconnect now"
            restartActionLabel="Restart the attempt"
            onReconnect={() => {}}
          />
        </div>
      </Section>
    </div>
  );
}

const container = document.getElementById('root');
if (container === null) {
  throw new Error('preview root missing');
}
createRoot(container).render(<App />);
