/**
 * Slash commands — the frontend's own vocabulary, shared by both sides.
 *
 * wing has two command sources (see the TUI's `app/commands.rs`):
 *
 * 1. **gateway prompt commands** (`GET /api/commands`, e.g. `/init`) — expanded by
 *    the backend and sent to the model;
 * 2. **frontend commands** — intercepted before they reach the model. The TUI keeps
 *    its table in `crates/wing/src/ui/popup/command.rs:59` (`TUI_ONLY_COMMANDS`) and
 *    routes it in `crates/wing/src/app/commands.rs:88` (`COMMANDS`).
 *
 * {@link FRONTEND_COMMANDS} mirrors source 2 so that both sides of the bridge agree
 * on what "this input is a command" means: the webview uses it for the `/`
 * candidates and to decide between `sendMessage` and `runPromptCommand`, the host
 * uses it to route what arrives.
 *
 * Everything here is pure — no DOM, no node, no `vscode` (enforced by the layer
 * guard).
 */

import type { CommandInfoModel } from './session';

/** Where a command goes when it is submitted. */
export type FrontendCommandKind =
  /** Has a dedicated control-plane intent on the bridge (send that, not a command). */
  | 'intent'
  /** Forwarded with `runPromptCommand`; the host resolves it. */
  | 'forward';

/** One row of {@link FRONTEND_COMMANDS}. */
export interface FrontendCommand extends CommandInfoModel {
  readonly kind: FrontendCommandKind;
}

/**
 * Commands the frontend intercepts.
 *
 * Names and aliases carry the **leading slash** (that is what the user types, what
 * `LOCAL_COMMANDS` already uses, and what `runPromptCommand.name` expects on the
 * wire — see `interfaces.md`).
 *
 * Wording is ours; the **names and aliases** are a 1:1 mirror of the TUI's
 * `TUI_ONLY_COMMANDS` (`crates/wing/src/ui/popup/command.rs:59`), minus the two Goal
 * commands (out of scope for this extension) — `tests/shared/commands.test.ts` pins
 * that list, so a drift shows up as a failing test.
 * `kind: 'forward'` rows never reach the switch in the composer's routing table:
 * the host owns their implementation (`/clear`, `/copy`, `/context`, …).
 */
export const FRONTEND_COMMANDS: readonly FrontendCommand[] = [
  {
    name: '/new',
    aliases: [],
    description: 'New session',
    params: '',
    kind: 'intent',
  },
  {
    name: '/clear',
    aliases: [],
    description: 'Clear the chat view',
    params: '',
    kind: 'forward',
  },
  {
    name: '/copy',
    aliases: [],
    description: 'Copy the last assistant message',
    params: '[N]',
    kind: 'forward',
  },
  {
    name: '/session',
    aliases: ['/ss'],
    description: 'Switch or list sessions',
    params: '[session_id]',
    kind: 'intent',
  },
  {
    name: '/model',
    aliases: ['/m'],
    description: 'Select provider and model',
    params: '',
    kind: 'intent',
  },
  {
    name: '/agents',
    aliases: [],
    description: 'Switch or show the agent template',
    params: '[name]',
    kind: 'forward',
  },
  {
    name: '/title',
    aliases: [],
    description: 'Show or set the session title',
    params: '[name]',
    kind: 'forward',
  },
  {
    name: '/workdir',
    aliases: [],
    description: 'Show or set the working directory',
    params: '[path]',
    kind: 'forward',
  },
  {
    name: '/think',
    aliases: ['/t'],
    description: 'Toggle thinking / set effort',
    params: 'on|off|low|medium|high|xhigh|max',
    kind: 'intent',
  },
  {
    name: '/yolo',
    aliases: [],
    description: 'Toggle YOLO mode',
    params: 'on|off',
    kind: 'intent',
  },
  {
    name: '/compact',
    aliases: [],
    description: 'Compact the session context',
    params: '[focus]',
    kind: 'intent',
  },
  {
    name: '/context',
    aliases: [],
    description: 'Show context stats and the system prompt',
    params: '',
    kind: 'forward',
  },
  {
    name: '/skills',
    aliases: [],
    description: 'Show the loaded skills',
    params: '',
    kind: 'forward',
  },
  {
    name: '/reload',
    aliases: [],
    description: 'Reload config, hooks and skills',
    params: '',
    kind: 'forward',
  },
  {
    name: '/fork',
    aliases: [],
    description: 'Fork the session at a message',
    params: '[message_uuid]',
    kind: 'intent',
  },
  {
    name: '/rewind',
    aliases: [],
    description: 'Rewind to a message',
    params: '[message_uuid]',
    kind: 'intent',
  },
];

/**
 * Reasoning effort levels the backend accepts.
 *
 * Source: `SessionInfoResponse.reasoning_effort` — "推理力度: low|medium|high|xhigh|max"
 * (`libs/core/wing/gateway/protocol.py:421`) and the TUI's `/think` usage string
 * (`crates/wing/src/app/commands.rs:619`).
 */
export const EFFORT_LEVELS = ['low', 'medium', 'high', 'xhigh', 'max'] as const;

/** One reasoning effort level. */
export type EffortLevel = (typeof EFFORT_LEVELS)[number];

