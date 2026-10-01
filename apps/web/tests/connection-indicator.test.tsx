import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { App } from '../src/app/App';
import { ConnectionStatus, RECOVERED_MS } from '../src/app/ConnectionStatus';
import { GatewayRuntime, type ConnectionView } from '../src/connection/runtime';
import { DEFAULT_SETTINGS } from '../src/settings/settings';

import { FakeGateway } from './support/fake-gateway';
import { TEST_LOCATION } from './support/harness';

/**
 * The connection indicator in the shell (step 08b).
 *
 * `ConnectionStatus` is the mapping layer: the runtime's four-phase
 * `ConnectionView` in, the ported card's three states out, plus the one piece of
 * memory the raw phase lacks (a link that came *back* is a recovery; a first connect
 * is not). The last test drives the real runtime through a failing dial, which is
 * what proves the mapping is wired to the shell rather than to a fixture.
 */

function view(overrides: Partial<ConnectionView> = {}): ConnectionView {
  return {
    phase: 'connected',
    clientId: 'client-1',
    everConnected: true,
    attempt: 0,
    reconnectInMs: null,
    lastError: null,
    address: 'http://localhost:5173 (this page)',
    unauthorized: false,
    ...overrides,
  };
}

describe('connection indicator', () => {
  it('stays quiet while the connection is healthy', () => {
    const { container } = render(<ConnectionStatus view={view()} onReconnect={vi.fn()} />);

    expect(container.textContent).toBe('');
  });

  it('shows the first dial as connecting, and retries on click', () => {
    const onReconnect = vi.fn();
    render(
      <ConnectionStatus
        view={view({ phase: 'connecting', clientId: null, everConnected: false })}
        onReconnect={onReconnect}
      />,
    );

    const indicator = screen.getByRole('button', { name: 'Restart the connection attempt' });
    expect(indicator.textContent).toContain('Connecting');
    fireEvent.click(indicator);
    expect(onReconnect).toHaveBeenCalledTimes(1);
  });

  it('folds the retry countdown into the reconnecting label', () => {
    render(
      <ConnectionStatus
        view={view({ phase: 'reconnecting', clientId: null, reconnectInMs: 3_900 })}
        onReconnect={vi.fn()}
      />,
    );

    expect(screen.getByRole('button', { name: 'Restart the connection attempt' }).textContent).toContain(
      'Reconnecting in 4s',
    );
  });

  it('shows an outage after a connection existed, and stays quiet before one', () => {
    const onReconnect = vi.fn();
    const { unmount } = render(
      <ConnectionStatus view={view({ phase: 'offline', clientId: null })} onReconnect={onReconnect} />,
    );
    const indicator = screen.getByRole('button', { name: 'Reconnect now' });
    expect(indicator.textContent).toContain('Disconnected');
    fireEvent.click(indicator);
    expect(onReconnect).toHaveBeenCalledTimes(1);
    unmount();

    // Never connected and nothing running: the banner explains it, the indicator has
    // nothing to add (running `connecting` is not an outage).
    const quiet = render(
      <ConnectionStatus
        view={view({ phase: 'offline', clientId: null, everConnected: false })}
        onReconnect={vi.fn()}
      />,
    );
    expect(quiet.container.textContent).toBe('');
  });

  it('hides itself when the gateway rejected the credentials', () => {
    const { container } = render(
      <ConnectionStatus view={view({ phase: 'offline', unauthorized: true })} onReconnect={vi.fn()} />,
    );

    // Retrying is pointless; the banner names the problem and offers the settings.
    expect(container.textContent).toBe('');
  });

  it('confirms a recovery, then goes quiet again', async () => {
    vi.useFakeTimers();
    try {
      const onReconnect = vi.fn();
      const app = render(
        <ConnectionStatus view={view({ phase: 'reconnecting' })} onReconnect={onReconnect} />,
      );
      expect(screen.getByRole('button', { name: 'Restart the connection attempt' })).toBeTruthy();

      app.rerender(<ConnectionStatus view={view({ phase: 'connected' })} onReconnect={onReconnect} />);
      expect(screen.getByRole('status').textContent).toContain('Reconnected');

      await act(async () => {
        await vi.advanceTimersByTimeAsync(RECOVERED_MS + 50);
      });
      // …plus the card's own exit transition, scheduled by the render the window's
      // expiry produces.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(200);
      });
      expect(app.container.textContent).toBe('');
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('the shell’s indicator', () => {
  let runtime: GatewayRuntime | null = null;

  beforeEach(() => {
    runtime = null;
  });

  afterEach(() => {
    runtime?.stop();
  });

  it('appears when the gateway cannot be reached, and reconnects on click', async () => {
    const gateway = new FakeGateway({ failDials: 999 });
    runtime = new GatewayRuntime({
      initialSettings: DEFAULT_SETTINGS,
      location: TEST_LOCATION,
      socketFactory: gateway.socketFactory,
      httpTransport: gateway.transport,
      listPollIntervalMs: 0,
      noticeTtlMs: 0,
      connectRetryBaseMs: 3_600_000,
    });
    render(<App runtime={runtime} />);

    const indicator = await screen.findByRole('button', { name: 'Restart the connection attempt' });
    expect(indicator.textContent).toContain('Connecting');
    const dials = gateway.sockets.length;

    // Re-query: the runtime re-renders the shell as its state machine moves, and a
    // node captured before the last transition is no longer in the document.
    fireEvent.click(screen.getByRole('button', { name: 'Restart the connection attempt' }));

    // A fresh dial is what "reconnect" *is* on this seam (the banner's own button
    // goes through the same action).
    await waitFor(() => {
      expect(gateway.sockets.length).toBeGreaterThan(dials);
    });
  });
});
