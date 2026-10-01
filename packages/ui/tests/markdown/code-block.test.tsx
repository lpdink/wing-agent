// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/tests/code-block.client.spec.tsx and
//         packages/client/ui-primitives/tests/code-card-controls.client.spec.tsx
// Modified for Wing: the streaming-highlight cases are not ported (the streaming
// session itself is not), so the block asserts the settled/plain arms here; the
// clipboard cases use this package's browser clipboard helper.

import { act, fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { CodeBlock } from '../../src/markdown/CodeBlock';
import type { CodeToolbarLabels } from '../../src/markdown/CodeToolbar';

const TOOLBAR: CodeToolbarLabels = {
  codeLabel: 'Code',
  wrapLabel: 'Wrap lines',
  unwrapLabel: 'Do not wrap',
};

function stubClipboard(writeText: (text: string) => Promise<void>): void {
  Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
}

describe('CodeBlock body', () => {
  it('highlights a settled fence into the shiki css-variables tree', () => {
    const { container } = render(
      <CodeBlock code={'const answer: number = 42;\n'} lang="ts" copyLabel="Copy" copiedLabel="Copied" />,
    );
    const pre = container.querySelector('pre.shiki');
    expect(pre).not.toBeNull();
    expect(pre?.textContent).toBe('const answer: number = 42;');
    // Colours resolve through the sheet's variables, never inline literals.
    expect(container.innerHTML).toContain('var(--shiki-token-');
  });

  it('renders plain text for an unknown language, keeping the line structure', () => {
    const { container } = render(
      <CodeBlock code={'a\nb\n'} lang="nosuchlang" copyLabel="Copy" copiedLabel="Copied" />,
    );
    expect(container.querySelector('pre.shiki')).toBeNull();
    expect(container.querySelector('[class*="plain"]')?.textContent).toBe('a\nb');
  });

  it('renders plain text while the fence is still streaming', () => {
    const { container } = render(
      <CodeBlock code="const x = " lang="ts" streaming copyLabel="Copy" copiedLabel="Copied" />,
    );
    expect(container.querySelector('pre.shiki')).toBeNull();
    expect(container.querySelector('[class*="plain"]')?.textContent).toBe('const x = ');
  });

  it('adds the numbered gutter and its width variable on request', () => {
    const { container } = render(
      <CodeBlock code={'a\nb'} lang="ts" lineNumbers copyLabel="Copy" copiedLabel="Copied" />,
    );
    const root = container.firstElementChild as HTMLElement;
    expect(root.getAttribute('data-line-numbers')).toBe('true');
    expect(root.style.getPropertyValue('--dsl-code-block-line-number-width')).toBe('2ch');
  });

  it('omits the header when the caller supplies its own chrome', () => {
    const { container } = render(
      <CodeBlock code="x" showHeader={false} copyLabel="Copy" copiedLabel="Copied" />,
    );
    expect(container.querySelector('[data-code-block-banner]')).toBeNull();
    expect(container.querySelector('[data-code-block-content]')).not.toBeNull();
  });
});

describe('CodeBlock toolbar', () => {
  it('labels the fence with its language and falls back to the code label', () => {
    const view = render(
      <CodeBlock code="x = 1" lang="python" copyLabel="Copy" copiedLabel="Copied" toolbarLabels={TOOLBAR} />,
    );
    expect(view.getByText('python')).toBeInTheDocument();
    view.rerender(
      <CodeBlock code="x" lang="" copyLabel="Copy" copiedLabel="Copied" toolbarLabels={TOOLBAR} />,
    );
    expect(view.getByText('Code')).toBeInTheDocument();
  });

  it('toggles the wrap attribute the stylesheet keys on', () => {
    const { container } = render(
      <CodeBlock code="x" lang="ts" copyLabel="Copy" copiedLabel="Copied" toolbarLabels={TOOLBAR} />,
    );
    const root = container.firstElementChild;
    expect(root?.getAttribute('data-code-wrap')).toBe('true');
    const button = screen.getByRole('button', { name: 'Wrap lines' });
    expect(button.getAttribute('aria-pressed')).toBe('true');
    // The accessible name is the control's ("wrap lines"); the title states the
    // action it will take, which is what the label pair is for.
    expect(button.getAttribute('title')).toBe('Do not wrap');
    fireEvent.click(button);
    expect(root?.getAttribute('data-code-wrap')).toBe('false');
    expect(button.getAttribute('aria-pressed')).toBe('false');
    expect(button.getAttribute('title')).toBe('Wrap lines');
  });

  it('honours a caller-owned wrap preference by dropping the toolbar action', () => {
    render(
      <CodeBlock
        code="x"
        lang="ts"
        wrap={false}
        copyLabel="Copy"
        copiedLabel="Copied"
        toolbarLabels={TOOLBAR}
      />,
    );
    expect(screen.queryByRole('button', { name: 'Wrap lines' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Do not wrap' })).toBeNull();
  });

  it('copies the rendered source and flips the label, ignoring a second click', async () => {
    vi.useFakeTimers();
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    render(
      <CodeBlock
        code={'const a = 1;\n'}
        lang="ts"
        copyLabel="Copy"
        copiedLabel="Copied"
        toolbarLabels={TOOLBAR}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith('const a = 1;');
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole('button', { name: 'Copied' })).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Copied' }));
    expect(writeText).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    expect(screen.getByRole('button', { name: 'Copy' })).toBeInTheDocument();
    vi.useRealTimers();
  });

  it('does not claim success when the host refuses the write', async () => {
    stubClipboard(vi.fn().mockRejectedValue(new Error('denied')));
    render(<CodeBlock code="x" lang="ts" copyLabel="Copy" copiedLabel="Copied" toolbarLabels={TOOLBAR} />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.queryByRole('button', { name: 'Copied' })).toBeNull();
  });

  it('keeps the plain (toolbar-less) header copy control working', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    stubClipboard(writeText);
    render(<CodeBlock code="echo hi" lang="bash" copyLabel="Copy" copiedLabel="Copied" />);
    fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
    expect(writeText).toHaveBeenCalledWith('echo hi');
    expect(await screen.findByRole('button', { name: 'Copied' })).toBeInTheDocument();
  });
});
