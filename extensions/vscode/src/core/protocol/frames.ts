/**
 * WebSocket handshake and client → server frames.
 *
 * Mirrors `wing/gateway/protocol.py::ConnectResponse` / `ClientRequest` /
 * `ToolCallRequest` / `ToolCallResult` and `crates/wing/src/protocol/
 * client_request.rs` + `connect_response.rs`.
 *
 * The gateway distinguishes inbound frames by shape (`routes/ws.py`): a frame
 * carrying `call_id` is a remote-tool result, everything else is a
 * `ClientRequest`. Only the latter is used by this extension — the tool-host
 * frames are mirrored so the protocol lives in one place.
 */

import { type JsonObject, isJsonObject } from './json';

/** First frame after a successful `/ws` upgrade (`type` is always `connected`). */
export interface ConnectResponse {
  readonly type: string;
  readonly client_id: string;
}

/** Client → server user-message frame (`ChatClientRequest`). */
export interface ClientRequest {
  /** Correlation id; the backend echoes it in `delivered` / `user_message_accepted`. */
  readonly request_id: string;
  readonly session_id: string;
  readonly content: string;
  /** Set when answering an `ask` event so the gateway resolves the right waiter. */
  readonly tool_call_id: string | null;
}

/** Server → tool host frame (`routes/ws.py::ToolCallRequest`). */
export interface ToolCallRequestFrame {
  readonly type: 'tool_call_request';
  readonly call_id: string;
  readonly name: string;
  readonly arguments: JsonObject;
}

/** Tool host → server frame (`routes/ws.py::ToolCallResult`). */
export interface ToolCallResultFrame {
  readonly type: 'tool_call_result';
  readonly call_id: string;
  readonly result: string;
  readonly is_error: boolean;
}

/**
 * A fresh request id.
 *
 * Format matters: the backend generates `uuid.uuid4().hex` (32 lowercase hex
 * characters, no dashes) and the host may compare ids across both paths.
 */
export function newRequestId(): string {
  return crypto.randomUUID().replace(/-/g, '');
}

/** Build a `ClientRequest` with a fresh id (the frame the host sends). */
export function createClientRequest(input: {
  readonly sessionId: string;
  readonly content: string;
  readonly toolCallId?: string | null;
}): ClientRequest {
  return {
    request_id: newRequestId(),
    session_id: input.sessionId,
    content: input.content,
    tool_call_id: input.toolCallId ?? null,
  };
}

/** Wire encoding: null `tool_call_id` is omitted (Pydantic's default). */
export function encodeClientRequest(frame: ClientRequest): string {
  return JSON.stringify({
    request_id: frame.request_id,
    session_id: frame.session_id,
    content: frame.content,
    ...(frame.tool_call_id === null ? {} : { tool_call_id: frame.tool_call_id }),
  });
}

/**
 * Decode the handshake frame; `null` when it is not a `ConnectResponse`.
 *
 * Structural and throw-free on purpose: this runs in the middle of `connect()`
 * and every "not a handshake" case is the same failure (a rejected dial), so the
 * caller only needs `null` — there is no payload worth surfacing.
 *
 * `type` is echoed (with the `connected` default) but **not required**: the
 * handshake is identified by *position* (the first frame after the upgrade), so
 * requiring the discriminant would only reject gateways that omit it.
 */
export function decodeConnectResponse(value: unknown): ConnectResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const clientId = value['client_id'];
  if (typeof clientId !== 'string') {
    return null;
  }
  const type = value['type'];
  return { type: typeof type === 'string' ? type : 'connected', client_id: clientId };
}

/**
 * Decode a `tool_call_request` frame (tool-host side); `null` when unusable.
 *
 * The discriminant is required (unlike the positional handshake): both ends of
 * the tool channel always write it (`ToolCallRequest.model_dump_json()` on the
 * gateway side, an explicit `"type"` in `wing_sdk/host.py`), and the frame
 * shares the socket with event payloads — a shape-only check would accept any
 * object that happens to carry `call_id`.
 */
export function decodeToolCallRequest(value: unknown): ToolCallRequestFrame | null {
  if (!isJsonObject(value) || value['type'] !== 'tool_call_request') {
    return null;
  }
  const callId = value['call_id'];
  const name = value['name'];
  if (typeof callId !== 'string' || typeof name !== 'string') {
    return null;
  }
  const args = value['arguments'];
  return {
    type: 'tool_call_request',
    call_id: callId,
    name,
    arguments: isJsonObject(args) ? args : {},
  };
}

/**
 * Decode a `tool_call_result` frame (gateway side); `null` when unusable.
 *
 * Same discriminant rule as {@link decodeToolCallRequest} — the tool host always
 * sends it, so accepting a typeless frame would only hide a protocol break.
 */
export function decodeToolCallResult(value: unknown): ToolCallResultFrame | null {
  if (!isJsonObject(value) || value['type'] !== 'tool_call_result') {
    return null;
  }
  const callId = value['call_id'];
  if (typeof callId !== 'string') {
    return null;
  }
  const result = value['result'];
  return {
    type: 'tool_call_result',
    call_id: callId,
    result: typeof result === 'string' ? result : '',
    is_error: value['is_error'] === true,
  };
}

/** Encode a `tool_call_result` frame (tool-host side). */
export function encodeToolCallResult(frame: ToolCallResultFrame): string {
  return JSON.stringify(frame);
}
