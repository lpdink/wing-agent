// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/tests/atoms.client.spec.tsx (button cases)
// Modified for Wing: rendered through this package's barrel; the ref case covers the
// React 19 `forwardRef` shape the port keeps.

import { createRef } from 'react';

import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { Button } from '../../src/index';

describe('Button', () => {
  it('renders a native button with the ghost/md defaults', () => {
    const view = render(<Button>Run</Button>);
    const button = view.getByRole('button', { name: 'Run' });
    expect(button.getAttribute('type')).toBe('button');
    expect(button.className).toContain('ghost');
    expect(button.className).toContain('md');
    expect(button).toHaveProperty('disabled', false);
  });

  it('maps variants and sizes to their classes', () => {
    const view = render(
      <>
        <Button variant="primary" size="sm">
          Yes
        </Button>
        <Button variant="outline">Maybe</Button>
        <Button variant="toolbar">Toolbar</Button>
      </>,
    );
    expect(view.getByRole('button', { name: 'Yes' }).className).toContain('primary');
    expect(view.getByRole('button', { name: 'Yes' }).className).toContain('sm');
    expect(view.getByRole('button', { name: 'Maybe' }).className).toContain('outline');
    expect(view.getByRole('button', { name: 'Toolbar' }).className).toContain('toolbar');
  });

  it('keeps native attributes, the disabled state and the click handler', () => {
    const onClick = vi.fn();
    const view = render(
      <>
        <Button onClick={onClick} disabled>
          Off
        </Button>
        <Button onClick={onClick}>On</Button>
      </>,
    );
    fireEvent.click(view.getByRole('button', { name: 'Off' }));
    expect(onClick).not.toHaveBeenCalled();
    fireEvent.click(view.getByRole('button', { name: 'On' }));
    expect(onClick).toHaveBeenCalledOnce();
  });

  it('wraps a leading icon and forwards the ref', () => {
    const ref = createRef<HTMLButtonElement>();
    const view = render(
      <Button ref={ref} icon={<span data-testid="glyph" />}>
        With icon
      </Button>,
    );
    expect(view.getByTestId('glyph')).toBeTruthy();
    expect(ref.current).toBe(view.getByRole('button', { name: 'With icon' }));
  });
});
