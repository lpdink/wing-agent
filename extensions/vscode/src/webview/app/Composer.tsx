/**
 * The composer: the shell's only way in.
 *
 * Shape follows VS Code's chat input (`CHAT:875-880` `.chat-input-container`:
 * `background: input-background`, `border: 1px solid input-border`,
 * `border-radius: cornerRadius-large`, `padding: 0 6px 6px 6px`; the editor gets
 * `padding-left: 4px` (`CHAT:1592-1594`); the button row is
 * `display:flex; gap:2px; margin-top:4px` (`CHAT:1661-1666`); the send button is a
 * 22×22 circle in `button-background` (`CHAT:1076-1127`)). Clicking the container
 * focuses the editor, like VS Code does with `cursor: text`.
 *
 * Behaviour:
 * - `Enter` sends; `Shift+Enter` inserts a newline (`chatInputPart.ts:2240` — "Press
 *   Enter to send out the request"); `Ctrl/Cmd+Enter` also sends, `Ctrl/Cmd+Escape`
 *   interrupts (`chatExecuteActions.ts:939-975`, the Cancel action's keybinding).
 * - While a turn is running the send button becomes **Stop** (same geometry, filled
 *   like the send button) and the toolbar shows the queued-message summary: sending
 *   meanwhile is allowed, the host queues it and marks the cell `pending`.
 * - A draft that starts with `/` opens the command candidates (data: the host's
 *   catalog merged with the frontend table). `Enter` accepts the highlighted row
 *   unless the typed text is already an exact command — then it runs (the TUI's rule
 *   for non-must-select commands, `popup/command.rs:48`). `Tab` always accepts.
 * - Submission is routed by `design.md` D6 and nothing else: one intent per command,
 *   or `sendMessage` for plain text. The composer never performs an action itself.
 */

import { useEffect, useRef, useState } from 'react';
import type { ChangeEvent, KeyboardEvent, ReactElement } from 'react';

import type { CommandInfoModel, SessionViewModel } from '../../shared';
import {
  filterCommands,
  matchCommand,
  mergeCommandCatalog,
  normalizeCommandName,
  parseSlashInput,
} from '../../shared';
import { postToHost } from '../bridge/channel';
import styles from '../styles/app.module.css';
import { optionId, useListNav } from './panels/listNav';
import { selectQueuedMessages } from './selectors';

/** Which overlay the composer asks the app to open. */
export type ComposerOverlay =
  { readonly kind: 'sessions' } | { readonly kind: 'branches'; readonly mode: 'rewind' | 'fork' };

export interface ComposerProps {
  readonly session: SessionViewModel | null;
  readonly draft: string;
  readonly onDraftChange: (text: string) => void;
  readonly onOpenOverlay: (overlay: ComposerOverlay) => void;
  /** Changes whenever the composer should take focus (mount, tab switch, host action). */
  readonly focusToken: string;
}

