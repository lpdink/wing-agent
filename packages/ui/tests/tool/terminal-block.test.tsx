// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/tests/terminal-block.client.spec.tsx
// Modified for Wing: the label fixtures are this package's own English constants
// (the component takes copy via props, not a locale), the copy cases exercise the
// browser clipboard helper directly, and the ANSI fixture renders through the
// ported parser. Same structural assertions otherwise.

import { act, fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { DEFAULT_TERMINAL_MAX_LINES, TerminalBlock } from '../../src/tool/TerminalBlock';
import type { TerminalBlockLabels } from '../../src/tool/TerminalBlock';

const ESC = '\u001b';

const LABELS: TerminalBlockLabels = {
  signal: (signal) => `killed by ${signal}`,
  exitCode: (code) => `exit ${code}`,
  noExitCode: 'no exit code',
  running: 'Running',
  failed: 'Failed',
  done: 'Done',
  copy: 'Copy',
  copied: 'Copied',
  noOutput: 'No output',
  collapseAria: 'Collapse output',
  collapse: 'Collapse',
  expandAria: (hidden) => `Show ${hidden} more lines`,
  expand: (hidden) => `… ${hidden} more lines`,
};

function outputLines(container: HTMLElement): string[] {
  return [...container.querySelectorAll('[class^="line"]')].map((row) => row.textContent ?? '');
}

function runStateOf(container: HTMLElement): { state: string | null; label: string | undefined } {
  return {
    state: container.querySelector('[class*="runState"][data-state]')?.getAttribute('data-state') ?? null,
    label: container.querySelector('[class^="runStateLabel"]')?.textContent ?? undefined,
  };
}

function promptRows(container: HTMLElement): string[] {
  return [...container.querySelectorAll('[class^="promptLine"]')].map((row) =>
    (row.textContent ?? '').trim(),
  );
}

function body(count: number): string {
  return Array.from({ length: count }, (_value, index) => `line ${index + 1}`).join('\n');
}

function stubClipboard(writeText: (text: string) => Promise<void>): void {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
}

describe('TerminalBlock prompt label', () => {
  it('collapses the home directory to ~ and shows only the last segment below it', () => {
    const view = render(<TerminalBlock command="ls" cwd="/Users/me" home="/Users/me" labels={LABELS} />);
    expect(view.getByText('~')).toBeInTheDocument();
    view.rerender(<TerminalBlock command="ls" cwd="/Users/me/Documents" home="/Users/me" labels={LABELS} />);
    expect(view.getByText('Documents')).toBeInTheDocument();
    view.rerender(<TerminalBlock command="ls" cwd="C:\\Users\\me" home="C:\\Users\\me" labels={LABELS} />);
    expect(view.getByText('~')).toBeInTheDocument();
  });

  it('renders a plain $ with no cwd and the command verbatim after the label', () => {
    render(<TerminalBlock command="git log --oneline | head -3" labels={LABELS} />);
    expect(screen.getByText('$')).toBeInTheDocument();
    expect(screen.getByText('git log --oneline | head -3')).toBeInTheDocument();
  });
});

describe('TerminalBlock states', () => {
  it('running with no output shows the command line only: no body, no placeholder, no copy', () => {
    const view = render(<TerminalBlock command="sleep 5" running labels={LABELS} />);
    expect(outputLines(view.container)).toEqual([]);
    expect(view.queryByText('No output')).toBeNull();
    expect(view.queryByRole('button')).toBeNull();
    expect(view.container.firstElementChild?.getAttribute('data-running')).toBe('');
    expect(view.container.firstElementChild?.getAttribute('data-body')).toBeNull();
  });

  it('running with live output streams the text and draws the banner divider', () => {
    const view = render(<TerminalBlock command="sleep 5" running output="partial" labels={LABELS} />);
    expect(view.getByText('partial')).toBeInTheDocument();
    expect(view.container.firstElementChild?.getAttribute('data-body')).toBe('');
  });

  it('settled with no visible output shows the placeholder and no copy control', () => {
    const view = render(<TerminalBlock command="true" output={`${ESC}[0m`} exitCode={0} labels={LABELS} />);
    expect(view.getByText('No output')).toBeInTheDocument();
    expect(view.queryByRole('button')).toBeNull();
  });

  it('drops the output terminator instead of drawing a blank line, keeping a genuine blank', () => {
    const view = render(<TerminalBlock command="ls" output={'a\nb\n'} labels={LABELS} />);
    expect(outputLines(view.container)).toEqual(['a', 'b']);
    view.rerender(<TerminalBlock command="ls" output={`a\nb\n${ESC}[0m`} labels={LABELS} />);
    expect(outputLines(view.container)).toEqual(['a', 'b']);
    view.rerender(<TerminalBlock command="ls" output={'a\nb\n\n'} labels={LABELS} />);
    expect(outputLines(view.container)).toEqual(['a', 'b', '']);
  });

  it('renders ANSI runs as styled spans and uncoloured text bare', () => {
    const view = render(<TerminalBlock command="ls" output={`${ESC}[31mbad${ESC}[39m ok`} labels={LABELS} />);
    expect(outputLines(view.container)).toEqual(['bad ok']);
    const span = view.container.querySelector('[class^="line"] span[style]');
    expect(span?.textContent).toBe('bad');
    expect(span?.getAttribute('style')).toContain('--dsw-alias-state-error-primary');

    view.rerender(<TerminalBlock command="ls" output={'plain one\nplain two\n'} labels={LABELS} />);
    expect(view.container.querySelectorAll('[class^="line"] span')).toHaveLength(0);
  });
});

describe('TerminalBlock status pill and run state', () => {
  it('renders no pill for a clean exit and none while the status is unknown', () => {
    const view = render(<TerminalBlock command="true" output="a" exitCode={0} labels={LABELS} />);
    expect(view.queryByText(/exit |killed by/u)).toBeNull();
    view.rerender(<TerminalBlock command="ls" output="a" labels={LABELS} />);
    expect(view.queryByText(/exit |killed by/u)).toBeNull();
  });

  it('renders the exit-code pill for a non-zero exit and the error dot', () => {
    const view = render(<TerminalBlock command="false" output="a" exitCode={1} labels={LABELS} />);
    expect(view.getByText('exit 1')).toBeInTheDocument();
    expect(runStateOf(view.container)).toEqual({ state: 'error', label: 'Failed' });
  });

  it('renders the no-exit-code pill for a command that settled without one', () => {
    const view = render(
      <TerminalBlock command="pnpm add x" output="ENOENT" exitCode={null} labels={LABELS} />,
    );
    expect(view.getByText('no exit code')).toBeInTheDocument();
    expect(runStateOf(view.container)).toEqual({ state: 'error', label: 'Failed' });
  });

  it('renders the signal pill, which outranks the exit code', () => {
    const view = render(
      <TerminalBlock command="sleep 9" output="a" exitCode={0} signal="SIGKILL" labels={LABELS} />,
    );
    expect(view.getByText('killed by SIGKILL')).toBeInTheDocument();
    expect(view.queryByText('exit 0')).toBeNull();
  });

  it('shows the running chase while running and the done dot for a clean settle', () => {
    const view = render(<TerminalBlock command="sleep 5" running labels={LABELS} />);
    expect(runStateOf(view.container)).toEqual({ state: 'ongoing', label: 'Running' });
    view.rerender(<TerminalBlock command="true" output="a" exitCode={0} labels={LABELS} />);
    expect(runStateOf(view.container)).toEqual({ state: 'done', label: 'Done' });
  });

  it('labels only the first row with the cwd and marks the call once', () => {
    const view = render(
      <TerminalBlock command={'cd ~\nls'} cwd="/srv/app" output="a" exitCode={0} labels={LABELS} />,
    );
    expect(promptRows(view.container)).toEqual(['appcd ~', '$ls']);
    expect(view.container.querySelectorAll('[class*="runState"][data-state]')).toHaveLength(1);
  });

  it('drops a trailing newline instead of drawing an empty final row', () => {
    const view = render(<TerminalBlock command={'echo one\necho two\n'} output="a" labels={LABELS} />);
    expect(promptRows(view.container)).toEqual(['$echo one', '$echo two']);
  });
});

describe('TerminalBlock height cap', () => {
  it('renders every line and no expand control under the cap', () => {
    const view = render(<TerminalBlock command="ls" output={body(4)} maxLines={4} labels={LABELS} />);
    expect(outputLines(view.container)).toHaveLength(4);
    expect(view.container.querySelector('[aria-expanded]')).toBeNull();
  });

  it('slices head and tail over the cap and expands on click', () => {
    const view = render(<TerminalBlock command="ls" output={body(10)} maxLines={4} labels={LABELS} />);
    expect(outputLines(view.container)).toEqual(['line 1', 'line 2', 'line 9', 'line 10']);
    const toggle = view.getByRole('button', { name: 'Show 6 more lines' });
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(toggle.textContent).toBe('… 6 more lines');

    fireEvent.click(toggle);
    expect(outputLines(view.container)).toHaveLength(10);
    fireEvent.click(view.getByRole('button', { name: 'Collapse output' }));
    expect(outputLines(view.container)).toEqual(['line 1', 'line 2', 'line 9', 'line 10']);
  });

  it('caps at the documented default when maxLines is absent', () => {
    const view = render(
      <TerminalBlock command="ls" output={body(DEFAULT_TERMINAL_MAX_LINES + 1)} labels={LABELS} />,
    );
    expect(outputLines(view.container)).toHaveLength(DEFAULT_TERMINAL_MAX_LINES);
    expect(view.getByRole('button', { name: 'Show 1 more lines' })).toBeInTheDocument();
  });
});

describe('TerminalBlock copy', () => {
  it('copies the raw output — never the prompt line or the pill — and flips the label', async () => {
    vi.useFakeTimers();
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    const output = `${ESC}[31mbad${ESC}[39m\n`;
    render(<TerminalBlock command="make" cwd="/Users/me/app" output={output} exitCode={2} labels={LABELS} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith(output);
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole('button', { name: 'Copied' })).toBeInTheDocument();
    // While the ok label is showing, further clicks are no-ops.
    fireEvent.click(screen.getByRole('button', { name: 'Copied' }));
    expect(writeText).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(screen.getByRole('button', { name: 'Copy' })).toBeInTheDocument();
    vi.useRealTimers();
  });

  it('copies the whole output while the height cap hides its middle', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    const output = `${body(10)}\n`;
    render(<TerminalBlock command="ls" output={output} maxLines={4} exitCode={0} labels={LABELS} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith(output);
    expect(await screen.findByRole('button', { name: 'Copied' })).toBeInTheDocument();
  });

  it('does not claim success when the host refuses the write', async () => {
    stubClipboard(vi.fn().mockRejectedValue(new Error('denied')));
    render(<TerminalBlock command="ls" output="a" labels={LABELS} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole('button', { name: 'Copy' })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Copied' })).toBeNull();
  });

  it('keeps the copy payload overridable and the control present before any output', () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    render(<TerminalBlock command="npm run build" running copyText="npm run build" labels={LABELS} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith('npm run build');
  });
});
