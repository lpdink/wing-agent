/**
 * The root component: runtime lifecycle in, actions out.
 *
 * It owns exactly two things — starting/stopping the runtime, and turning its
 * snapshot into the props the shell renders — so the shell stays a pure function
 * of `(snapshot, actions)` and can be rendered in tests without a gateway.
 */

import { useEffect, useMemo, type ReactElement } from 'react';

import type { GatewayRuntime, Notice, RuntimeSnapshot } from '../connection/runtime';
import type { GatewaySettings } from '../settings/settings';

import { Shell } from './Shell';
import { useRuntimeSnapshot } from './useRuntime';
import { useWebBridge } from './useWebBridge';

/** Everything the shell can *do*; each entry is one runtime call. */
export interface ShellActions {
  readonly newSession: () => void;
  readonly activate: (sessionId: string) => void;
  readonly refreshSessions: () => void;
  readonly reconnect: () => void;
  readonly applySettings: (settings: GatewaySettings) => void;
  readonly dismissNotice: (id: Notice['id']) => void;
  // ── control plane (step 09) ────────────────────────────────────────
  readonly sendText: (text: string) => boolean;
  readonly interrupt: () => Promise<void>;
  readonly openModelPicker: () => Promise<void>;
  readonly openBranchesPanel: (mode: 'rewind' | 'fork') => Promise<void>;
  readonly compact: (instruction?: string | null) => Promise<void>;
  readonly closeOverlays: () => void;
  readonly fork: (sourceSessionId: string, targetUuid: string) => Promise<string | null>;
  readonly rewind: (targetUuid: string) => Promise<void>;
  readonly updateMeta: (fields: {
    model?: string;
    provider?: string;
    thinking?: boolean;
    reasoning_effort?: string;
    yolo?: boolean;
    title?: string;
    agent?: string;
    workspace?: string;
  }) => Promise<void>;
}

export interface AppProps {
  readonly runtime: GatewayRuntime;
  /** `false` when the browser refused persistent storage (forwarded to the dialog). */
  readonly settingsPersistent?: boolean;
}

export function App({ runtime, settingsPersistent = true }: AppProps): ReactElement {
  const snapshot = useRuntimeSnapshot(runtime);

  useEffect(() => {
    runtime.start();
    return () => {
      // `stop()` is the page-teardown level: socket closed, timers cancelled, the
      // records kept. `dispose()` additionally drops the notice timers and is what
      // the tests call.
      runtime.stop();
    };
  }, [runtime]);

  // The renderer's bridge (ask answers, images, links): mounted with the shell, torn
  // down with it.
  useWebBridge(runtime);

  const actions = useMemo<ShellActions>(
    () => ({
      newSession: () => {
        void runtime.newSession();
      },
      activate: (sessionId) => {
        void runtime.activate(sessionId);
      },
      refreshSessions: () => {
        void runtime.refreshSessions();
      },
      reconnect: () => {
        runtime.reconnect();
      },
      applySettings: (settings) => {
        runtime.updateSettings(settings);
      },
      dismissNotice: (id) => {
        runtime.dismissNotice(id);
      },
      sendText: (text) => runtime.sendText(text),
      interrupt: () => runtime.interrupt(),
      openModelPicker: () => runtime.openModelPicker(),
      openBranchesPanel: (mode) => runtime.openBranchesPanel(mode),
      compact: (instruction) => runtime.compact(instruction ?? null),
      closeOverlays: () => runtime.closeOverlays(),
      fork: (sourceSessionId, targetUuid) => runtime.fork(sourceSessionId, targetUuid),
      rewind: (targetUuid) => runtime.rewind(targetUuid),
      updateMeta: (fields) => runtime.updateMeta(fields),
    }),
    [runtime],
  );

  return (
    <Shell
      snapshot={snapshot}
      actions={actions}
      location={runtime.location}
      settingsPersistent={settingsPersistent}
    />
  );
}

export type { RuntimeSnapshot };
