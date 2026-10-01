import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { App } from '../src/app/App';
import { GatewayRuntime } from '../src/connection/runtime';
import { DEFAULT_SETTINGS, type GatewaySettings } from '../src/settings/settings';

import { FakeGateway, makeSession } from './support/fake-gateway';
import { TEST_LOCATION } from './support/harness';

/**
 * The shell, rendered for real (jsdom) against the fake gateway.
 *
 * These are the acceptance-level assertions of this step: the first-connect guide
 * appears instead of a blank page, the session list is usable (list / create /
 * switch), the settings form applies and validates, and the main pane shows real
 * session state. Everything else is covered by the framework-free tests.
 */

function buildRuntime(
  gateway: FakeGateway,
  settings: Partial<GatewaySettings> = {},
  extra: { readonly connectRetryBaseMs?: number } = {},
): GatewayRuntime {
  return new GatewayRuntime({
    initialSettings: { ...DEFAULT_SETTINGS, ...settings },
    location: TEST_LOCATION,
    socketFactory: gateway.socketFactory,
    httpTransport: gateway.transport,
    listPollIntervalMs: 0,
    noticeTtlMs: 0,
    connectRetryBaseMs: extra.connectRetryBaseMs ?? 3_600_000,
  });
}

const FIRST = makeSession({
  id: 'session-1',
  name: 'Fix the parser',
  lastInteraction: '2026-10-01T12:00:00Z',
});
const SECOND = makeSession({
  id: 'session-2',
  name: 'Deploy check',
  lastInteraction: '2026-10-01T11:00:00Z',
});

