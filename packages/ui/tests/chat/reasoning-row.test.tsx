// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-chat/tests/reasoning-row.client.spec.tsx
// Modified for Wing: the slot contract (`useDisclosure` / `usePresentation`) is
// replaced by this package's props (`collapseKey` / `previewEnabled`), the row starts
// open while streaming (the thinking cell's rule) and the expanded body renders the
// package's own markdown stream.

import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { latestCompletedParagraphFirstLine, ReasoningRow, resetCollapseOverrides } from '../../src/index';

describe('latestCompletedParagraphFirstLine', () => {
  // Ported table (upstream spec): only a paragraph terminated by a newline counts as
  // complete, so a streaming tail never previews half a sentence.
  it.each([
    ['', ''],
    ['An unfinished line', ''],
    ['A completed first line\nUnfinished continuation', 'A completed first line'],
    ['First\nSecond\nThird\n', 'First'],
    ['First\n\nNext unfinished', 'First'],
    ['First\n\nNext complete\nDetails', 'Next complete'],
    ['First\n\n\nNext complete\n', 'Next complete'],
    ['First\n \n\t\nNext complete\nDetails', 'Next complete'],
    ['First\r\n\r\n\r\nNext complete\r\n', 'Next complete'],
    ['First\r\n \r\n\t\r\nNext complete\r\nDetails', 'Next complete'],
    ['First\n\n\nNext unfinished', 'First'],
    ['\n\n\nFirst complete\n', 'First complete'],
    ['First\r\n \t\r\nNext complete\r\n', 'Next complete'],
    ['First\n\n\n', 'First'],
  ])('previews the completed paragraph first line for %j', (text, summary) => {
    expect(latestCompletedParagraphFirstLine(text)).toBe(summary);
  });
});

describe('ReasoningRow', () => {
  it('starts open while streaming and shows the reasoning body', () => {
    const view = render(<ReasoningRow text={'Inspecting the session\nMore detail'} streaming />);
    const root = view.container.querySelector('[data-variant="think"]');
    expect(root?.getAttribute('data-state')).toBe('running');
    expect(root?.hasAttribute('data-expanded')).toBe(true);
    expect(view.getByRole('button').getAttribute('aria-expanded')).toBe('true');
    expect(view.getByText('Thinking', { selector: '[class*="title"]' })).toBeTruthy();
    expect(view.getByText(/More detail/)).toBeTruthy();
  });

  it('collapses to one line once settled, with the duration title and the first-line preview', () => {
    const view = render(
      <ReasoningRow text={'First line\nSecond line'} streaming={false} durationMs={2400} />,
    );
    const root = view.container.querySelector('[data-variant="think"]');
    expect(root?.getAttribute('data-state')).toBe('ok');
    expect(root?.hasAttribute('data-expanded')).toBe(false);
    expect(root?.hasAttribute('data-preview')).toBe(true);
    expect(view.getByText('Thought for 2.4s')).toBeTruthy();
    expect(view.getByText('First line')).toBeTruthy();
    expect(view.queryByText(/Second line/)).toBeNull();
  });

  it('shows the settled title without a duration and can suppress the preview', () => {
    const view = render(
      <ReasoningRow text={'Only line'} streaming={false} durationMs={null} previewEnabled={false} />,
    );
    expect(view.getByText('Thought')).toBeTruthy();
    // The summary node stays mounted (collapsing toggles CSS display, not the tree);
    // `data-preview` is what decides whether it paints.
    expect(view.container.querySelector('[data-preview]')).toBeNull();
    expect(view.container.querySelector('[class*="summaryText"]')?.textContent).toBe('Only line');
  });

  it('keeps the preview visible in the collapsed streaming state and drops it when expanded', () => {
    const text = 'First paragraph\n\nNewest completed line\n';
    const view = render(<ReasoningRow text={text} streaming />);
    const root = view.container.querySelector('[data-variant="think"]') as Element;
    const toggle = view.getByRole('button');

    // Streaming starts open: the preview is suppressed.
    expect(root.hasAttribute('data-preview')).toBe(false);

    fireEvent.click(toggle);
    expect(root.hasAttribute('data-preview')).toBe(true);
    expect(view.getByText('Newest completed line').closest('[data-streaming]')).not.toBeNull();
    // The running header is rendered twice (the inert shimmer decoration); only the
    // interactive copy carries text.
    expect(
      [...view.container.querySelectorAll('[class*="summaryText"]')].filter(
        (node) => node.textContent !== '',
      ),
    ).toHaveLength(1);

    fireEvent.click(toggle);
    expect(root.hasAttribute('data-preview')).toBe(false);
    expect(view.getByText(/Newest completed line/)).toBeTruthy();
  });

  it('strips double-asterisk markers from the collapsed summary', () => {
    const view = render(
      <ReasoningRow text={'**Comparing checkout and merge bases**\nKeep reviewing'} streaming={false} />,
    );
    expect(view.getByText('Comparing checkout and merge bases')).toBeTruthy();
    expect(view.queryByText('**Comparing checkout and merge bases**')).toBeNull();
  });

  it('remembers the reader choice across mounts only when a collapseKey is given', () => {
    const first = render(<ReasoningRow text="a\nb" streaming={false} collapseKey="cell-1" />);
    fireEvent.click(first.getByRole('button'));
    expect(first.getByRole('button').getAttribute('aria-expanded')).toBe('true');
    first.unmount();

    const again = render(<ReasoningRow text="a\nb" streaming={false} collapseKey="cell-1" />);
    expect(again.getByRole('button').getAttribute('aria-expanded')).toBe('true');
    again.unmount();

    // The same row without a key is local state: a fresh mount collapses again.
    resetCollapseOverrides();
    const localFirst = render(<ReasoningRow text="a\nb" streaming={false} />);
    fireEvent.click(localFirst.getByRole('button'));
    expect(localFirst.getByRole('button').getAttribute('aria-expanded')).toBe('true');
    localFirst.unmount();
    const localAgain = render(<ReasoningRow text="a\nb" streaming={false} />);
    expect(localAgain.getByRole('button').getAttribute('aria-expanded')).toBe('false');
  });

  it('accepts label overrides', () => {
    const view = render(
      <ReasoningRow
        text="line"
        streaming={false}
        labels={{ running: 'Ruminating', settled: () => 'Done ruminating', announcement: 'Ruminating' }}
      />,
    );
    expect(view.getByText('Done ruminating')).toBeTruthy();
  });

  it('announces the running state to assistive tech only while streaming', () => {
    const view = render(<ReasoningRow text="line" streaming />);
    expect(view.container.querySelector('[class*="visuallyHidden"]')?.textContent).toBe('Running');
    view.rerender(<ReasoningRow text="line" streaming={false} />);
    expect(view.container.querySelector('[class*="visuallyHidden"]')).toBeNull();
  });
});
