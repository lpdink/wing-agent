import { describe, expect, it } from 'vitest';

import {
  createClientRequest,
  decodeConnectResponse,
  decodeToolCallRequest,
  decodeToolCallResult,
  encodeClientRequest,
  encodeToolCallResult,
  newRequestId,
} from '../../src/core/protocol/frames';

/**
 * WS frames — handshake, the client → server `ClientRequest`, and the remote
 * tool-host frames (`routes/ws.py`).
 */

describe('handshake', () => {
  it('decodes a ConnectResponse', () => {
    expect(decodeConnectResponse({ type: 'connected', client_id: 'abc' })).toStrictEqual({
      type: 'connected',
      client_id: 'abc',
    });
  });

  it('rejects anything without a client_id', () => {
    expect(decodeConnectResponse({ type: 'connected' })).toBeNull();
    expect(decodeConnectResponse({ type: 'connected', client_id: 7 })).toBeNull();
    expect(decodeConnectResponse({ type: 'text', content: 'hi' })).toBeNull();
    expect(decodeConnectResponse('nope')).toBeNull();
  });
});

describe('ClientRequest', () => {
  it('generates uuid4().hex-shaped request ids', () => {
    const id = newRequestId();
    expect(id).toMatch(/^[0-9a-f]{32}$/);
    expect(newRequestId()).not.toBe(id);
  });

  it('builds a frame with a fresh id and no tool call unless asked', () => {
    const frame = createClientRequest({ sessionId: 'sess-1', content: 'hi' });
    expect(frame).toMatchObject({ session_id: 'sess-1', content: 'hi', tool_call_id: null });
    expect(frame.request_id).toMatch(/^[0-9a-f]{32}$/);

    expect(createClientRequest({ sessionId: 's', content: 'c', toolCallId: 'call-1' }).tool_call_id).toBe(
      'call-1',
    );
  });

  it('omits a null tool_call_id on the wire (Pydantic default semantics)', () => {
    const encoded = encodeClientRequest({
      request_id: 'req-1',
      session_id: 'sess-1',
      content: 'hi',
      tool_call_id: null,
    });
    expect(JSON.parse(encoded)).toStrictEqual({ request_id: 'req-1', session_id: 'sess-1', content: 'hi' });

    const withTool = encodeClientRequest({
      request_id: 'req-2',
      session_id: 'sess-1',
      content: 'yes',
      tool_call_id: 'call-9',
    });
    expect(JSON.parse(withTool)).toMatchObject({ tool_call_id: 'call-9' });
  });
});

describe('remote tool frames', () => {
  it('decodes a tool_call_request', () => {
    expect(
      decodeToolCallRequest({
        type: 'tool_call_request',
        call_id: 'c1',
        name: 'Bash',
        arguments: { command: 'ls' },
      }),
    ).toStrictEqual({
      type: 'tool_call_request',
      call_id: 'c1',
      name: 'Bash',
      arguments: { command: 'ls' },
    });
  });

  it('defaults missing tool_call_request arguments to an empty object', () => {
    expect(decodeToolCallRequest({ call_id: 'c1', name: 'Bash' })).toMatchObject({ arguments: {} });
    expect(decodeToolCallRequest({ call_id: 'c1' })).toBeNull();
  });

  it('decodes and encodes a tool_call_result', () => {
    const frame = { type: 'tool_call_result', call_id: 'c1', result: 'ok', is_error: false } as const;
    expect(decodeToolCallResult({ call_id: 'c1', result: 'ok' })).toStrictEqual(frame);
    expect(JSON.parse(encodeToolCallResult(frame))).toStrictEqual(frame);
    expect(decodeToolCallResult({ result: 'no call id' })).toBeNull();
  });
});