describe('web shell', () => {
  it('guides to the settings form when the gateway cannot be reached', async () => {
    const gateway = new FakeGateway({ failDials: 999 });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);

    const banner = await screen.findByRole('alert');
    expect(banner.textContent).toContain('Cannot reach the gateway');
    expect(banner.textContent).toContain('connection refused');

    fireEvent.click(screen.getByRole('button', { name: 'Set the gateway address' }));
    const dialog = await screen.findByRole('dialog', { name: 'Gateway settings' });
    expect(within(dialog).getByLabelText('Scheme')).toBeTruthy();
    expect(within(dialog).getByLabelText('Host')).toBeTruthy();
    expect(within(dialog).getByLabelText('Port')).toBeTruthy();
    expect(within(dialog).getByLabelText('API key')).toBeTruthy();
    expect(within(dialog).getByLabelText('Ignore certificate errors')).toBeTruthy();
    // The preview shows what the current (same-origin) settings resolve to, and the
    // scheme / port fields are inert while the host is empty.
    expect(dialog.textContent).toContain('http://localhost:5173');
    expect(within(dialog).getByLabelText<HTMLInputElement>('Port').disabled).toBe(true);
    expect(within(dialog).getByLabelText<HTMLSelectElement>('Scheme').disabled).toBe(true);

    fireEvent.change(within(dialog).getByLabelText('Host'), { target: { value: 'gc.lan' } });
    expect(within(dialog).getByLabelText<HTMLInputElement>('Port').disabled).toBe(false);
    runtime.stop();
  });

  it('lists the sessions and opens the one that is clicked', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST, SECOND] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);

    const first = await screen.findByRole('button', { name: /Fix the parser/ });
    expect(await screen.findByRole('button', { name: /Deploy check/ })).toBeTruthy();
    await waitFor(() => {
      expect(first.getAttribute('aria-current')).toBe('true');
    });

    fireEvent.click(screen.getByRole('button', { name: /Deploy check/ }));

    await waitFor(() => {
      expect(
        gateway.callsTo('POST', '/api/session/subscribe').map((call) => call.body?.['session_id']),
      ).toEqual(['session-1', 'session-2']);
    });
    await waitFor(() => {
      expect(screen.getByRole('button', { name: /Deploy check/ }).getAttribute('aria-current')).toBe('true');
    });

    runtime.stop();
  });

  it('creates a session from the empty state', async () => {
    const gateway = new FakeGateway();
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);

    const main = screen.getByRole('main');
    expect(await within(main).findByText('No session open')).toBeTruthy();
    fireEvent.click(within(main).getByRole('button', { name: 'New session' }));

    await waitFor(() => {
      expect(gateway.callsTo('POST', '/api/session/create')).toHaveLength(1);
    });

    // The row appears and is current; the pane shows the new session's id. (The
    // row title is the workspace basename until the model names the session —
    // the same derivation the TUI and the extension use.)
    const sidebar = screen.getByRole('complementary', { name: 'Sessions' });
    await waitFor(() => {
      const rows = within(sidebar).getAllByRole('button');
      expect(rows.some((row) => row.getAttribute('aria-current') === 'true')).toBe(true);
    });
    expect(within(main).getByText('created-1')).toBeTruthy();

    runtime.stop();
  });

  it('shows the open session’s live state in the main pane', async () => {
    const session = makeSession({ id: 'session-1', name: 'Fix the parser', workspace: '/srv/app' });
    session.runtime = {
      ...session.runtime,
      model: 'glm-4.6',
      thinking: true,
      reasoning_effort: 'high',
      yolo: true,
      workdir: '/srv/app',
    };
    const gateway = new FakeGateway({ sessions: [session] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);

    const main = screen.getByRole('main');
    await waitFor(() => {
      expect(within(main).getByText('glm-4.6')).toBeTruthy();
    });
    expect(within(main).getByText('/srv/app')).toBeTruthy();
    expect(within(main).getByText('on (high)')).toBeTruthy();
    expect(within(main).getByText('session-1')).toBeTruthy();

    runtime.stop();
  });

  it('applies the settings from the dialog and closes it', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);
    await screen.findByRole('button', { name: /Fix the parser/ });

    fireEvent.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Gateway settings' });
    fireEvent.change(within(dialog).getByLabelText('Host'), { target: { value: 'gateway.lan' } });
    fireEvent.change(within(dialog).getByLabelText('Port'), { target: { value: '8443' } });
    fireEvent.change(within(dialog).getByLabelText('Scheme'), { target: { value: 'https' } });
    expect(dialog.textContent).toContain('https://gateway.lan:8443');

    fireEvent.click(within(dialog).getByRole('button', { name: /Save/ }));

    await waitFor(() => {
      expect(runtime.getSnapshot().settings).toEqual({
        scheme: 'https',
        host: 'gateway.lan',
        port: 8443,
        apiKey: null,
        ignoreCertErrors: false,
      });
    });
    expect(screen.queryByRole('dialog', { name: 'Gateway settings' })).toBeNull();

    runtime.stop();
  });

  it('refuses an unusable gateway address instead of writing it', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST] });
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);
    await screen.findByRole('button', { name: /Fix the parser/ });

    fireEvent.click(screen.getByRole('button', { name: 'Settings' }));
    const dialog = await screen.findByRole('dialog', { name: 'Gateway settings' });
    const host = within(dialog).getByLabelText('Host');
    fireEvent.change(host, { target: { value: 'localhost:8080' } });

    await waitFor(() => {
      expect(dialog.textContent).toContain('contains a port');
    });
    expect(within(dialog).getByRole('button', { name: /Save/ }).hasAttribute('disabled')).toBe(true);
    expect(runtime.getSnapshot().settings.host).toBe('');

    runtime.stop();
  });

  it('renders notices and dismisses them', async () => {
    const gateway = new FakeGateway({ sessions: [FIRST] });
    gateway.missing.add('session-1');
    const runtime = buildRuntime(gateway);
    render(<App runtime={runtime} />);

    const notice = await screen.findByText(/Could not open that session/);
    expect(notice).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    await waitFor(() => {
      expect(screen.queryByText(/Could not open that session/)).toBeNull();
    });

    runtime.stop();
  });
});
