/**
 * Copying to the clipboard, including the path a plain-http deployment needs.
 *
 * The renderer's "Copy" buttons post `copyText` and show their own confirmation
 * (`chat/interaction.ts` — a fire-and-forget feedback, 1200 ms). The host side must
 * therefore be honest about failures, and it must have an answer for the case this
 * app actually ships in: a gateway-hosted SPA reached over `http://` on a LAN is
 * **not a secure context**, so `navigator.clipboard` is simply `undefined` there, and
 * the obvious `navigator.clipboard?.writeText(...)` would do nothing at all —
 * silently, while the button says "Copied".
 *
 * Two paths, in order:
 *
 * 1. `navigator.clipboard.writeText` — the platform API. Also rejected by permission
 *    policies, which is why a rejection falls through instead of failing outright.
 * 2. A throwaway `<textarea>` + `document.execCommand('copy')` — deprecated, but the
 *    only mechanism a non-secure context has, and the same one every web client used
 *    before the async clipboard API existed.
 *
 * The caller gets the outcome (never a silent boolean), so it can tell the user when
 * *both* failed. The DOM is injected, so the decision logic is testable without a
 * browser.
 */

import type { CoreLogger } from '@wing-agent/client';

export type ClipboardOutcome =
  /** The async clipboard API did it. */
  | 'clipboard-api'
  /** The `execCommand` fallback did it (no secure context / API refused). */
  | 'exec-command'
  /** Neither path could copy — the caller should say so. */
  | 'failed';

/** What the fallback needs from a document (jsdom has no `execCommand`; tests fake it). */
export interface ClipboardDocument {
  execCommand?: ((command: string) => boolean) | undefined;
  createElement(tagName: 'textarea'): {
    value: string;
    style: Record<string, string>;
    setAttribute(name: string, value: string): void;
    select(): void;
    setSelectionRange?(start: number, end: number): void;
  };
  body: { appendChild(node: never): void; removeChild(node: never): void } | null;
}

export interface ClipboardDeps {
  /** `navigator.clipboard`, or `null` where the context is not secure. */
  readonly clipboard?: { writeText(text: string): Promise<void> } | null;
  readonly document?: ClipboardDocument | null;
}

export async function copyTextToClipboard(
  text: string,
  deps: ClipboardDeps,
  logger?: CoreLogger,
): Promise<ClipboardOutcome> {
  const clipboard = deps.clipboard ?? null;
  if (clipboard !== null) {
    try {
      await clipboard.writeText(text);
      return 'clipboard-api';
    } catch (error) {
      // Not a failure yet: a denied permission is exactly when the fallback helps.
      logger?.debug('clipboard API refused the write; trying the textarea fallback', error);
    }
  }
  if (deps.document !== undefined && deps.document !== null && copyViaExecCommand(deps.document, text)) {
    return 'exec-command';
  }
  return 'failed';
}

/** The `<textarea>` + `execCommand` path; `false` when the document cannot do it. */
function copyViaExecCommand(document: ClipboardDocument, text: string): boolean {
  const exec = document.execCommand;
  if (typeof exec !== 'function' || document.body === null) {
    return false;
  }
  const field = document.createElement('textarea');
  field.value = text;
  // Off-screen but still focusable: `select()` needs the element in the document, and
  // the copy must not scroll the transcript underneath the user.
  field.setAttribute('readonly', '');
  field.style['position'] = 'fixed';
  field.style['top'] = '-1000px';
  field.style['left'] = '-1000px';
  document.body.appendChild(field as never);
  try {
    field.select();
    return exec.call(document, 'copy') === true;
  } catch (error) {
    void error;
    return false;
  } finally {
    document.body.removeChild(field as never);
  }
}
