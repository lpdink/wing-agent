// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/tests/ansi.client.spec.ts
// Modified for Wing: a representative subset — the colour/decorations mapping, the
// cursor-replay and erase cases, column arithmetic and line-end state — keeping the
// source's verified-against-a-real-terminal fixtures; the escape-sequence and CRLF
// cases are covered by the same code paths below.

import { describe, expect, it } from 'vitest';

import { parseAnsiLines } from '../../src/tool/ansi';
import type { AnsiSpan } from '../../src/tool/ansi';

const ESC = '\u001b';
const BS = '\u0008';

function sgr(codes: string, text: string): string {
  return `${ESC}[${codes}m${text}${ESC}[0m`;
}

function onlySpan(text: string): AnsiSpan {
  const lines = parseAnsiLines(text);
  expect(lines).toHaveLength(1);
  expect(lines[0]).toHaveLength(1);
  return lines[0]![0]!;
}

describe('parseAnsiLines: text without SGR state', () => {
  it('leaves plain text as one unstyled span', () => {
    expect(parseAnsiLines('hello')).toEqual([[{ text: 'hello', style: undefined }]]);
  });

  it('returns exactly one empty line for empty input', () => {
    expect(parseAnsiLines('')).toEqual([[]]);
  });

  it('splits a multi-line run and keeps an interior blank line', () => {
    expect(parseAnsiLines('a\n\nb')).toEqual([
      [{ text: 'a', style: undefined }],
      [],
      [{ text: 'b', style: undefined }],
    ]);
  });

  it('keeps tabs, which the terminal surface needs for column layout', () => {
    expect(onlySpan('a\tb')).toEqual({ text: 'a\tb', style: undefined });
  });
});

describe('parseAnsiLines: basic colours mapped onto theme tokens', () => {
  it.each<[string, string]>([
    ['30', 'var(--dsw-alias-label-primary)'],
    ['37', 'var(--dsw-alias-label-primary)'],
    ['90', 'var(--dsw-alias-label-tertiary)'],
    ['31', 'var(--dsw-alias-state-error-primary)'],
    ['91', 'var(--dsw-alias-state-error-secondary)'],
    ['32', 'var(--dsw-alias-state-success-primary)'],
    ['92', 'var(--dsw-alias-state-success-secondary)'],
    ['33', 'var(--dsw-alias-state-warn-primary)'],
    ['93', 'var(--dsw-alias-state-warn-secondary)'],
    ['34', 'var(--dsw-alias-state-business-primary)'],
    ['94', 'var(--dsw-static-blue-400)'],
    ['36', 'var(--dsw-static-blue-600)'],
    ['96', 'var(--dsw-static-blue-500)'],
  ])('SGR %s resolves to %s', (code, token) => {
    expect(onlySpan(sgr(code, 'x'))).toEqual({ text: 'x', style: { color: token } });
  });

  it('falls through to anser’s literal for colours with no token equivalent', () => {
    expect(onlySpan(sgr('35', 'x')).style).toEqual({ color: 'rgb(187, 0, 187)' });
    expect(onlySpan(sgr('38;5;208', 'x')).style).toEqual({ color: 'rgb(255, 135, 0)' });
    expect(onlySpan(sgr('38;2;10;20;30', 'x')).style).toEqual({ color: 'rgb(10, 20, 30)' });
  });

  it('keeps the literal foreground when the run paints its own background', () => {
    expect(onlySpan(sgr('41;37', 'x')).style).toEqual({
      backgroundColor: 'rgb(187, 0, 0)',
      color: 'rgb(255,255,255)',
    });
  });
});

describe('parseAnsiLines: decorations', () => {
  it.each<[string, Record<string, unknown>]>([
    ['1', { fontWeight: 700 }],
    ['2', { opacity: 0.7 }],
    ['3', { fontStyle: 'italic' }],
    ['4', { textDecoration: 'underline' }],
    ['9', { textDecoration: 'line-through' }],
    ['8', { visibility: 'hidden' }],
  ])('SGR %s resolves to %o', (code, style) => {
    expect(onlySpan(sgr(code, 'x')).style).toEqual(style);
  });

  it('lets the later textDecoration win when a run declares both', () => {
    expect(onlySpan(sgr('4;9', 'x')).style).toEqual({ textDecoration: 'line-through' });
    expect(onlySpan(sgr('9;4', 'x')).style).toEqual({ textDecoration: 'underline' });
  });

  it('reproduces no animation for blink, leaving the run unstyled', () => {
    expect(onlySpan(sgr('5', 'x'))).toEqual({ text: 'x', style: undefined });
  });
});

