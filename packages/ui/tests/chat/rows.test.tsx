// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Sources: packages/client/ui-primitives/tests/text-shimmer.client.spec.tsx and
//          packages/client/ui-primitives/tests/disclosure-row-styles.client.spec.ts
// Modified for Wing: rendered through `@wing-agent/ui`'s barrel (the consumer surface),
// the icon is this package's own, and the stylesheet assertions check the token axis
// this package supplies (styles/base.css) instead of the upstream theme sheet.

import { readFileSync } from 'node:fs';
import path from 'node:path';

import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { DisclosureRow, TextShimmer } from '../../src/index';

const ICON = <span data-testid="glyph" />;

describe('DisclosureRow', () => {
  it('renders one row header and toggles it from a click when the row expands', () => {
    const onToggle = vi.fn();
    const view = render(
      <DisclosureRow icon={ICON} title="Bash" open={false} expandable expandOnRowClick onToggle={onToggle} />,
    );
    const row = view.container.querySelector('[data-disclosure-row]');
    expect(row).not.toBeNull();
    expect(row?.getAttribute('data-expandable')).toBe('true');
    expect(row?.getAttribute('role')).toBe('button');
    expect(row?.getAttribute('tabindex')).toBe('0');
    expect(row?.getAttribute('aria-expanded')).toBe('false');
    expect(view.getByText('Bash')).toBeTruthy();

    fireEvent.click(row as Element);
    expect(onToggle).toHaveBeenCalledTimes(1);
  });

  it('toggles from Enter and Space on the row, and ignores other keys', () => {
    const onToggle = vi.fn();
    const view = render(
      <DisclosureRow icon={ICON} title="Bash" open expandable expandOnRowClick onToggle={onToggle} />,
    );
    const row = view.container.querySelector('[data-disclosure-row]') as Element;
    expect(row.getAttribute('aria-expanded')).toBe('true');

    fireEvent.keyDown(row, { key: 'Enter' });
    fireEvent.keyDown(row, { key: ' ' });
    expect(onToggle).toHaveBeenCalledTimes(2);
    fireEvent.keyDown(row, { key: 'Escape' });
    fireEvent.keyDown(row, { key: 'a' });
    expect(onToggle).toHaveBeenCalledTimes(2);

    // React 19 renders the boolean `inert` as an attribute on the decoration only.
    expect(view.container.querySelector('[inert]')).toBeNull();
  });

  it('keeps a leading toggle button when the row itself is not the target', () => {
    const onToggle = vi.fn();
    const view = render(
      <DisclosureRow icon={ICON} title="Read src/host.ts" open={false} expandable onToggle={onToggle} />,
    );
    const row = view.container.querySelector('[data-disclosure-row]');
    expect(row?.getAttribute('role')).toBeNull();
    const leading = view.getByRole('button', { name: 'Read src/host.ts' });
    expect(leading.getAttribute('aria-expanded')).toBe('false');
    fireEvent.click(leading);
    expect(onToggle).toHaveBeenCalledTimes(1);
  });

  it('renders children only while open and keeps the collapsed content policy', () => {
    const view = render(
      <DisclosureRow
        icon={ICON}
        title="Tool"
        open={false}
        expandable
        onToggle={() => {}}
        collapsedContent={<span>summary</span>}
      >
        <span>body</span>
      </DisclosureRow>,
    );
    expect(view.getByText('summary')).toBeTruthy();
    expect(view.queryByText('body')).toBeNull();
    expect(view.container.querySelector('[data-open]')).toBeNull();

    view.rerender(
      <DisclosureRow
        icon={ICON}
        title="Tool"
        open
        expandable
        onToggle={() => {}}
        collapsedContent={<span>summary</span>}
        keepContentWhenOpen
      >
        <span>body</span>
      </DisclosureRow>,
    );
    expect(view.getByText('body')).toBeTruthy();
    expect(view.getByText('summary')).toBeTruthy();
    expect(view.container.querySelector('[data-open]')).not.toBeNull();
  });

  it('shimmers the whole header while the owning operation is running', () => {
    const view = render(
      <DisclosureRow icon={ICON} title="Running" open={false} expandable onToggle={() => {}} running />,
    );
    expect(view.container.querySelectorAll('[data-shimmer="true"]')).toHaveLength(1);
    expect(view.container.querySelector('[inert]')).not.toBeNull();

    view.rerender(
      <DisclosureRow
        icon={ICON}
        title="Running"
        open={false}
        expandable
        onToggle={() => {}}
        running={false}
      />,
    );
    expect(view.container.querySelector('[data-shimmer]')).toBeNull();
    expect(view.container.querySelector('[inert]')).toBeNull();
  });
});

