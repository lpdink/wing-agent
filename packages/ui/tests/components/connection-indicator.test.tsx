// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Source: packages/client/ui-primitives/src/ConnectionIndicator.tsx
// Modified for Wing: the cases assert the component's own contract (three states,
// the reconnect sink, the 150ms exit) — the shell owns the state mapping.

import { act, fireEvent, render } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

import { ConnectionIndicator } from '../../src/index';

const LABELS = {
  disconnectedLabel: 'Disconnected — retry',
  connectingLabel: 'Reconnecting',
  recoveredLabel: 'Connected',
  reconnectActionLabel: 'Reconnect now',
  restartActionLabel: 'Restart the attempt',
};

afterEach(() => {
  vi.useRealTimers();
});

describe('ConnectionIndicator', () => {
  it('renders nothing while the connection is fine', () => {
    const view = render(<ConnectionIndicator state={undefined} onReconnect={() => {}} {...LABELS} />);
    expect(view.container.textContent).toBe('');
  });

  it('offers a reconnect control while the connection is down', () => {
    const onReconnect = vi.fn();
    const view = render(<ConnectionIndicator state="disconnected" onReconnect={onReconnect} {...LABELS} />);
    const button = view.getByRole('button', { name: 'Reconnect now' });
    expect(button.getAttribute('data-phase')).toBe('disconnected');
    expect(view.getByText(/Disconnected/)).toBeTruthy();
    fireEvent.click(button);
    expect(onReconnect).toHaveBeenCalledOnce();
  });

  it('shows the attempt and the animated dots while reconnecting', () => {
    const view = render(<ConnectionIndicator state="connecting" onReconnect={() => {}} {...LABELS} />);
    const button = view.getByRole('button', { name: 'Restart the attempt' });
    expect(button.getAttribute('data-phase')).toBe('connecting');
    expect(button.textContent).toContain('Reconnecting');
    expect(button.querySelectorAll('[class*="dots"] > span')).toHaveLength(3);
    expect(button.querySelector('[data-state="ongoing"]')).not.toBeNull();
  });

  it('confirms the recovery with a status role and no action', () => {
    const view = render(<ConnectionIndicator state="recovered" onReconnect={() => {}} {...LABELS} />);
    const status = view.getByRole('status');
    expect(status.getAttribute('aria-label')).toBe('Connected');
    expect(view.queryByRole('button')).toBeNull();
  });

  it('keeps the indicator mounted for the exit transition before unmounting', () => {
    vi.useFakeTimers();
    const view = render(<ConnectionIndicator state="recovered" onReconnect={() => {}} {...LABELS} />);
    view.rerender(<ConnectionIndicator state={undefined} onReconnect={() => {}} {...LABELS} />);
    const leaving = view.getByRole('status');
    expect(leaving.className).toContain('leaving');

    act(() => {
      vi.advanceTimersByTime(150);
    });
    expect(view.queryByRole('status')).toBeNull();
  });

  it('restarts the exit countdown when the state comes back before it elapses', () => {
    vi.useFakeTimers();
    const view = render(<ConnectionIndicator state="connecting" onReconnect={() => {}} {...LABELS} />);
    view.rerender(<ConnectionIndicator state={undefined} onReconnect={() => {}} {...LABELS} />);
    act(() => {
      vi.advanceTimersByTime(100);
    });
    view.rerender(<ConnectionIndicator state="disconnected" onReconnect={() => {}} {...LABELS} />);
    expect(view.getByRole('button').getAttribute('data-phase')).toBe('disconnected');
    act(() => {
      vi.advanceTimersByTime(150);
    });
    expect(view.getByRole('button')).toBeTruthy();
  });
});
