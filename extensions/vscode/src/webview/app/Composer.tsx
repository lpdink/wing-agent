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

import type { CommandInfoModel, FrontendCommand, SessionViewModel } from '../../shared';
import {
  filterCommands,
  isEffortLevel,
  matchCommand,
  mergeCommandCatalog,
  normalizeCommandName,
  parseBoolArg,
  parseSlashInput,
} from '../../shared';
import { postToHost } from '../bridge/channel';
import styles from '../styles/app.module.css';
import { optionId, useListNav } from './panels/listNav';
import { selectQueuedMessages } from './selectors';

export interface ComposerProps {
  readonly session: SessionViewModel | null;
  readonly draft: string;
  readonly onDraftChange: (text: string) => void;
  /** Changes whenever the composer should take focus (mount, tab switch, host action). */
  readonly focusToken: string;
  /**
   * True while the host has an overlay on screen.
   *
   * The composer must not steal the focus from an open panel — and, just as
   * importantly, taking it back when the panel closes is *its* job: the host closes
   * overlays without sending `focusComposer`, so this transition is the only signal
   * the input gets (otherwise the focus dies with the unmounted panel).
   */
  readonly overlayOpen: boolean;
}

export function Composer({
  session,
  draft,
  onDraftChange,
  focusToken,
  overlayOpen,
}: ComposerProps): ReactElement {
  const textarea = useRef<HTMLTextAreaElement>(null);
  const [candidatesDismissed, setCandidatesDismissed] = useState(false);
  /** Feedback for a submission the composer refused (e.g. a bad `/think` argument). */
  const [rejection, setRejection] = useState<string | null>(null);

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
    if (!overlayOpen) {
      textarea.current?.focus();
    }
  }, [focusToken, overlayOpen]);

  // A different draft means a new candidate list and no stale rejection message:
  // re-open the list (Escape only silences it for as long as the user keeps typing
  // that same text).
  useEffect(() => {
    setCandidatesDismissed(false);
    setRejection(null);
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
    const outcome = dispatch(text, session);
    if (outcome.kind === 'sent') {
      setRejection(null);
      onDraftChange('');
    } else {
      // Nothing left the composer: keep the text so the user can fix it, and say why.
      setRejection(outcome.hint);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>): void => {
    if (session === null) {
      return;
    }
    // Input method editors: the Enter that commits a composition must never send the
    // half-finished candidate list (`isComposing`; Chromium also reports 229). VS Code
    // gates its own keybindings on the same flag (`keybindingService.ts:282-290`).
    if (event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229) {
      return;
    }
    // Ctrl/Cmd+Escape interrupts — the Cancel action's primary keybinding
    // (CHATEXE:968-976, the chord itself at :970; VS Code guards it with
    // `hasActiveRequest`, we guard on `working`).
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
        return;
      }
      // Nothing local to close: ask the host (it owns every overlay). Harmless when
      // none is open — the host clears pickers that are already `null`.
      event.preventDefault();
      postToHost({ type: 'closeOverlays' });
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
        <div className={styles.queueBar} data-testid="queue-bar" role="status" aria-label="Queued messages">
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
          // The focus stays in the textarea, so the highlighted candidate is announced
          // from the combobox (the listbox carries its own copy for pointer users).
          {...nav.listProps}
          onChange={(event: ChangeEvent<HTMLTextAreaElement>) => onDraftChange(event.target.value)}
          onKeyDown={onKeyDown}
        />
        <div className={styles.composerToolbar}>
          <span
            className={styles.composerHint}
            data-testid="composer-hint"
            data-rejected={rejection === null ? 'false' : 'true'}
          >
            {rejection ?? hintFor(session, working)}
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
 * Outcome of a submission: either something left the composer, or it was refused
 * with a hint the user can act on (never a silent drop).
 */
export type DispatchOutcome = { readonly kind: 'sent' } | { readonly kind: 'invalid'; readonly hint: string };

const USAGE: Record<string, string> = {
  '/think': 'Usage: /think [on|off|low|medium|high|xhigh|max]',
  '/yolo': 'Usage: /yolo [on|off]',
};

/**
 * Route one submission to exactly one intent (design.md D6, as frozen by
 * `interfaces.md`).
 *
 * The panel commands are *not* handled here: bare `/ss`, `/rewind` and `/fork` are
 * forwarded as commands, and the host answers by opening `sessionPicker` /
 * `branchPicker`. Nothing in the webview opens an overlay.
 *
 * Commands whose arguments are a closed vocabulary (`/think`, `/yolo`) are validated
 * before anything is sent — an illegal value must never reach the gateway, and the
 * user gets a usage line back instead (the TUI behaves the same way,
 * `crates/wing/src/app/commands.rs:594-652`).
 */
export function dispatch(text: string, session: SessionViewModel): DispatchOutcome {
  const parsed = parseSlashInput(text);
  // A lone `/` is not a command (the candidate list owns that state and may have been
  // dismissed): it goes to the model as text, exactly like the TUI does.
  if (parsed === null || normalizeCommandName(parsed.name) === '/') {
    postToHost({ type: 'sendMessage', sessionId: session.sessionId, text });
    return SENT;
  }

  const command = matchCommand(parsed.name);
  if (command === null) {
    // A gateway prompt command (`/init`, …): the host resolves it. The leading slash
    // is part of the wire name (`interfaces.md`).
    return forward(session, normalizeCommandName(parsed.name), parsed.args);
  }

  // Everything the host owns goes out unchanged: one place, no per-command knowledge.
  if (isFrontend(command) && command.kind === 'forward') {
    return forward(session, command.name, parsed.args);
  }

  const { sessionId, meta } = session;
  const bare = parsed.args === '';

  switch (command.name) {
    case '/new':
      postToHost({ type: 'newSession' });
      return SENT;
    case '/model':
      if (bare) {
        postToHost({ type: 'openModelPicker', sessionId });
      } else {
        return forward(session, command.name, parsed.args);
      }
      return SENT;
    case '/session':
    case '/rewind':
    case '/fork':
      // Bare or with an id/uuid: the host opens the picker or executes the command.
      // The **typed** spelling travels (`/ss` stays `/ss`, `interfaces.md`), because
      // the host's table knows the aliases the same way the TUI's does.
      return forward(session, parsed.name, parsed.args);
    case '/think': {
      const arg = parseBoolArg(parsed.args);
      switch (arg.kind) {
        case 'empty':
          postToHost({ type: 'setThinking', sessionId, enabled: !meta.thinking });
          return SENT;
        case 'on':
          postToHost({ type: 'setThinking', sessionId, enabled: true });
          return SENT;
        case 'off':
          postToHost({ type: 'setThinking', sessionId, enabled: false });
          return SENT;
        case 'other':
          if (isEffortLevel(arg.value)) {
            postToHost({ type: 'setThinking', sessionId, enabled: true });
            postToHost({ type: 'setEffort', sessionId, effort: arg.value });
            return SENT;
          }
          return { kind: 'invalid', hint: USAGE['/think'] ?? '' };
        default:
          return SENT;
      }
    }
    case '/yolo': {
      const arg = parseBoolArg(parsed.args);
      switch (arg.kind) {
        case 'empty':
          postToHost({ type: 'setYolo', sessionId, enabled: !meta.yolo });
          return SENT;
        case 'on':
          postToHost({ type: 'setYolo', sessionId, enabled: true });
          return SENT;
        case 'off':
          postToHost({ type: 'setYolo', sessionId, enabled: false });
          return SENT;
        default:
          return { kind: 'invalid', hint: USAGE['/yolo'] ?? '' };
      }
    }
    case '/compact':
      if (bare) {
        postToHost({ type: 'compact', sessionId });
      } else {
        // The `compact` intent carries no instruction; the command form does.
        return forward(session, command.name, parsed.args);
      }
      return SENT;
    default:
      return forward(session, command.name, parsed.args);
  }
}

const SENT: DispatchOutcome = { kind: 'sent' };

/** True when the matched entry really is one of our own table's rows. */
function isFrontend(command: CommandInfoModel): command is FrontendCommand {
  return 'kind' in command;
}

/** `/clear`, `/copy`, `/agents`, `/ss`, `/rewind`, … — the host owns them. */
function forward(session: SessionViewModel, name: string, argsText: string): DispatchOutcome {
  postToHost({
    type: 'runPromptCommand',
    sessionId: session.sessionId,
    name: normalizeCommandName(name),
    argsText,
  });
  return SENT;
}
