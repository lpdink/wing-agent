/**
 * Payload reading primitives shared by every decoder in `src/core/protocol`.
 *
 * The wire is JSON produced by Pydantic (`wire_dump` in `wing/event/__init__.py`
 * strips null values and storage-only keys), so decoding means: *prove* the shape
 * field by field instead of casting a `JSON.parse` result into a typed object.
 * A required field that is missing / of the wrong JSON type throws
 * {@link ProtocolDecodeError}; the event decoder turns that into an
 * `UnknownWingEvent` rather than handing a half-filled object to the host.
 */

import type { JsonValue } from '../../shared';

/** A JSON object (what every wire payload is). */
export type JsonObject = { readonly [key: string]: JsonValue };

/** Thrown by the `req*` / `read*` helpers when a payload does not match the mirror. */
export class ProtocolDecodeError extends Error {
  override readonly name: string = 'ProtocolDecodeError';

  constructor(message: string) {
    super(message);
  }
}

/** `true` for a non-null, non-array object (JSON's definition of an object). */
export function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** `true` for a JSON array. */
export function isJsonArray(value: unknown): value is readonly JsonValue[] {
  return Array.isArray(value);
}

/**
 * `JSON.parse` without the `any`-typed result.
 *
 * Parsing failure is a value, not an exception: the read path needs to tell
 * "not JSON" apart from "valid JSON that is not what we expected".
 */
export function tryParseJson(
  text: string,
): { readonly ok: true; readonly value: unknown } | { readonly ok: false; readonly error: string } {
  try {
    return { ok: true, value: JSON.parse(text) as unknown };
  } catch (cause) {
    return { ok: false, error: cause instanceof Error ? cause.message : 'invalid JSON' };
  }
}

function typeName(value: unknown): string {
  if (value === null) {
    return 'null';
  }
  if (Array.isArray(value)) {
    return 'array';
  }
  if (typeof value === 'object') {
    return 'object';
  }
  return typeof value;
}

/** Required string field. */
export function reqString(object: JsonObject, key: string): string {
  const value = object[key];
  if (typeof value !== 'string') {
    throw new ProtocolDecodeError(
      `field "${key}" must be a string (got ${value === undefined ? 'nothing' : typeName(value)})`,
    );
  }
  return value;
}

/** Nullable string field: absent / `null` → `null`. */
export function optString(object: JsonObject, key: string): string | null {
  const value = object[key];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== 'string') {
    throw new ProtocolDecodeError(`field "${key}" must be a string or null (got ${typeName(value)})`);
  }
  return value;
}

/** Nullable string field with a default for the absent case. */
export function stringOr(object: JsonObject, key: string, fallback: string): string {
  return optString(object, key) ?? fallback;
}

/** Required number field. */
export function reqNumber(object: JsonObject, key: string): number {
  const value = object[key];
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new ProtocolDecodeError(
      `field "${key}" must be a finite number (got ${value === undefined ? 'nothing' : typeName(value)})`,
    );
  }
  return value;
}

/** Nullable number field: absent / `null` → `null`. */
export function optNumber(object: JsonObject, key: string): number | null {
  const value = object[key];
  if (value === undefined || value === null) {
    return null;
  }
  return reqNumber(object, key);
}

/** Nullable number field with a default for the absent case. */
export function numberOr(object: JsonObject, key: string, fallback: number): number {
  return optNumber(object, key) ?? fallback;
}

/** Required boolean field. */
export function reqBoolean(object: JsonObject, key: string): boolean {
  const value = object[key];
  if (typeof value !== 'boolean') {
    throw new ProtocolDecodeError(
      `field "${key}" must be a boolean (got ${value === undefined ? 'nothing' : typeName(value)})`,
    );
  }
  return value;
}

/** Nullable boolean field with a default for the absent case. */
export function booleanOr(object: JsonObject, key: string, fallback: boolean): boolean {
  const value = object[key];
  if (value === undefined || value === null) {
    return fallback;
  }
  return reqBoolean(object, key);
}

/** Nullable string field restricted to a known set; anything else → `fallback`. */
export function enumOr<T extends string>(
  object: JsonObject,
  key: string,
  allowed: readonly T[],
  fallback: T,
): T {
  const value = optString(object, key);
  if (value === null) {
    return fallback;
  }
  return allowed.some((candidate) => candidate === value) ? (value as T) : fallback;
}

/** Nullable JSON object field. */
export function optJsonObject(object: JsonObject, key: string): JsonObject | null {
  const value = object[key];
  if (value === undefined || value === null) {
    return null;
  }
  if (!isJsonObject(value)) {
    throw new ProtocolDecodeError(`field "${key}" must be an object or null (got ${typeName(value)})`);
  }
  return value;
}

/** Nullable JSON value field, kept verbatim (usage blocks, tool args, …). */
export function optJsonValue(object: JsonObject, key: string): JsonValue | null {
  const value = object[key];
  return value === undefined ? null : value;
}

/** Array field as raw JSON values; absent / `null` → `[]`. */
export function readJsonArray(object: JsonObject, key: string): readonly unknown[] {
  const value = object[key];
  if (value === undefined || value === null) {
    return [];
  }
  if (!isJsonArray(value)) {
    throw new ProtocolDecodeError(`field "${key}" must be an array (got ${typeName(value)})`);
  }
  return value;
}

/** Array of JSON objects; absent / `null` → `[]`. */
export function readJsonObjectArray(object: JsonObject, key: string): readonly JsonObject[] {
  return readJsonArray(object, key).filter(isJsonObject);
}

/** Array of strings; absent / `null` → `[]`. */
export function readStringArray(object: JsonObject, key: string): readonly string[] {
  const values = readJsonArray(object, key);
  return values.map((value) => {
    if (typeof value !== 'string') {
      throw new ProtocolDecodeError(
        `field "${key}" must be an array of strings (got ${typeName(value)} element)`,
      );
    }
    return value;
  });
}

/**
 * Decode every element of a collection, dropping the ones that do not decode.
 *
 * Used exactly where the backend itself is forward-tolerant (`sync_session`
 * history / replay nodes, `ask` questions, `content_blocks`): one unknown node
 * must not invalidate a whole replay. Scalars stay strict — that split is the
 * policy documented at the top of `events.ts`.
 */
export function decodeEach<T>(values: readonly unknown[], decode: (value: unknown) => T | null): T[] {
  const decoded: T[] = [];
  for (const value of values) {
    const item = decode(value);
    if (item !== null) {
      decoded.push(item);
    }
  }
  return decoded;
}

/**
 * Build a request body, dropping `null` / `undefined` fields.
 *
 * Mirrors Pydantic's "absent = use the default" semantics and the Rust client's
 * `skip_serializing_if = "Option::is_none"`: `false` / `0` / `""` are values and
 * are always kept.
 */
export function jsonBody(fields: Readonly<Record<string, JsonValue | undefined>>): JsonObject {
  const body: Record<string, JsonValue> = {};
  for (const [key, value] of Object.entries(fields)) {
    if (value !== null && value !== undefined) {
      body[key] = value;
    }
  }
  return body;
}
