/**
 * The empty state of a brand-new session.
 *
 * Layout follows the chat welcome view (`chatViewWelcome.css:56-215`): an icon
 * (`40px`, `descriptionForeground`), a title (`13px`, weight 600, `margin-top: 5px`),
 * a message (`12px`, `max-width: 280px`, `padding: 0 20px`, `margin: 8px auto 0`) and
 * a row of suggestion cards (`height: 20px; padding: 0 6px; gap: 6px;
 * border-radius: 4px; background: editorWidget-background; border: 1px solid
 * chat-requestBorder`, 12px label, hover `list-hoverBackground`).
 *
 * Two deliberate differences from Copilot:
 * - the suggestions **fill the composer instead of sending**: the shell never presses
 *   send on the user's behalf;
 * - the icon comes from the host's `assetUris` when available (the webview has no
 *   codicon font, and `wing.svg` is the extension's own asset) and degrades to a text
 *   wordmark in the preview harness.
 */

import type { ReactElement } from 'react';

import { readBootstrap } from '../bootstrap';
import styles from '../styles/app.module.css';

/** Neutral, side-effect-free starting points (they only fill the input). */
export const SUGGESTIONS: readonly string[] = [
  'Explain how this project is organised',
  'Find and fix the failing tests',
  'Summarise what changed in the last commit',
];

export interface WelcomeProps {
  readonly onSuggest: (text: string) => void;
}

/**
 * The host's asset map, read once per document.
 *
 * `readBootstrap()` warns when nothing was injected (the preview harness never
 * injects anything), so it must not run on every render.
 */
let cachedAssetUris: Readonly<Record<string, string>> | null = null;

function iconUri(): string | undefined {
  cachedAssetUris ??= readBootstrap().assetUris;
  return cachedAssetUris['wing.svg'];
}

export function Welcome({ onSuggest }: WelcomeProps): ReactElement {
  const icon = iconUri();

  return (
    <div className={styles.welcome} data-testid="welcome">
      {icon === undefined ? (
        <div className={styles.welcomeWordmark} aria-hidden="true">
          Wing
        </div>
      ) : (
        <img className={styles.welcomeIcon} src={icon} alt="" aria-hidden="true" />
      )}
      <h2 className={styles.welcomeTitle}>Wing agent</h2>
      <p className={styles.welcomeMessage}>
        Ask a question or describe a change. Wing works in this workspace with its own tools and keeps the
        transcript here.
      </p>
      <div className={styles.welcomeSuggestions} data-testid="welcome-suggestions">
        {SUGGESTIONS.map((text) => (
          <button
            key={text}
            type="button"
            className={styles.welcomeSuggestion}
            data-testid="welcome-suggestion"
            onClick={() => onSuggest(text)}
          >
            {text}
          </button>
        ))}
      </div>
      <p className={styles.welcomeHint}>Enter to send · Shift+Enter for a new line · / for commands</p>
    </div>
  );
}