/** A boolean-style argument, classified like the TUI's `parse_bool_arg`. */
export type BoolArg =
  /** No argument at all. */
  | { readonly kind: 'empty' }
  | { readonly kind: 'on' }
  | { readonly kind: 'off' }
  /** Anything else, lowercased for the message. */
  | { readonly kind: 'other'; readonly value: string };

/**
 * Classify an `on|off` argument.
 *
 * Mirror of the TUI's `parse_bool_arg` (`crates/wing/src/app/commands.rs:51-58`):
 * `on|true|1` and `off|false|0`, case-insensitive; everything else is `other` and
 * must **not** be turned into an intent (the caller shows the usage line instead).
 */
export function parseBoolArg(raw: string): BoolArg {
  switch (raw.trim().toLowerCase()) {
    case '':
      return { kind: 'empty' };
    case 'on':
    case 'true':
    case '1':
      return { kind: 'on' };
    case 'off':
    case 'false':
    case '0':
      return { kind: 'off' };
    default:
      return { kind: 'other', value: raw.trim().toLowerCase() };
  }
}

/** True when `value` is one of the levels the backend accepts. */
export function isEffortLevel(value: string): value is EffortLevel {
  return (EFFORT_LEVELS as readonly string[]).includes(value);
}

/**
 * The `current` branch target's uuid on the wire.
 *
 * The gateway appends it as the last entry of `get_branch_targets()`
 * (`libs/core/wing/context_manager.py:925`: `{"uuid": "current", "content": "(current)"}`),
 * meaning "the newest state" rather than a message. The **host** normalizes it into
 * `BranchTargetModel.current`; the webview renders that flag (the constant stays
 * here for the host and for the fixtures that build a gateway-shaped list).
 */
export const BRANCH_CURRENT_UUID = 'current';

/** A parsed slash line. */
export interface ParsedCommand {
  /** Command name including the leading slash (e.g. `/model`). */
  readonly name: string;
  /** Raw argument tail (everything after the first space); `''` when absent. */
  readonly args: string;
}

/** Ensure a command name has exactly one leading slash. */
export function normalizeCommandName(raw: string): string {
  const trimmed = raw.trim();
  return trimmed.startsWith('/') ? trimmed : `/${trimmed}`;
}

/**
 * Split a draft into `{ name, args }` — the webview's port of the TUI's
 * `parse_slash_input` (`crates/wing/src/ui/popup/command.rs:203`).
 *
 * Returns `null` when the text is not a slash line at all (so it is a message).
 * `/model  gpt-4o` parses to `{ name: '/model', args: 'gpt-4o' }`.
 */
export function parseSlashInput(text: string): ParsedCommand | null {
  const left = text.trimStart();
  if (!left.startsWith('/')) {
    return null;
  }
  const space = left.search(/\s/);
  if (space < 0) {
    return { name: left, args: '' };
  }
  return { name: left.slice(0, space), args: left.slice(space + 1).trim() };
}

/** Case-insensitive name / alias lookup in a command table. */
export function matchCommand(
  name: string,
  table: readonly CommandInfoModel[] = FRONTEND_COMMANDS,
): CommandInfoModel | null {
  const normalized = normalizeCommandName(name).toLowerCase();
  for (const command of table) {
    if (normalizeCommandName(command.name).toLowerCase() === normalized) {
      return command;
    }
    for (const alias of command.aliases) {
      if (normalizeCommandName(alias).toLowerCase() === normalized) {
        return command;
      }
    }
  }
  return null;
}

/**
 * Merge the host's catalog with {@link FRONTEND_COMMANDS}, de-duplicating on the
 * normalized name (the host's entry wins, because it may carry fresher wording).
 *
 * Aliases are not merged: a command is one row keyed by its canonical name.
 */
export function mergeCommandCatalog(catalog: readonly CommandInfoModel[]): readonly CommandInfoModel[] {
  const seen = new Set<string>();
  const merged: CommandInfoModel[] = [];
  for (const command of [...catalog, ...FRONTEND_COMMANDS]) {
    const key = normalizeCommandName(command.name).toLowerCase();
    if (seen.has(key)) {
      continue;
    }
    seen.add(key);
    merged.push(command);
  }
  return merged;
}

/**
 * Filter commands for the composer's `/` candidates.
 *
 * Case-insensitive match on the name or any alias, ranked like the TUI's
 * `filter_commands` (`crates/wing/src/ui/popup/command.rs:210-250`): **exact
 * matches first, then prefix matches**, nothing else (the TUI has no substring
 * tier — `/s` must not list `/agents`).
 */
export function filterCommands(
  commands: readonly CommandInfoModel[],
  filter: string,
): readonly CommandInfoModel[] {
  const needle = filter.replace(/^\//, '').toLowerCase();
  if (needle === '') {
    return commands;
  }
  const exact: CommandInfoModel[] = [];
  const prefix: CommandInfoModel[] = [];
  for (const command of commands) {
    const names = [command.name, ...command.aliases].map((name) =>
      normalizeCommandName(name).slice(1).toLowerCase(),
    );
    if (names.includes(needle)) {
      exact.push(command);
    } else if (names.some((name) => name.startsWith(needle))) {
      prefix.push(command);
    }
  }
  return [...exact, ...prefix];
}
