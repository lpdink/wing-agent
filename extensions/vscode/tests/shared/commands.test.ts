import { describe, expect, it } from 'vitest';

import type { CommandInfoModel } from '../../src/shared';
import {
  BRANCH_CURRENT_UUID,
  EFFORT_LEVELS,
  FRONTEND_COMMANDS,
  filterCommands,
  isEffortLevel,
  matchCommand,
  mergeCommandCatalog,
  normalizeCommandName,
  parseSlashInput,
} from '../../src/shared';

/**
 * The frontend command vocabulary (step 05).
 *
 * These helpers decide two things the whole shell hangs on:
 * 1. "is this draft a command or a message?" (`parseSlashInput` + `matchCommand`);
 * 2. "which rows does the `/` candidate list show?" (`mergeCommandCatalog` + `filterCommands`).
 *
 * They mirror the TUI (`crates/wing/src/ui/popup/command.rs`), whose behaviour is the
 * reference implementation — the assertions below quote it where it matters.
 */

describe('parseSlashInput', () => {
  it('splits a command from its argument tail (TUI `parse_slash_input`)', () => {
    expect(parseSlashInput('/model')).toEqual({ name: '/model', args: '' });
    expect(parseSlashInput('/model gpt-4o')).toEqual({ name: '/model', args: 'gpt-4o' });
    expect(parseSlashInput('/model   gpt-4o  ')).toEqual({ name: '/model', args: 'gpt-4o' });
    expect(parseSlashInput('  /ss sess-1')).toEqual({ name: '/ss', args: 'sess-1' });
    expect(parseSlashInput('/compact focus on the parser')).toEqual({
      name: '/compact',
      args: 'focus on the parser',
    });
  });

  it('returns null for anything that is not a slash line', () => {
    expect(parseSlashInput('hello')).toBeNull();
    expect(parseSlashInput('')).toBeNull();
    expect(parseSlashInput('  ')).toBeNull();
    // A slash that is not the first character is a path, not a command.
    expect(parseSlashInput('src/webview')).toBeNull();
  });

  it('keeps a bare slash as a name (the candidate list owns that state)', () => {
    expect(parseSlashInput('/')).toEqual({ name: '/', args: '' });
  });
});

describe('normalizeCommandName', () => {
  it('adds the leading slash exactly once', () => {
    expect(normalizeCommandName('ss')).toBe('/ss');
    expect(normalizeCommandName('/ss')).toBe('/ss');
    expect(normalizeCommandName('  /ss ')).toBe('/ss');
  });
});

describe('matchCommand', () => {
  it('matches canonical names and aliases', () => {
    expect(matchCommand('/ss')?.name).toBe('/session');
    expect(matchCommand('/session')?.name).toBe('/session');
    expect(matchCommand('/m')?.name).toBe('/model');
    expect(matchCommand('/t')?.name).toBe('/think');
  });

  it('is case-insensitive and tolerant of the missing slash', () => {
    expect(matchCommand('YOLO')?.name).toBe('/yolo');
    expect(matchCommand('compact')?.name).toBe('/compact');
  });

  it('returns null for commands the frontend does not route', () => {
    // `/init` is a gateway prompt command: it is forwarded, not routed locally.
    expect(matchCommand('/init')).toBeNull();
    expect(matchCommand('/help')).toBeNull();
  });

  it('pins the vocabulary the TUI also carries', () => {
    // Sources: `crates/wing/src/ui/popup/command.rs:59` (TUI_ONLY_COMMANDS) and
    // `crates/wing/src/app/commands.rs:88` (COMMANDS). Goal commands are out of scope.
    const names = FRONTEND_COMMANDS.map((command) => command.name);
    expect(names).toEqual([
      '/new',
      '/clear',
      '/copy',
      '/session',
      '/model',
      '/agents',
      '/title',
      '/workdir',
      '/think',
      '/yolo',
      '/compact',
      '/context',
      '/skills',
      '/reload',
      '/fork',
      '/rewind',
    ]);
    expect(names).not.toContain('/goal');

    // Names and aliases are unique, so no row can shadow another (the TUI pins the
    // same property in `commands.rs`).
    const spelled = FRONTEND_COMMANDS.flatMap((command) => [command.name, ...command.aliases]);
    expect(new Set(spelled).size).toBe(spelled.length);
    expect(spelled.every((name) => name.startsWith('/'))).toBe(true);
  });
});

describe('mergeCommandCatalog', () => {
  const gateway: readonly CommandInfoModel[] = [
    { name: 'init', aliases: [], description: 'Initialize the workspace', params: '' },
    { name: 'help', aliases: [], description: 'Show help', params: '' },
    { name: 'compact', aliases: [], description: 'Compress context', params: '[focus]' },
  ];

  it('keeps the host wording for a command both sides know', () => {
    const merged = mergeCommandCatalog(gateway);
    const compact = merged.find((command) => normalizeCommandName(command.name) === '/compact');
    expect(compact?.description).toBe('Compress context');
    expect(merged.filter((command) => normalizeCommandName(command.name) === '/compact')).toHaveLength(1);
  });

  it('appends the frontend commands the host does not know', () => {
    const merged = mergeCommandCatalog(gateway);
    const names = merged.map((command) => normalizeCommandName(command.name));
    expect(names).toContain('/init');
    expect(names).toContain('/session');
    expect(names).toContain('/rewind');
    // Aliases ride along with their command (`/ss` for `/session`).
    expect(merged.some((command) => command.aliases.includes('/ss'))).toBe(true);
    // De-duplicated: no name twice.
    expect(new Set(names).size).toBe(names.length);
  });

  it('handles an empty catalog (host has not fetched anything yet)', () => {
    expect(mergeCommandCatalog([])).toEqual(FRONTEND_COMMANDS);
  });
});

describe('filterCommands', () => {
  const commands = mergeCommandCatalog([
    { name: 'init', aliases: [], description: 'Initialize', params: '' },
  ]);

  it('returns everything for an empty filter', () => {
    expect(filterCommands(commands, '')).toHaveLength(commands.length);
    expect(filterCommands(commands, '/')).toHaveLength(commands.length);
  });

  it('ranks exact matches above prefix matches above the rest', () => {
    const filtered = filterCommands(commands, 'in');
    // `/init` is a prefix match; nothing exact.
    expect(filtered[0]?.name).toBe('init');
  });

  it('matches aliases too', () => {
    const filtered = filterCommands(commands, 'ss');
    expect(filtered.map((command) => normalizeCommandName(command.name))).toContain('/session');
  });

  it('drops everything that does not match', () => {
    expect(filterCommands(commands, 'zzz')).toEqual([]);
  });
});

describe('effort vocabulary', () => {
  it('mirrors the backend levels', () => {
    expect(EFFORT_LEVELS).toEqual(['low', 'medium', 'high', 'xhigh', 'max']);
    expect(isEffortLevel('high')).toBe(true);
    expect(isEffortLevel('huge')).toBe(false);
  });
});

describe('branch current marker', () => {
  it('is the gateway sentinel', () => {
    // `libs/core/wing/context_manager.py:925` appends `{"uuid": "current", ...}`.
    expect(BRANCH_CURRENT_UUID).toBe('current');
  });
});
