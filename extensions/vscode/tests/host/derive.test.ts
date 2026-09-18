import { describe, expect, it } from 'vitest';

import {
  ASK_UNANSWERED,
  DIFF_MAX_ROWS,
  buildAskReply,
  buildDiffWindow,
  commandOneLine,
  deriveTitle,
  lineDiff,
  normalizeAsk,
  splitLines,
  todoItemsFromArgs,
  toolDisplay,
  truncateChars,
  truncatePathSegments,
} from '../../src/host/session/derive';
import { parsePartialJson } from '../../src/host/session/partial-json';
import type { AskEvent } from '../../src/core';

/**
 * The pure derivations — ported from the TUI (`tool_call.rs`, `diff_view.rs`,
 * `ask.rs`, `partial_json.rs`). These tests pin the ported semantics: if a rule
 * here changes, both frontends must change together.
 */

describe('truncateChars', () => {
  it('counts code points, not UTF-16 units', () => {
    expect(truncateChars('hello', 10)).toBe('hello');
    expect(truncateChars('hello world!', 8)).toBe('hello...');
    // 8 CJK chars; max 8 → unchanged, max 6 → 3 chars + "..."
    expect(truncateChars('中文标题测试用例', 8)).toBe('中文标题测试用例');
    expect(truncateChars('中文标题测试用例', 6)).toBe('中文标...');
    expect(Array.from(truncateChars('😀'.repeat(6), 5)).length).toBe(5);
  });
});

describe('deriveTitle', () => {
  const base = { workspace: '/work/project', maxLength: 100, fallback: 'New session' };

  it('prefers the explicit name', () => {
    expect(deriveTitle({ ...base, explicit: 'My session', firstUserText: 'first message' })).toBe(
      'My session',
    );
  });

  it('falls back to the first user message truncated to 100 chars', () => {
    const long = 'x'.repeat(150);
    const title = deriveTitle({ ...base, explicit: null, firstUserText: long });
    expect(Array.from(title).length).toBe(100);
    expect(title.endsWith('...')).toBe(true);
  });

  it('falls back to the workspace basename and then the fallback string', () => {
    expect(deriveTitle({ ...base, explicit: null, firstUserText: '  ' })).toBe('project');
    expect(deriveTitle({ ...base, workspace: null, explicit: null, firstUserText: null })).toBe(
      'New session',
    );
  });
});

describe('tool display', () => {
  it('summarizes Bash on one line', () => {
    expect(toolDisplay('Bash', { command: 'pnpm test\n--watch' })).toEqual({
      title: 'Bash',
      subject: 'pnpm test⏎--watch',
    });
  });

  it('keeps the last six path segments and adds the Read offset', () => {
    const deep = '/Users/a/b/c/d/e/f/g/file.ts';
    expect(toolDisplay('Read', { path: deep, offset: 42 }).subject).toBe('c/d/e/f/g/file.ts:42');
    expect(toolDisplay('Read', { path: deep, offset: 1 }).subject).toBe('c/d/e/f/g/file.ts');
    expect(toolDisplay('Write', { path: deep }).subject).toBe('c/d/e/f/g/file.ts');
    expect(toolDisplay('Edit', { path: 'a.ts' }).subject).toBe('a.ts');
  });

  it('renders Glob/Grep with path and pattern', () => {
    expect(toolDisplay('Grep', { path: 'src', pattern: 'TODO' }).subject).toBe('src, TODO');
    expect(toolDisplay('Glob', { pattern: '*.ts' }).subject).toBe('*.ts');
  });

  it('summarizes TodoWrite item counts and hides AskUserQuestion args', () => {
    expect(
      toolDisplay('TodoWrite', {
        todos: [
          { content: 'a', status: 'completed' },
          { content: 'b', status: 'in_progress' },
        ],
      }).subject,
    ).toBe('2 items · 1 open');
    expect(toolDisplay('AskUserQuestion', { questions: [{}] }).subject).toBe('');
  });

  it('falls back to key=value pairs for unknown tools', () => {
    expect(toolDisplay('Explorer', { prompt: 'find things', purpose: 'skip me' })).toEqual({
      title: 'Explorer',
      subject: 'prompt=find things',
    });
    expect(toolDisplay('Explorer', null).subject).toBe('');
  });
});

describe('line diff', () => {
  it('keeps context and groups deletions before insertions', () => {
    const rows = lineDiff(['a', 'b', 'c'], ['a', 'B', 'c']);
    expect(rows.map((row) => [row.kind, row.text])).toEqual([
      ['context', 'a'],
      ['del', 'b'],
      ['add', 'B'],
      ['context', 'c'],
    ]);
  });

  it('handles pure insertions, deletions and identical inputs', () => {
    expect(lineDiff([], ['x']).map((row) => row.kind)).toEqual(['add']);
    expect(lineDiff(['x'], []).map((row) => row.kind)).toEqual(['del']);
    expect(lineDiff(['x', 'y'], ['x', 'y']).map((row) => row.kind)).toEqual(['context', 'context']);
  });

  it('reconstructs both sides exactly (edit-script soundness)', () => {
    const oldLines = ['one', 'two', 'three', 'four', 'five'];
    const newLines = ['one', 'TWO', 'three', 'four and a half', 'five', 'six'];
    const rows = lineDiff(oldLines, newLines);
    const rebuiltOld = rows.filter((row) => row.kind !== 'add').map((row) => row.text);
    const rebuiltNew = rows.filter((row) => row.kind !== 'del').map((row) => row.text);
    expect(rebuiltOld).toEqual(oldLines);
    expect(rebuiltNew).toEqual(newLines);
  });
});

