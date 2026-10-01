/**
 * The composer — text input, send, interrupt and in-flight state.
 *
 * Three visual states:
 *
 * - **idle**: textarea + Send button (or YOLO-Send when yolo is on)
 * - **draft**: textarea pre-filled with the restored draft + Send
 * - **sending**: textarea disabled + Interrupt button + "running" indicator
 *
 * In-flight queuing (vscode parity — no local queue): when turn is active the
 * textarea is disabled and the user sees only Interrupt. Text cannot be typed
 * while the agent is responding; when sending fails the text returns to the
 * textarea via `setDraft`.
 *
 * IME protection: Enter only submits when no IME composition is in progress.
 * Shift+Enter always inserts a newline.
 *
 * Draft persistence: restored from `record.draft` (set by the runtime after a
 * failed send); also backed to `sessionStorage` by sessionId for page refresh
 * resilience.
 *
 * Mobile keyboard: on phones the composer is `position: fixed` at the bottom
 * of the viewport. When the soft keyboard opens, `visualViewport.resize` fires
 * and the composer's `bottom` is adjusted by the keyboard height so it stays
 * visible above the keyboard.
 */

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ChangeEvent,
  type KeyboardEvent,
  type ReactElement,
} from 'react';

import type { SessionRecord } from '@wing-agent/session';
import {
  FRONTEND_COMMANDS,
  filterCommands,
  matchCommand,
  normalizeCommandName,
  parseBoolArg,
  parseSlashInput,
} from '@wing-agent/session';

import type { ShellActions } from './App';
import { useVisualViewportOffset } from './useKeyboardOffset';

export interface ComposerProps {
  readonly record: SessionRecord;
  readonly actions: ShellActions;
}

/** The composer's own state (not from the record). */
interface ComposerState {
  readonly text: string;
  readonly composing: boolean;
}

