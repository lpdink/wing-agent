/**
 * Small shared aliases and JSON-safe primitives.
 *
 * Contract conventions (03 / 04 / 05 must follow these):
 *
 * 1. **Host is the only authority.** Every model value below is produced by the
 *    host's reducer; the webview only *applies* it (hydrate + ordered ops).
 * 2. **Nullable, not optional.** Fields that can be absent are typed `| null`
 *    rather than `?`, so both sides always agree on the key set and a JSON
 *    round-trip is lossless. Additive changes must therefore append `| null`
 *    fields (or new variants) — never repurpose an existing field.
 * 3. **Plain JSON only.** Everything crossing the bridge must survive
 *    `JSON.parse(JSON.stringify(x))`: no `Date`, `Map`, `Set`, class instances,
 *    `undefined`, or functions.
 */

/** A value that survives structured JSON serialization (bridge-safe). */
export type JsonValue =
  string | number | boolean | null | readonly JsonValue[] | { readonly [key: string]: JsonValue };

/** Gateway session id (opaque string). */
export type SessionId = string;

/** Host-assigned, session-unique identifier for a transcript cell. */
export type CellId = string;

/** Gateway tool-call correlation id (also used as an ask correlation id). */
export type ToolCallId = string;

/** Correlation id for a host → webview request that expects a webview answer. */
export type RequestId = string;

/** Epoch milliseconds (host clock) — never a `Date`. */
export type EpochMs = number;