describe('diff window', () => {
  it('numbers rows from the absolute start lines and writes a git-style hunk', () => {
    const window = buildDiffWindow({
      oldText: 'l7\nl8\nold9\nl10',
      newText: 'l7\nl8\nnew9\nl10',
      oldStartLine: 7,
      newStartLine: 7,
      maxRows: DIFF_MAX_ROWS,
    });
    expect(window.lines[0]?.text).toBe('@@ -7,4 +7,4 @@');
    expect(window.lines.map((line) => [line.kind, line.oldLine, line.newLine])).toEqual([
      ['hunk', null, null],
      ['context', 7, 7],
      ['context', 8, 8],
      ['del', 9, null],
      ['add', null, 9],
      ['context', 10, 10],
    ]);
    expect([window.added, window.removed, window.truncated]).toEqual([1, 1, false]);
  });

  it('renders a new file as all-additions with the zero-side header', () => {
    const window = buildDiffWindow({
      oldText: null,
      newText: 'first\nsecond',
      oldStartLine: 1,
      newStartLine: 3,
      maxRows: DIFF_MAX_ROWS,
    });
    expect(window.lines[0]?.text).toBe('@@ -0,0 +3,2 @@');
    expect(window.lines.slice(1).every((line) => line.kind === 'add')).toBe(true);
    expect(window.added).toBe(2);
    expect(window.removed).toBe(0);
  });

  it('windows an oversized payload and flags it', () => {
    const lines = Array.from({ length: 40 }, (_, index) => `line ${index}`);
    const window = buildDiffWindow({
      oldText: null,
      newText: lines.join('\n'),
      oldStartLine: 1,
      newStartLine: 1,
      maxRows: 10,
    });
    expect(window.truncated).toBe(true);
    expect(window.lines).toHaveLength(11); // header + 10 rows
    expect(window.added).toBe(10);
  });

  it('never emits a diff for identical texts beyond a context-only window', () => {
    const window = buildDiffWindow({
      oldText: 'same',
      newText: 'same',
      oldStartLine: 1,
      newStartLine: 1,
      maxRows: DIFF_MAX_ROWS,
    });
    expect(window.lines.map((line) => line.kind)).toEqual(['hunk', 'context']);
  });
});

describe('splitLines', () => {
  it('mirrors Rust `str::lines()` (trailing newline dropped, CR stripped)', () => {
    expect(splitLines('a\nb\n')).toEqual(['a', 'b']);
    expect(splitLines('a\r\nb')).toEqual(['a', 'b']);
    expect(splitLines('')).toEqual([]);
  });
});

