/**
 * Session-history record types — the typed mirror of the Message projection.
 *
 * Mirrors `wing/session.py::serialize_message` (the per-message dict the backend
 * sends as `SyncSessionEvent.messages` / `.uncommitted` and as
 * `GET /api/session/get` → `messages`), i.e. the same shape
 * `crates/wing/src/protocol/history.rs::SessionMessage` mirrors.
 *
 * The projection carries exactly: `role`, `content`, `uuid`, plus — only when
 * non-empty — `reasoning_content`, `tool_calls[{id,name,arguments}]` and
 * `tool_call_id`. Decoding is therefore deliberately tolerant (missing optional
 * fields default, unknown fields are ignored): these payloads are replayed after
 * a gateway upgrade and an old record must not break a whole replay. Fact-event
 * nodes are *not* records — they decode through `WingEvent` instead.
 */

import type { JsonValue } from '../../shared';
import {
  type JsonObject,
  decodeEach,
  isJsonObject,
  optJsonValue,
  optString,
  readJsonArray,
  stringOr,
} from './json';

/** One tool call on a history message (`{id, name, arguments}`). */
export interface SessionToolCall {
  readonly id: string;
  readonly name: string;
  /**
   * Parsed tool arguments. The Rust mirror distinguishes "absent" from an
   * explicit `null` (only `wing tail` prints that difference); the host does not,
   * so both collapse to `null` here.
   */
  readonly arguments: JsonValue | null;
}

/**
 * A Message record of the session history (the frontend replay projection).
 *
 * `role` stays a plain string on purpose (`TUI`/Rust do the same): the backend
 * emits `system | user | assistant | tool`, but an unknown future role should
 * degrade to "render nothing" instead of failing the replay.
 */
export interface SessionMessage {
  readonly role: string;
  readonly content: string;
  readonly uuid: string | null;
  readonly reasoning_content: string | null;
  readonly tool_calls: readonly SessionToolCall[];
  readonly tool_call_id: string | null;
}

/** One unterminated tool call (`Agent.uncommitted_tools()`), streaming args and all. */
export interface UncommittedTool {
  readonly tool_call_id: string;
  readonly tool_name: string;
  /** Raw args text accumulated so far — the backend never parses partial JSON. */
  readonly args_fragment: string;
}

/** Decode one history payload; `null` when it is not a Message projection. */
export function decodeSessionMessage(value: unknown): SessionMessage | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    role: stringOr(value, 'role', ''),
    content: stringOr(value, 'content', ''),
    uuid: optString(value, 'uuid'),
    reasoning_content: optString(value, 'reasoning_content'),
    tool_calls: decodeEach(readJsonArray(value, 'tool_calls'), decodeSessionToolCall),
    tool_call_id: optString(value, 'tool_call_id'),
  };
}

function decodeSessionToolCall(value: unknown): SessionToolCall | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const id = optString(value, 'id');
  const name = optString(value, 'name');
  if (id === null || name === null) {
    return null;
  }
  return { id, name, arguments: optJsonValue(value, 'arguments') };
}

/** Decode a `messages` / `events`-style array, skipping entries that do not decode. */
export function decodeSessionMessages(values: readonly unknown[]): SessionMessage[] {
  return decodeEach(values, decodeSessionMessage);
}

/** Decode one unterminated-tool entry; `null` when the payload is unusable. */
export function decodeUncommittedTool(value: unknown): UncommittedTool | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const toolCallId = optString(value, 'tool_call_id');
  const toolName = optString(value, 'tool_name');
  if (toolCallId === null || toolName === null) {
    return null;
  }
  return {
    tool_call_id: toolCallId,
    tool_name: toolName,
    args_fragment: stringOr(value, 'args_fragment', ''),
  };
}

/** Decode `SyncSessionEvent.uncommitted_tools`. */
export function decodeUncommittedTools(values: readonly unknown[]): UncommittedTool[] {
  return decodeEach(values, decodeUncommittedTool);
}

/** `true` when the message carries at least one tool call. */
export function hasToolCalls(message: SessionMessage): boolean {
  return message.tool_calls.length > 0;
}

/** Narrow a decoded tool-call argument object (helper for the host's renderers). */
export function asJsonObject(value: JsonValue | null): JsonObject | null {
  return isJsonObject(value) ? value : null;
}