describe('parseAnsiLines: sequences that carry no colour', () => {
  it('removes OSC strings and non-CSI escapes', () => {
    expect(onlySpan(`a${ESC}]0;window title\u0007b`)).toEqual({ text: 'ab', style: undefined });
    expect(onlySpan(`x${ESC}(By${ESC}cz`)).toEqual({ text: 'xyz', style: undefined });
  });

  it('removes inert C0 controls but keeps cursor-only CSI out of the text', () => {
    expect(onlySpan('\u0000ab\u001fc\u007f')).toEqual({ text: 'abc', style: undefined });
    expect(onlySpan(`${ESC}[2K${ESC}[1Adone`)).toEqual({ text: 'done', style: undefined });
  });
});

describe('parseAnsiLines: carriage returns and backspaces', () => {
  it('keeps only the last redraw of a line', () => {
    expect(onlySpan('10%\r55%\r100%')).toEqual({ text: '100%', style: undefined });
  });

  it('leaves the tail of a longer frame standing under a shorter redraw', () => {
    // Verified against a real terminal: `100%\rOK` paints `OK0%`.
    expect(onlySpan('100%\rOK')).toEqual({ text: 'OK0%', style: undefined });
    expect(onlySpan('abcdef\rXY')).toEqual({ text: 'XYcdef', style: undefined });
  });

  it('keeps SGR state in force across a redraw, as a terminal does', () => {
    expect(onlySpan(`${ESC}[31mgone\rkept`)).toEqual({
      text: 'kept',
      style: { color: 'var(--dsw-alias-state-error-primary)' },
    });
  });

  it('applies a backspace as the overwrite a terminal draws', () => {
    expect(onlySpan(`abc${BS}${BS}XY`)).toEqual({ text: 'aXY', style: undefined });
    expect(onlySpan(`ab${BS}${BS}${BS}${BS}xyz`)).toEqual({ text: 'xyz', style: undefined });
  });

  it('treats a trailing backspace as a cursor move, not a delete', () => {
    expect(onlySpan(`abc${BS}`)).toEqual({ text: 'abc', style: undefined });
  });

  it('steps over an SGR sequence instead of erasing its bytes', () => {
    expect(parseAnsiLines(`${sgr('31', 'abc')}${BS}${BS}XY`)).toEqual([
      [
        { text: 'a', style: { color: 'var(--dsw-alias-state-error-primary)' } },
        { text: 'XY', style: undefined },
      ],
    ]);
  });

  it('keeps a CRLF pair as two lines instead of reading it as a redraw', () => {
    expect(parseAnsiLines('a\r\r\nb\r\n')).toEqual([
      [{ text: 'a', style: undefined }],
      [{ text: 'b', style: undefined }],
      [],
    ]);
  });
});

describe('parseAnsiLines: erase and column arithmetic', () => {
  it('erases the rest of the line, the fixed companion of a redraw', () => {
    expect(onlySpan(`100%\r${ESC}[KOK`)).toEqual({ text: 'OK', style: undefined });
    expect(onlySpan(`100%\r${ESC}[0KOK`)).toEqual({ text: 'OK', style: undefined });
  });

  it('erases the whole line for the 2K form and to the cursor for 1K', () => {
    expect(onlySpan(`ab\r${ESC}[2Kxy`)).toEqual({ text: 'xy', style: undefined });
    expect(onlySpan(`abcd${ESC}[1K|`)).toEqual({ text: '    |', style: undefined });
  });

  it('paints columns a 2K dropped as blanks when a later write lands past them', () => {
    expect(onlySpan(`abcd${ESC}[2Kx`)).toEqual({ text: '    x', style: undefined });
  });

  it('advances a redraw cursor by tab stops, leaving a tabbed column standing', () => {
    expect(onlySpan('a\tb\rXY')).toEqual({ text: 'XY      b', style: undefined });
  });

  it('counts a wide character as the two columns a terminal advances', () => {
    expect(onlySpan('中x\rab')).toEqual({ text: 'abx', style: undefined });
  });

  it('does not accumulate a cursor or erase sequence into a cell style', () => {
    expect(parseAnsiLines(`${ESC}[31ma\r${ESC}[Kb`)).toEqual([
      [{ text: 'b', style: { color: 'var(--dsw-alias-state-error-primary)' } }],
    ]);
  });
});

describe('parseAnsiLines: line-end state', () => {
  it('closes a run whose reset lands after the last written cell', () => {
    // The shape every build tool writes: `\r\x1b[K\x1b[32m✓ built\x1b[0m`.
    expect(parseAnsiLines(`${ESC}[32mdone\rok${ESC}[0m\nplain`)).toEqual([
      [{ text: 'okne', style: { color: 'var(--dsw-alias-state-success-primary)' } }],
      [{ text: 'plain', style: undefined }],
    ]);
  });
});