describe('ask normalization and replies', () => {
  const META = { created_at: '', request_id: '', session_id: null, uuid: null } as const;
  const multiAsk: AskEvent = {
    ...META,
    type: 'ask',
    tool_call_id: 'ask-1',
    questions: [
      {
        id: 'color',
        header: 'Color',
        question: 'Which?',
        multiSelect: false,
        options: [{ label: 'Dark', description: 'dim' }],
        choices: [],
      },
      {
        id: 'legacy',
        header: '',
        question: 'Old shape?',
        multiSelect: false,
        options: [],
        choices: ['Yes', 'No'],
      },
    ],
    question: '',
    choices: [],
    required: false,
  };

  it('normalizes questions, folding legacy choices into options', () => {
    const normalized = normalizeAsk(multiAsk);
    expect(normalized.approval).toBe(false);
    expect(normalized.answerable).toBe(true);
    expect(normalized.questions[0]?.options).toEqual([{ label: 'Dark', description: 'dim' }]);
    expect(normalized.questions[1]?.options).toEqual([
      { label: 'Yes', description: '' },
      { label: 'No', description: '' },
    ]);
  });

  it('folds the retired required ask into a single required choice', () => {
    const normalized = normalizeAsk({
      type: 'ask',
      tool_call_id: 'ask-2',
      questions: [],
      question: 'Proceed?',
      choices: ['y', 'n', 'yolo'],
      required: true,
      created_at: '',
      request_id: '',
      session_id: null,
      uuid: null,
    });
    expect(normalized.approval).toBe(true);
    expect(normalized.questions).toEqual([
      {
        id: 'choice',
        question: 'Proceed?',
        header: '',
        multiSelect: false,
        options: [
          { label: 'y', description: '' },
          { label: 'n', description: '' },
          { label: 'yolo', description: '' },
        ],
        required: true,
      },
    ]);
  });

  it('marks a retired non-required ask as unanswerable', () => {
    const normalized = normalizeAsk({
      type: 'ask',
      tool_call_id: 'ask-3',
      questions: [],
      question: 'heads up',
      choices: ['a'],
      required: false,
      created_at: '',
      request_id: '',
      session_id: null,
      uuid: null,
    });
    expect(normalized.answerable).toBe(false);
  });

  it('builds `header: answer` replies, using the placeholder for skips', () => {
    const questions = normalizeAsk(multiAsk).questions;
    const reply = buildAskReply({ approval: false, questions }, [
      { questionId: 'color', selected: [], text: '' },
      { questionId: 'legacy', selected: ['No'], text: '' },
    ]);
    expect(reply).toBe(`Color: ${ASK_UNANSWERED}\nlegacy: No`);
  });

  it('joins multi-select labels in option order and appends free-form text', () => {
    const questions = normalizeAsk({
      type: 'ask',
      tool_call_id: 'ask-4',
      questions: [
        {
          id: 'q',
          header: 'Q',
          question: 'Pick',
          multiSelect: true,
          options: [
            { label: 'A', description: '' },
            { label: 'B', description: '' },
            { label: 'C', description: '' },
          ],
          choices: [],
        },
      ],
      question: '',
      choices: [],
      required: false,
      created_at: '',
      request_id: '',
      session_id: null,
      uuid: null,
    }).questions;
    const reply = buildAskReply({ approval: false, questions }, [
      { questionId: 'q', selected: ['C', 'A'], text: 'custom' },
    ]);
    expect(reply).toBe('Q: A, C, custom');
  });

  it('builds a bare label for an approval', () => {
    expect(
      buildAskReply(
        {
          approval: true,
          questions: [
            {
              id: 'choice',
              question: 'Proceed?',
              header: '',
              multiSelect: false,
              options: [{ label: 'y', description: '' }],
              required: true,
            },
          ],
        },
        [{ questionId: 'choice', selected: ['y'], text: '' }],
      ),
    ).toBe('y');
  });
});

describe('todos and command helpers', () => {
  it('parses TodoWrite items and normalizes unknown statuses', () => {
    expect(
      todoItemsFromArgs({
        todos: [
          { content: 'a', status: 'completed' },
          { content: 'b', status: 'weird' },
          { notContent: true },
        ],
      }),
    ).toEqual([
      { content: 'a', status: 'completed' },
      { content: 'b', status: 'pending' },
      { content: '', status: 'pending' },
    ]);
    expect(todoItemsFromArgs({})).toBeNull();
    expect(todoItemsFromArgs(null)).toBeNull();
  });

  it('collapses commands and paths like the TUI', () => {
    expect(commandOneLine('  ls -la  ')).toBe('ls -la');
    expect(commandOneLine('a\r\nb\nc')).toBe('a⏎b⏎c');
    expect(truncatePathSegments('a/b/c/d/e/f/g/h')).toBe('c/d/e/f/g/h');
    expect(truncatePathSegments('a/b')).toBe('a/b');
    expect(truncatePathSegments('/a/b/')).toBe('/a/b');
  });
});

describe('partial JSON', () => {
  it('parses complete JSON through the fast path', () => {
    expect(parsePartialJson('{"command":"ls -la"}')).toEqual({ command: 'ls -la' });
  });

  it('keeps fields that were already complete when the stream is cut', () => {
    expect(parsePartialJson('{"command": "pnpm test')).toEqual({ command: 'pnpm test' });
    expect(parsePartialJson('{"a":1,"b":')).toEqual({ a: 1, b: null });
    expect(parsePartialJson('{"a":1,"b":2')).toEqual({ a: 1, b: 2 });
    expect(parsePartialJson('{"path":"src/a.ts","content":"line1\\nline2')).toEqual({
      path: 'src/a.ts',
      content: 'line1\nline2',
    });
  });

  it('parses arrays and nested objects', () => {
    expect(parsePartialJson('{"todos":[{"content":"a","status":"pending"')).toEqual({
      todos: [{ content: 'a', status: 'pending' }],
    });
    expect(parsePartialJson('[1, 2, 3')).toEqual([1, 2, 3]);
  });

  it('handles escapes and surrogate pairs', () => {
    expect(parsePartialJson('{"s":"a\\"b"}')).toEqual({ s: 'a"b' });
    expect(parsePartialJson('{"s":"\\uD83D\\uDE00"}')).toEqual({ s: '😀' });
    expect(parsePartialJson('{"s":"\\q"}')).toEqual({ s: '\\q' });
  });

  it('returns null for garbage and non-JSON', () => {
    expect(parsePartialJson('')).toBeNull();
    expect(parsePartialJson('   ')).toBeNull();
    expect(parsePartialJson('not json')).toBeNull();
    expect(parsePartialJson('{"a": tru')).toEqual({ a: true });
  });
});
