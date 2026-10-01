/**
 * Vocabulary the session model and every frontend must agree on.
 *
 * Only environment-agnostic constants live here: values that describe the gateway's
 * session semantics or the shell's own command table. The host ⇄ view bridge's own
 * numbers (the protocol version, the patch chunk cap) and the VS Code webview's DOM
 * vocabulary belong to the extension — see `extensions/vscode/src/shared/constants.ts`.
 */

/**
 * Tool names — must match the backend tool registry.
 * Source of truth: `crates/wing/src/shared/constants.rs`, `libs/core/wing/tools/`.
 */
export const TOOL_NAMES = {
  bash: 'Bash',
  read: 'Read',
  write: 'Write',
  edit: 'Edit',
  glob: 'Glob',
  grep: 'Grep',
  askUserQuestion: 'AskUserQuestion',
  todoWrite: 'TodoWrite',
} as const;

export type ToolName = (typeof TOOL_NAMES)[keyof typeof TOOL_NAMES];

/**
 * Frontend-only commands: handled by the host without reaching the gateway.
 * Mirrors `crates/wing/src/shared/constants.rs`.
 *
 * Kept as the TUI's mirror; the shell's vocabulary (which includes these three) is
 * {@link FRONTEND_COMMANDS} in `./commands`, and only the contract test reads this
 * constant today.
 */
export const LOCAL_COMMANDS = {
  new: '/new',
  clear: '/clear',
  copy: '/copy',
} as const;

/**
 * Session title derivation limit (characters) — must match the backend rule
 * (`first_user_message` truncation, see `libs/core/wing/session.py`). The host
 * derives titles with the same rule so both sides agree without an extra RPC.
 */
export const SESSION_TITLE_MAX_LENGTH = 100;
