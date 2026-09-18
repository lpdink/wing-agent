import { describe, expect, it } from 'vitest';

import {
  CHUNK_TYPE,
  COMPACT_HTTP_TIMEOUT_MS,
  ChunkReassembler,
  DEFAULT_HTTP_TIMEOUT_MS,
  DEFAULT_RECONNECT_OPTIONS,
  GatewayConnection,
  GatewayHttpClient,
  GatewayHttpError,
  GatewaySocketError,
  WS_CLOSE_UNAUTHORIZED,
  consoleLogger,
  createClientRequest,
  createFetchTransport,
  createNativeSocketFactory,
  decodeWingEvent,
  gatewayUrls,
  isKnownEvent,
  isKnownEventType,
  newRequestId,
  normalizeApiKey,
  reconnectDelayMs,
  redactUrl,
  silentLogger,
} from '../../src/core';
import type {
  AgentInfo,
  ClientRequest,
  ConnectionState,
  CoreLogger,
  KnownWingEvent,
  SessionMessage,
  SessionStatus,
  SocketFactory,
  UpdateSessionRequest,
  WingEvent,
} from '../../src/core';

/**
 * The barrel is a contract: 03 imports from `src/core` and nowhere deeper, so a
 * name that disappears (or a `export *` collision that silently drops one) must
 * fail here rather than in the host.
 *
 * The type-only imports above are part of the test: they stop compiling if the
 * barrel stops exporting them.
 */

describe('public API barrel', () => {
  it('exposes the connection, the HTTP client and the two transports', () => {
    expect(typeof GatewayConnection).toBe('function');
    expect(typeof GatewayHttpClient).toBe('function');
    expect(typeof createNativeSocketFactory).toBe('function');
    expect(typeof createFetchTransport).toBe('function');
  });

  it('exposes the error types with their classification helpers', () => {
    const http = new GatewayHttpError({ kind: 'http', message: 'nope', status: 404 });
    expect(http).toBeInstanceOf(Error);
    expect(http.isNotFound()).toBe(true);
    expect(http.isTransport()).toBe(false);

    const socket = new GatewaySocketError({ kind: 'unauthorized', message: 'nope', closeCode: 4001 });
    expect(socket).toBeInstanceOf(Error);
    expect(socket.retryable).toBe(false);
    expect(WS_CLOSE_UNAUTHORIZED).toBe(4001);
  });

  it('exposes the protocol helpers the host needs', () => {
    expect(newRequestId()).toMatch(/^[0-9a-f]{32}$/);
    expect(createClientRequest({ sessionId: 's', content: 'c' })).toMatchObject({ session_id: 's' });
    expect(isKnownEventType('text')).toBe(true);
    expect(isKnownEvent(decodeWingEvent({ type: 'text', content: 'hi' }) as KnownWingEvent)).toBe(true);
    expect(CHUNK_TYPE).toBe('_chunk');
    expect(DEFAULT_HTTP_TIMEOUT_MS).toBe(60_000);
    expect(COMPACT_HTTP_TIMEOUT_MS).toBe(1_200_000);
    expect(DEFAULT_RECONNECT_OPTIONS).toStrictEqual({ baseDelayMs: 1_000, maxDelayMs: 30_000 });
    expect(reconnectDelayMs(0)).toBe(1_000);
    expect(gatewayUrls({ host: '127.0.0.1', port: 32523 }).wsUrl).toBe('ws://127.0.0.1:32523/ws');
    expect(redactUrl('ws://h/ws?api_key=k')).toBe('ws://h/ws?api_key=***');
    expect(normalizeApiKey(' ')).toBeNull();
    expect(typeof ChunkReassembler).toBe('function');
  });

  it('exposes the loggers', () => {
    const loggers: CoreLogger[] = [consoleLogger, silentLogger];
    expect(loggers).toHaveLength(2);
    expect(() => silentLogger.debug('dropped')).not.toThrow();
  });

  it('keeps the exported model types usable (compile-time contract)', () => {
    const state: ConnectionState = {
      status: 'connected',
      clientId: 'c1',
      attempt: 0,
      reconnectInMs: null,
      lastError: null,
    };
    const frame: ClientRequest = {
      request_id: 'r1',
      session_id: 's1',
      content: 'hi',
      tool_call_id: null,
    };
    const message: SessionMessage = {
      role: 'user',
      content: 'hi',
      uuid: null,
      reasoning_content: null,
      tool_calls: [],
      tool_call_id: null,
    };
    const update: UpdateSessionRequest = {
      session_id: 's1',
      model: null,
      provider: null,
      agent: null,
      title: 'T',
      thinking: null,
      reasoning_effort: null,
      yolo: null,
      workspace: null,
      tools: null,
    };
    const agent: AgentInfo = {
      model_name: 'gpt-5',
      system_prompt: null,
      tools: [],
      skills: [],
      rules: [],
      workspace: null,
      provider_name: null,
    };
    const status: SessionStatus = 'working';
    const factory: SocketFactory | null = null;

    expect({ state, frame, message, update, agent, status, factory }).toBeDefined();
  });

  it('keeps the event union nameable', () => {
    const events: WingEvent[] = [decodeWingEvent({ type: 'done' })];
    expect(events).toHaveLength(1);
  });
});
