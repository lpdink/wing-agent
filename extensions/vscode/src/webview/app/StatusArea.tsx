/**
 * The status area: everything the user must be able to see at a glance.
 *
 * Organised like Copilot's secondary toolbar — the control row under the input
 * (`CHAT:1668-1741`): `display: flex; gap: 2px; padding: 0 4px`, with chips shaped
 * like `.chat-input-picker-item .action-label` (`height: 16px; padding: 3px 8px;
 * border-radius: 4px; color: icon-foreground`) and a 10px chevron at `opacity: .75`.
 *
 * Every value is host data (see `src/webview/app/selectors.ts` for the two display
 * projections: the context percentage and the newest metrics cell). Clicking a chip
 * only ever sends an intent; nothing is computed here.
 */

import type { ReactElement } from 'react';

import type { SessionViewModel } from '../../shared';
import { postToHost, pingHost } from '../bridge/channel';
import { useAppStore } from '../state/appStore';
import styles from '../styles/app.module.css';
import { baseName, contextUsage, formatDuration, formatTokens, statusLabel, ttftMs } from './selectors';

/** The context ring's geometry — `chatContextUsageWidget.css:6-79` (14×14, stroke 4). */
const RING_RADIUS = 5;
const RING_CIRCUMFERENCE = 2 * Math.PI * RING_RADIUS;

export interface StatusAreaProps {
  /** `null` before the host hydrates a session — only the channel chip renders then. */
  readonly session: SessionViewModel | null;
}

export function StatusArea({ session }: StatusAreaProps): ReactElement {
  const bridge = useAppStore((state) => state.bridge);
  if (session === null) {
    return (
      <div className={styles.statusArea} data-testid="status-area">
        <span className={styles.statusSpacer} />
        <ChannelChip connection={bridge.connection} lastPongMs={bridge.lastPong?.rttMs ?? null} />
      </div>
    );
  }
  return <SessionStatus session={session} />;
}

function SessionStatus({ session }: { readonly session: SessionViewModel }): ReactElement {
  const bridge = useAppStore((state) => state.bridge);
  const { meta, context, totals } = session;
  const usage = contextUsage(context);
  const ttft = ttftMs(session);
  const workspace = baseName(meta.workspace);

  return (
    <div className={styles.statusArea} data-testid="status-area">
      {/* The status is announced once per change; the visible signal is the dot in
       * the tab bar (and the spinner state in the composer). */}
      <span className={styles.visuallyHidden} role="status" aria-live="polite" data-testid="session-status">
        {statusLabel(session.status)}
      </span>
      <button
        type="button"
        className={styles.chip}
        data-testid="model-chip"
        aria-label="Select model"
        title="Model and reasoning"
        onClick={() => postToHost({ type: 'openModelPicker', sessionId: session.sessionId })}
      >
        <span data-testid="session-model">
          {meta.model === '' ? 'No model' : meta.model}
          {meta.provider === '' ? '' : ` · ${meta.provider}`}
        </span>
        <span className={styles.chipChevron} aria-hidden="true" />
      </button>

      <button
        type="button"
        className={styles.chip}
        data-testid="thinking-chip"
        data-thinking={meta.thinking ? 'on' : 'off'}
        aria-label="Thinking and reasoning effort"
        title="Thinking and reasoning effort"
        onClick={() => postToHost({ type: 'openModelPicker', sessionId: session.sessionId })}
      >
        <span data-testid="session-thinking">{thinkingLabel(session)}</span>
        <span className={styles.chipChevron} aria-hidden="true" />
      </button>

      <button
        type="button"
        className={styles.chip}
        data-testid="yolo-chip"
        data-kind="yolo"
        data-enabled={meta.yolo ? 'true' : 'false'}
        aria-pressed={meta.yolo}
        aria-label="YOLO mode (skip tool approvals)"
        title="YOLO mode: skip dangerous-command approvals"
        onClick={() => postToHost({ type: 'setYolo', sessionId: session.sessionId, enabled: !meta.yolo })}
      >
        YOLO
      </button>

      {workspace === '' ? null : (
        <span className={styles.chipStatic} data-testid="workspace-chip" title={meta.workspace}>
          {workspace}
        </span>
      )}

      <span className={styles.statusSpacer} />

      <span
        className={styles.statusMeta}
        data-testid="session-tokens"
        title={`Input ${totals.promptTokens} tokens · output ${totals.completionTokens} · cached ${totals.cachedTokens}`}
      >
        {`↑${formatTokens(totals.promptTokens)} ↓${formatTokens(totals.completionTokens)} ⚡${formatTokens(totals.cachedTokens)}`}
      </span>
      {ttft === null ? null : (
        <span className={styles.statusMeta} data-testid="session-ttft" title="Time to first token">
          {`TTFT ${formatDuration(ttft)}`}
        </span>
      )}

      {/* Context usage: ring + numbers, thresholds 75% / 90% (chatContextUsageWidget.ts:468). */}
      <span
        className={styles.contextChip}
        data-testid="session-context"
        data-level={usage.level}
        aria-label={`Context window usage: ${Math.round(usage.percent)}%`}
        title={`${context.usedTokens} of ${context.windowTokens} tokens · ${context.messageCount} messages`}
      >
        <svg className={styles.contextRing} viewBox="0 0 14 14" aria-hidden="true">
          <circle className={styles.contextRingBg} cx="7" cy="7" r={RING_RADIUS} />
          <circle
            className={styles.contextRingArc}
            cx="7"
            cy="7"
            r={RING_RADIUS}
            strokeDasharray={RING_CIRCUMFERENCE}
            strokeDashoffset={RING_CIRCUMFERENCE * (1 - usage.percent / 100)}
          />
        </svg>
        <span>
          {usage.known
            ? `${formatTokens(context.usedTokens)} / ${formatTokens(context.windowTokens)}`
            : 'no window'}
        </span>
      </span>

      <ChannelChip connection={bridge.connection} lastPongMs={bridge.lastPong?.rttMs ?? null} />
    </div>
  );
}

/**
 * Channel diagnostics — the affordance step 01's debug footer had, kept because
 * "is the host still there?" is the one thing a user cannot otherwise see when a
 * turn never starts. Clicking measures the round-trip (`ping` / `pong`).
 */
function ChannelChip({
  connection,
  lastPongMs,
}: {
  readonly connection: 'connecting' | 'ready';
  readonly lastPongMs: number | null;
}): ReactElement {
  // The protocol version is not user-facing chrome, but it is the first thing you
  // need when a mismatch is suspected — it stays readable on the element.
  const protocolVersion = useAppStore((state) => state.bridge.protocolVersion);
  return (
    <button
      type="button"
      className={styles.chip}
      data-testid="ping-button"
      data-state={connection}
      data-protocol={protocolVersion}
      aria-label="Ping the extension host"
      title="Connection to the extension host — click to measure the round-trip"
      onClick={() => pingHost()}
    >
      <span data-testid="bridge-status" role="status" aria-live="polite">
        {connection === 'ready' ? 'ready' : 'connecting'}
      </span>
      {lastPongMs === null ? null : (
        <span className={styles.statusMeta} data-testid="ping-rtt">{`pong ${lastPongMs}ms`}</span>
      )}
    </button>
  );
}

/** `Thinking: high` / `Thinking: on` / `Thinking: off`. */
function thinkingLabel(session: SessionViewModel): string {
  if (!session.meta.thinking) {
    return 'Thinking: off';
  }
  return session.meta.reasoningEffort === '' ? 'Thinking: on' : `Thinking: ${session.meta.reasoningEffort}`;
}
