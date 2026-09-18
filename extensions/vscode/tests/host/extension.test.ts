import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type * as vscode from 'vscode';

import { ChatViewProvider } from '../../src/host/chatViewProvider';
import { activate, deactivate } from '../../src/host/extension';
import { disposeLog } from '../../src/host/log';
import { Uri, mockState } from '../mocks/vscode';

/**
 * Activation wiring: what the extension host gets when VS Code activates the
 * extension. Everything is asserted through the `vscode` mock (no editor, no GUI).
 */

function makeContext(): vscode.ExtensionContext & { subscriptions: { dispose(): unknown }[] } {
  return {
    extensionUri: Uri.file('/extension'),
    subscriptions: [],
  } as unknown as vscode.ExtensionContext & { subscriptions: { dispose(): unknown }[] };
}

describe('activate', () => {
  beforeEach(() => {
    mockState.reset();
    disposeLog();
  });

  afterEach(() => {
    disposeLog();
  });

  it('registers the chat view provider with retained context', () => {
    const context = makeContext();

    activate(context);

    expect(mockState.viewProviders).toHaveLength(1);
    const registration = mockState.viewProviders[0];
    expect(registration?.viewId).toBe(ChatViewProvider.viewId);
    expect(registration?.provider).toBeInstanceOf(ChatViewProvider);
    expect(registration?.options).toEqual({ webviewOptions: { retainContextWhenHidden: true } });
  });

  it('puts every disposable on the extension context', () => {
    const context = makeContext();

    activate(context);

    expect(context.subscriptions).toHaveLength(2);
    expect(context.subscriptions[0]).toBeInstanceOf(ChatViewProvider);
  });

  it('logs activation to the output channel', () => {
    activate(makeContext());

    const lines = mockState.outputChannels.flatMap((channel) => channel.lines);
    expect(lines.some((line) => line.includes('Wing extension activated'))).toBe(true);
    expect(mockState.outputChannels.map((channel) => channel.name)).toEqual(['Wing']);
  });

  it('does not register anything twice when deactivate is called', () => {
    const context = makeContext();
    activate(context);

    expect(() => deactivate()).not.toThrow();
    expect(mockState.viewProviders).toHaveLength(1);
  });
});