export function Composer({
  session,
  draft,
  onDraftChange,
  onOpenOverlay,
  focusToken,
}: ComposerProps): ReactElement {
  const textarea = useRef<HTMLTextAreaElement>(null);
  const [candidatesDismissed, setCandidatesDismissed] = useState(false);

  const catalog = session?.panels.commandCatalog?.commands ?? [];
  const merged = mergeCommandCatalog(catalog);

  // Candidates are shown while the draft is a bare slash line — a space means the
  // user moved on to arguments (and the panels take over from there).
  const trimmed = draft.trimStart();
  const typingCommand = trimmed.startsWith('/') && !/\s/.test(draft);
  const candidates = typingCommand && !candidatesDismissed ? filterCommands(merged, trimmed) : [];

  const accept = (index: number): void => {
    const candidate = candidates[index];
    if (candidate === undefined) {
      return;
    }
    onDraftChange(`${normalizeCommandName(candidate.name)} `);
    setCandidatesDismissed(true);
  };

  const nav = useListNav(
    candidates.map(() => ({ selectable: true })),
    'command-candidates',
    { onSelect: accept },
  );

  useEffect(() => {
    textarea.current?.focus();
  }, [focusToken]);

  // A different draft means a new candidate list: re-open it (Escape only silences
  // the list for as long as the user keeps typing that same text).
  useEffect(() => {
    setCandidatesDismissed(false);
  }, [draft]);

  const working = session?.status === 'working';
  const queued = session === null ? [] : selectQueuedMessages(session.cells);

  const submit = (): void => {
    if (session === null) {
      return;
    }
    const text = draft.trim();
    if (text === '') {
      return;
    }
    if (candidates.length > 0) {
      const parsed = parseSlashInput(text);
      const typed = parsed === null ? '' : normalizeCommandName(parsed.name).toLowerCase();
      // An alias counts as exact too: `/ss` and `/m` must run, not complete.
      const exact = candidates.some((candidate) =>
        [candidate.name, ...candidate.aliases].some(
          (name) => normalizeCommandName(name).toLowerCase() === typed,
        ),
      );
      if (!exact) {
        accept(nav.index);
        return;
      }
    }
    if (dispatch(text, session, onOpenOverlay)) {
      onDraftChange('');
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>): void => {
    if (session === null) {
      return;
    }
    // Ctrl/Cmd+Escape interrupts — the Cancel action's primary keybinding
    // (`chatExecuteActions.ts:966`, guarded by `hasActiveRequest` like here).
    if (event.key === 'Escape' && (event.ctrlKey || event.metaKey)) {
      if (working) {
        event.preventDefault();
        postToHost({ type: 'interrupt', sessionId: session.sessionId });
      }
      return;
    }
    if (event.key === 'Enter') {
      if (event.shiftKey) {
        return; // newline: let the browser do it
      }
      event.preventDefault();
      submit();
      return;
    }
    if (event.key === 'Tab' && candidates.length > 0) {
      event.preventDefault();
      accept(nav.index);
      return;
    }
    if ((event.key === 'ArrowDown' || event.key === 'ArrowUp') && candidates.length > 0) {
      event.preventDefault();
      nav.onKeyDown(event);
      return;
    }
    if (event.key === 'Escape') {
      if (candidates.length > 0) {
        event.preventDefault();
        setCandidatesDismissed(true);
      }
      return;
    }
    if (event.key === 'Home' || event.key === 'End') {
      if (candidates.length > 0) {
        event.preventDefault();
        nav.onKeyDown(event);
      }
    }
  };

  const disabled = session === null;
  const showSend = session === null || !working;

  return (
    <div className={styles.composerArea} data-testid="composer-area">
      {candidates.length === 0 ? null : (
        <div
          className={styles.popover}
          role="listbox"
          id="command-candidates"
          aria-label="Commands"
          data-testid="command-candidates"
          {...nav.listProps}
        >
          {candidates.map((candidate, index) => (
            <div
              key={normalizeCommandName(candidate.name)}
              className={styles.candidate}
              role="option"
              id={optionId('command-candidates', index)}
              aria-selected={index === nav.index}
              data-highlighted={index === nav.index ? 'true' : 'false'}
              data-testid="command-candidate"
              // Mouse down, not click: the textarea must not lose focus before the
              // completion lands (VS Code's suggest widget does the same).
              onMouseDown={(event) => {
                event.preventDefault();
                accept(index);
              }}
            >
              <span className={styles.candidateName}>{commandLabel(candidate)}</span>
              <span className={styles.candidateDescription}>{candidate.description}</span>
              {candidate.params === '' ? null : (
                <span className={styles.candidateParams}>{candidate.params}</span>
              )}
            </div>
          ))}
        </div>
      )}

      {queued.length === 0 ? null : (
        <div className={styles.queueBar} data-testid="queue-bar" aria-label="Queued messages">
          <span className={styles.queueLabel}>{`Queued · ${queued.length}`}</span>
          {queued.map((cell) => (
            <span key={cell.id} className={styles.queueItem} title={cell.text}>
              {firstLine(cell.text)}
            </span>
          ))}
        </div>
      )}

      <div
        className={styles.composer}
        data-testid="composer"
        data-state={session?.status ?? 'no-session'}
        onClick={() => textarea.current?.focus()}
      >
        <textarea
          ref={textarea}
          className={styles.input}
          data-testid="composer-input"
          rows={1}
          value={draft}
          disabled={disabled}
          placeholder={placeholderFor(session)}
          aria-label="Message"
          role="combobox"
          aria-expanded={candidates.length > 0}
          aria-controls={candidates.length > 0 ? 'command-candidates' : undefined}
          aria-autocomplete="list"
          onChange={(event: ChangeEvent<HTMLTextAreaElement>) => onDraftChange(event.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className={styles.composerToolbar}>
          <span className={styles.composerHint} data-testid="composer-hint">
            {hintFor(session, working)}
          </span>
          {showSend ? (
            <button
              type="button"
              className={styles.submitButton}
              data-testid="send-button"
              aria-label="Send"
              title="Send (Enter)"
              disabled={disabled || draft.trim() === ''}
              onClick={submit}
            >
              <span className={styles.sendGlyph} aria-hidden="true" />
            </button>
          ) : (
            <button
              type="button"
              className={styles.submitButton}
              data-testid="stop-button"
              aria-label="Stop"
              title="Stop the current turn (Ctrl+Escape)"
              onClick={() =>
                session !== null && postToHost({ type: 'interrupt', sessionId: session.sessionId })
              }
            >
              <span className={styles.stopGlyph} aria-hidden="true" />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/** `/name` plus aliases, as the candidate row shows it. */
function commandLabel(command: CommandInfoModel): string {
  const name = normalizeCommandName(command.name);
  return command.aliases.length === 0
    ? name
    : `${name} (${command.aliases.map(normalizeCommandName).join(', ')})`;
}

function firstLine(text: string): string {
  const [line = ''] = text.split('\n');
  return line.length > 80 ? `${line.slice(0, 79)}…` : line;
}

function placeholderFor(session: SessionViewModel | null): string {
  if (session === null) {
    return 'No session — start one with +';
  }
  if (session.status === 'waiting-for-input') {
    return 'Answer the question above to continue';
  }
  return 'Ask Wing to do something…';
}

function hintFor(session: SessionViewModel | null, working: boolean): string {
  if (session === null) {
    return '';
  }
  if (session.status === 'waiting-for-input') {
    return 'Waiting for your answer';
  }
  if (working) {
    return 'Working… Enter queues the message';
  }
  return 'Enter to send · Shift+Enter for a new line';
}

/**
 * Route one submission to exactly one intent (design.md D6).
 *
 * Returns `true` when the draft was consumed (command handled or message sent), so
 * the caller knows whether to clear it. Nothing here decides *what* a command does —
 * it picks the bridge message that the host already understands.
 */
export function dispatch(
  text: string,
  session: SessionViewModel,
  onOpenOverlay: (overlay: ComposerOverlay) => void,
): boolean {
  const parsed = parseSlashInput(text);
  if (parsed === null) {
    postToHost({ type: 'sendMessage', sessionId: session.sessionId, text });
    return true;
  }

  const command = matchCommand(parsed.name);
  if (command === null) {
    // A gateway prompt command (`/init`, …): the host resolves it.
    postToHost({
      type: 'runPromptCommand',
      sessionId: session.sessionId,
      name: normalizeCommandName(parsed.name),
      argsText: parsed.args,
    });
    return true;
  }

  const { sessionId, meta } = session;
  const bare = parsed.args === '';

  switch (command.name) {
    case '/new':
      postToHost({ type: 'newSession' });
      return true;
    case '/model':
      if (bare) {
        postToHost({ type: 'openModelPicker', sessionId });
      } else {
        forward(sessionId, command.name, parsed.args);
      }
      return true;
    case '/session':
      if (bare) {
        onOpenOverlay({ kind: 'sessions' });
      } else {
        forward(sessionId, command.name, parsed.args);
      }
      return true;
    case '/think':
      if (bare) {
        postToHost({ type: 'setThinking', sessionId, enabled: !meta.thinking });
      } else if (parsed.args === 'on') {
        postToHost({ type: 'setThinking', sessionId, enabled: true });
      } else if (parsed.args === 'off') {
        postToHost({ type: 'setThinking', sessionId, enabled: false });
      } else {
        postToHost({ type: 'setThinking', sessionId, enabled: true });
        postToHost({ type: 'setEffort', sessionId, effort: parsed.args });
      }
      return true;
    case '/yolo':
      if (bare) {
        postToHost({ type: 'setYolo', sessionId, enabled: !meta.yolo });
      } else {
        postToHost({ type: 'setYolo', sessionId, enabled: parsed.args === 'on' });
      }
      return true;
    case '/compact':
      if (bare) {
        postToHost({ type: 'compact', sessionId });
      } else {
        forward(sessionId, command.name, parsed.args);
      }
      return true;
    case '/rewind':
    case '/fork':
      if (bare) {
        onOpenOverlay({ kind: 'branches', mode: command.name === '/rewind' ? 'rewind' : 'fork' });
      } else {
        forward(sessionId, command.name, parsed.args);
      }
      return true;
    default:
      forward(sessionId, command.name, parsed.args);
      return true;
  }
}

/** `/clear`, `/copy`, `/agents`, … — the host owns them (it has the gateway client). */
function forward(sessionId: string, name: string, argsText: string): void {
  postToHost({
    type: 'runPromptCommand',
    sessionId,
    name: normalizeCommandName(name),
    argsText,
  });
}