export function Composer({ record, actions }: ComposerProps): ReactElement {
  const [state, setState] = useState<ComposerState>(() => ({
    text: record.draft ?? '',
    composing: false,
  }));
  const [showCommands, setShowCommands] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const composerRef = useRef<HTMLDivElement>(null);
  const noTranscriptRef = useRef<HTMLDivElement | null>(null); // unused on web (the transcript scrolls naturally)
  /** Guard against rapid double-send (review r1 S2). */
  const sendingRef = useRef(false);
  const turnActive = record.turn.active;
  const yolo = record.meta.yolo;

  // On mobile: keep the composer above the soft keyboard.
  useVisualViewportOffset(composerRef, noTranscriptRef);

  // Restore draft when it changes (a failed send returns text to the composer).
  useEffect(() => {
    setState((prev) => {
      if (record.draft !== null && record.draft !== prev.text) {
        return { ...prev, text: record.draft };
      }
      return prev;
    });
  }, [record.draft, record.draftSeq]);

  // Persist draft to sessionStorage on text change.
  useEffect(() => {
    try {
      sessionStorage.setItem(`composer:draft:${record.sessionId}`, state.text);
    } catch {
      // Ignore storage errors (private browsing, full disk).
    }
  }, [state.text, record.sessionId]);

  // Restore draft from sessionStorage on mount only.
  const draftRestoredRef = useRef(false);
  useEffect(() => {
    if (draftRestoredRef.current) {
      return;
    }
    draftRestoredRef.current = true;
    try {
      const saved = sessionStorage.getItem(`composer:draft:${record.sessionId}`);
      if (saved !== null && saved.length > 0 && record.draft === null) {
        setState((prev) => ({ ...prev, text: saved }));
      }
    } catch {
      // Ignore storage errors (private browsing, full disk).
    }
  }, [record.sessionId]);

  // Focus the textarea when a new session opens or turn becomes idle.
  useEffect(() => {
    if (!turnActive) {
      textareaRef.current?.focus();
    }
  }, [turnActive, record.sessionId]);

  const handleInput = useCallback(
    (event: ChangeEvent<HTMLTextAreaElement>) => {
      const value = event.target.value;
      setState((prev) => ({ ...prev, text: value }));
      // Show slug command popup when the user types `/`.
      if (value === '/') {
        setShowCommands(true);
      } else if (showCommands && !value.startsWith('/')) {
        setShowCommands(false);
      }
    },
    [showCommands],
  );

  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLTextAreaElement>) => {
      // IME protection: Enter only submits when composition is finished.
      if (event.key === 'Enter' && !event.shiftKey && !state.composing) {
        event.preventDefault();
        submit();
      }
    },
    [state.composing, state.text],
  );

  const submit = useCallback(() => {
    const text = state.text.trim();
    if (text === '' || turnActive || sendingRef.current) {
      return;
    }

    // Check for slash command routing.
    if (text.startsWith('/')) {
      const parsed = parseSlashInput(text);
      if (parsed !== null) {
        const command = matchCommand(parsed.name);
        if (command !== null) {
          routeCommand(parsed.name, parsed.args, actions, {
            thinking: record.meta.thinking,
            yolo: record.meta.yolo,
          });
          setState((prev) => ({ ...prev, text: '' }));
          setShowCommands(false);
          return;
        }
      }
    }

    sendingRef.current = true;
    try {
      actions.sendText(text);
    } finally {
      sendingRef.current = false;
    }
    setState((prev) => ({ ...prev, text: '' }));
    setShowCommands(false);
  }, [state.text, turnActive, actions, record.meta.thinking, record.meta.yolo]);

  const handleCommandSelect = useCallback(
    (name: string) => {
      routeCommand(name, '', actions, {
        thinking: record.meta.thinking,
        yolo: record.meta.yolo,
      });
      setState((prev) => ({ ...prev, text: '' }));
      setShowCommands(false);
    },
    [actions, record.meta.thinking, record.meta.yolo],
  );

  const handleKeyDownCommand = useCallback((event: KeyboardEvent<HTMLDivElement>) => {
    if (event.key === 'Escape') {
      setShowCommands(false);
      textareaRef.current?.focus();
    }
  }, []);

  const commandFilter = state.text.startsWith('/') ? state.text.slice(1).trimStart() : '';
  const matchedCommands = showCommands ? filterCommands(FRONTEND_COMMANDS, commandFilter) : [];

  return (
    <div ref={composerRef} className="composer">
      {showCommands && matchedCommands.length > 0 ? (
        <div
          className="composer__commands"
          role="listbox"
          aria-label="Commands"
          onKeyDown={handleKeyDownCommand}
        >
          {matchedCommands.map((cmd) => (
            <button
              key={cmd.name}
              type="button"
              className="composer__command"
              role="option"
              aria-selected={false}
              onClick={() => {
                handleCommandSelect(cmd.name);
              }}
            >
              <span className="composer__command-name">{cmd.name}</span>
              <span className="composer__command-desc">{cmd.description}</span>
              {cmd.params ? <span className="composer__command-params">{cmd.params}</span> : null}
            </button>
          ))}
        </div>
      ) : null}

      <div className="composer__bar">
        <textarea
          ref={textareaRef}
          className="composer__input"
          value={state.text}
          onChange={handleInput}
          onKeyDown={handleKeyDown}
          onCompositionStart={() => {
            setState((prev) => ({ ...prev, composing: true }));
          }}
          onCompositionEnd={() => {
            setState((prev) => ({ ...prev, composing: false }));
          }}
          disabled={turnActive}
          placeholder={turnActive ? 'Agent is responding…' : yolo ? 'Message (YOLO mode)…' : 'Message…'}
          rows={Math.min(Math.max(state.text.split('\n').length, 1), 8)}
          aria-label="Message composer"
        />

        <div className="composer__actions">
          {turnActive ? (
            <button
              type="button"
              className="button button--danger composer__interrupt"
              onClick={() => {
                void actions.interrupt();
              }}
              title="Interrupt the running turn"
            >
              Interrupt
            </button>
          ) : (
            <button
              type="button"
              className="button button--primary composer__send"
              disabled={state.text.trim() === ''}
              onClick={submit}
              title={yolo ? 'Send (YOLO mode)' : 'Send'}
            >
              Send
            </button>
          )}
        </div>
      </div>

      {turnActive ? <div className="composer__running">Agent is responding…</div> : null}
    </div>
  );
}

