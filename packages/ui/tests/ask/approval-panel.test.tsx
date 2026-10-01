// Portions from DeepSeek Harness (https://github.com/deepseek-ai/deepseek-harness),
// Copyright (c) 2026 DeepSeek, licensed under the MIT License (see THIRD_PARTY_NOTICES.md).
// Sources: packages/client/ui-approval/tests/* (the approval flow assertions) and
//          packages/client/ui-approval/src/client/ApprovalPanel.tsx
// Modified for Wing: the carrier/promise contract is replaced by props — decisions are
// observed through `onDecide` and the lifecycle through `state` — so the cases assert
// the panel's own keyboard policy rather than an answer round trip.

import { fireEvent, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import { ApprovalPanel } from '../../src/index';

function focusBody(view: ReturnType<typeof render>): HTMLElement {
  const body = view.container.querySelector('[data-approval-scroll]') as HTMLElement;
  body.focus();
  expect(document.activeElement).toBe(body);
  return body;
}

describe('ApprovalPanel', () => {
  it('renders the waiting strip, the escalation headline and the tool detail', () => {
    const view = render(
      <ApprovalPanel
        requestId="toolu_1"
        toolName="Bash"
        detail={<code>rm -rf /tmp/x</code>}
        state="awaiting"
        onDecide={() => {}}
      />,
    );
    expect(view.getByText('Waiting for approval')).toBeTruthy();
    expect(view.getByText('Tool Bash requests privileged execution')).toBeTruthy();
    expect(view.getByText('rm -rf /tmp/x')).toBeTruthy();
    expect(view.getByRole('button', { name: 'Allow once' })).toBeTruthy();
    expect(view.getByRole('button', { name: 'Reject' })).toBeTruthy();
    expect(view.container.querySelector('[data-approval-key="toolu_1"]')).not.toBeNull();
  });

  it('prefers a host-provided reason over the escalation headline', () => {
    const view = render(
      <ApprovalPanel
        requestId="r"
        toolName="Bash"
        reason="Writes outside the workspace"
        state="awaiting"
        onDecide={() => {}}
      />,
    );
    expect(view.getByText('Writes outside the workspace')).toBeTruthy();
    expect(view.queryByText('Tool Bash requests privileged execution')).toBeNull();
  });

  it('reports the decision from both buttons', () => {
    const onDecide = vi.fn();
    const view = render(<ApprovalPanel requestId="r" toolName="Bash" state="awaiting" onDecide={onDecide} />);
    fireEvent.click(view.getByRole('button', { name: 'Allow once' }));
    fireEvent.click(view.getByRole('button', { name: 'Reject' }));
    expect(onDecide.mock.calls).toEqual([['approve'], ['deny']]);
  });

  it('approves on Enter and denies on Escape while focus is inside the card', () => {
    const onDecide = vi.fn();
    const view = render(<ApprovalPanel requestId="r" toolName="Bash" state="awaiting" onDecide={onDecide} />);
    const body = focusBody(view);
    fireEvent.keyDown(body, { key: 'Enter' });
    fireEvent.keyDown(body, { key: 'Escape' });
    expect(onDecide.mock.calls).toEqual([['approve'], ['deny']]);
  });

  it('does not hijack Enter pressed on a button, and ignores modifiers', () => {
    const onDecide = vi.fn();
    const view = render(<ApprovalPanel requestId="r" toolName="Bash" state="awaiting" onDecide={onDecide} />);
    const approve = view.getByRole('button', { name: 'Allow once' });
    approve.focus();
    fireEvent.keyDown(approve, { key: 'Enter' });
    expect(onDecide).not.toHaveBeenCalled();

    const body = focusBody(view);
    fireEvent.keyDown(body, { key: 'Enter', ctrlKey: true });
    fireEvent.keyDown(body, { key: 'Enter', shiftKey: true });
    expect(onDecide).not.toHaveBeenCalled();
  });

  it('protects an active IME composition from deciding the request', () => {
    const onDecide = vi.fn();
    const view = render(<ApprovalPanel requestId="r" toolName="Bash" state="awaiting" onDecide={onDecide} />);
    const root = view.container.querySelector('[data-approval-key]') as HTMLElement;
    const body = focusBody(view);

    fireEvent.compositionStart(root);
    fireEvent.keyDown(body, { key: 'Enter' });
    expect(onDecide).not.toHaveBeenCalled();

    // The guard also covers the keystroke that ends the composition, until keyup.
    fireEvent.compositionEnd(root);
    fireEvent.keyDown(body, { key: 'Enter' });
    expect(onDecide).not.toHaveBeenCalled();

    fireEvent.keyUp(root);
    fireEvent.keyDown(body, { key: 'Enter' });
    expect(onDecide.mock.calls).toEqual([['approve']]);
  });

  it('locks once the host settled the request and says so in the strip', () => {
    const onDecide = vi.fn();
    const view = render(<ApprovalPanel requestId="r" toolName="Bash" state="answered" onDecide={onDecide} />);
    expect(view.getByText('Decision sent')).toBeTruthy();
    expect(view.getByRole('button', { name: 'Allow once' })).toHaveProperty('disabled', true);
    expect(view.getByRole('button', { name: 'Reject' })).toHaveProperty('disabled', true);
    const body = focusBody(view);
    fireEvent.keyDown(body, { key: 'Enter' });
    fireEvent.keyDown(body, { key: 'Escape' });
    expect(onDecide).not.toHaveBeenCalled();
    expect(view.container.querySelector('[data-approval-key]')?.getAttribute('aria-busy')).toBe('true');
  });

  it('shows the cancelled lifecycle as idle rather than waiting', () => {
    const view = render(
      <ApprovalPanel requestId="r" toolName="Bash" state="cancelled" onDecide={() => {}} />,
    );
    expect(view.getByText('Decision sent')).toBeTruthy();
    expect(view.getByRole('button', { name: 'Allow once' })).toHaveProperty('disabled', true);
  });

  it('accepts copy overrides', () => {
    const view = render(
      <ApprovalPanel
        requestId="r"
        toolName="Bash"
        state="awaiting"
        onDecide={() => {}}
        labels={{
          waiting: 'Approval pending',
          escalation: (tool) => `${tool} asks`,
          settled: 'Sent',
          approve: 'Yes',
          deny: 'No',
          detailAria: 'Command',
        }}
      />,
    );
    expect(view.getByText('Approval pending')).toBeTruthy();
    expect(view.getByText('Bash asks')).toBeTruthy();
    expect(view.getByRole('button', { name: 'Yes' })).toBeTruthy();
    expect(view.getByRole('button', { name: 'No' })).toBeTruthy();
    expect(view.container.querySelector('[aria-label="Command"]')).not.toBeNull();
  });
});