describe('TextShimmer', () => {
  it('retains a string child as its activity and text change', () => {
    const view = render(<TextShimmer active>Reading</TextShimmer>);
    const text = view.getByText('Reading');
    expect(view.container.textContent).toBe('Reading');
    expect(view.container.querySelectorAll('[data-shimmer="true"]')).toHaveLength(1);

    view.rerender(<TextShimmer active={false}>Read</TextShimmer>);
    expect(view.getByText('Read')).toBe(text);
    expect(view.container.querySelector('[data-shimmer]')).toBeNull();
    expect(view.container.querySelector('[inert]')).toBeNull();
  });

  it('retains text and controls while nested fragments share one inert decoration', () => {
    const open = vi.fn();
    const row = (active: boolean, title: string) => (
      <TextShimmer active={active}>
        <TextShimmer>{title}</TextShimmer>
        <button type="button" onClick={open}>
          <TextShimmer>file.ts</TextShimmer>
        </button>
      </TextShimmer>
    );
    const view = render(row(true, 'Read'));
    const title = view.getByText('Read');
    const button = view.getByRole('button', { name: 'file.ts' });
    const decoration = view.container.querySelector('[inert]') as Element;
    expect(decoration.getAttribute('aria-hidden')).toBe('true');
    expect(view.container.querySelectorAll('[inert]')).toHaveLength(1);
    expect(
      [...decoration.querySelectorAll('[data-shimmer-text]')].map((node) =>
        node.getAttribute('data-shimmer-text'),
      ),
    ).toEqual(['Read', 'file.ts']);
    expect(view.container.textContent).toBe('Readfile.ts');
    fireEvent.click(button);
    expect(open).toHaveBeenCalledOnce();

    view.rerender(row(true, 'Reading'));
    expect(view.getByText('Reading')).toBe(title);
    expect(view.getByRole('button', { name: 'file.ts' })).toBe(button);
    expect(view.container.querySelector('[inert]')).toBe(decoration);

    view.rerender(row(false, 'Read'));
    expect(view.getByText('Read')).toBe(title);
    expect(view.container.querySelector('[inert]')).toBeNull();
  });
});

// ── stylesheet contract ───────────────────────────────────────────────────

// This file runs in the jsdom project, where `import.meta.url` is a document URL —
// the package root is the vitest working directory instead.
const SRC = path.resolve(process.cwd(), 'src');
const read = (file: string): string => readFileSync(path.join(SRC, file), 'utf8');
const stripped = (file: string): string => read(file).replace(/\/\*[\s\S]*?\*\//g, ' ');

/** Declarations of one selector, as `property: value` strings. */
function declarations(css: string, selector: string): string[] {
  const rule = new RegExp(
    `(?:^|\\})\\s*${selector.replace(/[.[\]():*+^$\\]/g, '\\$&')}\\s*\\{([^{}]*)\\}`,
  ).exec(css);
  if (rule === null) throw new Error(`the sheet has no \`${selector}\` rule`);
  return (rule[1] ?? '')
    .split(';')
    .map((part) => part.trim())
    .filter(Boolean);
}

describe('DisclosureRow stylesheet follows the content-size axis', () => {
  it('sizes the title from the secondary content tier on the shared row line', () => {
    expect(declarations(stripped('chat/DisclosureRow.module.css'), '.title')).toEqual(
      expect.arrayContaining([
        'font-size: var(--dsh-content-font-size-secondary, 13px)',
        'line-height: calc(24px + var(--dsh-content-font-delta, 0px))',
      ]),
    );
  });

  it('moves the row height and leading box by the same delta', () => {
    const css = stripped('chat/DisclosureRow.module.css');
    expect(declarations(css, '.row')).toEqual(
      expect.arrayContaining(['height: calc(24px + var(--dsh-content-font-delta, 0px))']),
    );
    expect(declarations(css, '.leading')).toEqual(
      expect.arrayContaining([
        'width: calc(16px + var(--dsh-content-font-delta, 0px))',
        'height: calc(16px + var(--dsh-content-font-delta, 0px))',
      ]),
    );
  });

  it('scales leading glyphs via the svg edge but exempts StateDot', () => {
    // StateDot marks itself with data-state; the :not filter keeps the status mark at
    // its fixed size while text-furniture icons follow the text.
    expect(declarations(stripped('chat/DisclosureRow.module.css'), '.leading svg:not([data-state])')).toEqual(
      expect.arrayContaining([
        'width: calc(14px + var(--dsh-content-font-delta, 0px))',
        'height: calc(14px + var(--dsh-content-font-delta, 0px))',
      ]),
    );
  });

  it('reads only the axis tokens the token sheet defines', () => {
    const base = read('styles/base.css');
    for (const token of ['--dsh-content-font-delta:', '--dsh-content-font-size-secondary:']) {
      expect(base).toContain(token);
    }
  });
});