/**
 * Route a command to its action.
 *
 * Mirrors `extensions/vscode/src/host/session/manager.ts::runPromptCommand`.
 *
 * `currentMeta` is required for toggle commands (`/think`, `/yolo`) that need
 * the current session state to decide the next value.
 */
function routeCommand(
  name: string,
  args: string,
  actions: ShellActions,
  currentMeta?: { readonly thinking: boolean; readonly yolo: boolean },
): void {
  const command = normalizeCommandName(name);
  switch (command) {
    case '/new':
      actions.newSession();
      return;
    case '/ss':
    case '/session':
      if (args === '') {
        void actions.openBranchesPanel('fork');
      } else {
        actions.activate(args);
      }
      return;
    case '/model':
    case '/m':
      void actions.openModelPicker();
      return;
    case '/think':
    case '/t': {
      handleThinkCommand(args, actions, currentMeta?.thinking ?? false);
      return;
    }
    case '/yolo': {
      handleYoloCommand(args, actions, currentMeta?.yolo ?? false);
      return;
    }
    case '/compact':
      void actions.compact(args === '' ? null : args);
      return;
    case '/fork':
      if (args === '') {
        void actions.openBranchesPanel('fork');
      } else {
        void actions.fork('', args);
      }
      return;
    case '/rewind':
      if (args === '') {
        void actions.openBranchesPanel('rewind');
      } else {
        void actions.rewind(args);
      }
      return;
    case '/context':
    case '/skills':
    case '/reload':
    case '/copy':
    case '/title':
    case '/workdir':
    case '/agents':
    case '/clear':
      // These are forwarded commands — handled locally in vscode; on web we
      // send them as messages for the gateway to process.
      actions.sendText(`${command}${args === '' ? '' : ` ${args}`}`);
      return;
    default:
      // Unknown command: send as message.
      actions.sendText(`${command}${args === '' ? '' : ` ${args}`}`);
      return;
  }
}

/**
 * Handle `/think` command — toggle or set thinking mode.
 *
 * Semantics (vscode parity):
 * - No arg → toggle (current on → off, current off → on)
 * - `on` → enable thinking
 * - `off` → disable thinking
 * - effort level (low/medium/high/xhigh/max) → enable with that effort
 */
function handleThinkCommand(args: string, actions: ShellActions, currentThinking: boolean): void {
  const boolArg = parseBoolArg(args);
  switch (boolArg.kind) {
    case 'empty':
      void actions.updateMeta({ thinking: !currentThinking });
      return;
    case 'on':
      void actions.updateMeta({ thinking: true });
      return;
    case 'off':
      void actions.updateMeta({ thinking: false });
      return;
    case 'other':
      if (['low', 'medium', 'high', 'xhigh', 'max'].includes(boolArg.value)) {
        void actions.updateMeta({ thinking: true, reasoning_effort: boolArg.value });
      }
      return;
  }
}

/**
 * Handle `/yolo` command — toggle or set YOLO mode.
 *
 * Semantics (vscode parity):
 * - No arg → toggle (current on → off, current off → on)
 * - `on` → enable YOLO
 * - `off` → disable YOLO
 */
function handleYoloCommand(args: string, actions: ShellActions, currentYolo: boolean): void {
  const boolArg = parseBoolArg(args);
  switch (boolArg.kind) {
    case 'empty':
      void actions.updateMeta({ yolo: !currentYolo });
      return;
    case 'on':
      void actions.updateMeta({ yolo: true });
      return;
    case 'off':
      void actions.updateMeta({ yolo: false });
      return;
    case 'other':
      return;
  }
}
