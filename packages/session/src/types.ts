/**
 * Small shared aliases and JSON-safe primitives.
 *
 * Contract conventions (every frontend must follow these):
 *
 * 1. **Host is the only authority.** Every model value built by this package is
 *    produced by the host's reducer; a view only *applies* it (hydrate + ordered ops).
 * 2. **Nullable, not optional.** Fields that can be absent are typed `| null`
 *    rather than `?`, so both sides always agree on the key set and a JSON
 *    round-trip is lossless. Additive changes must therefore append `| null`
 *    fields (or new variants) — never repurpose an existing field.
 * 3. **Plain JSON only.** Everything crossing a process/bridge boundary must survive
 *    `JSON.parse(JSON.stringify(x))`: no `Date`, `Map`, `Set`, class instances,
 *    `undefined`, or functions.
 */

/**
 * A value that survives structured JSON serialization (bridge-safe).
 *
 * Re-exported from the gateway capability layer rather than redefined here: the
 * wire's value type is the client package's business (`packages/client/src/protocol/
 * json.ts`), and the session package already depends on it for the decoded events,
 * so there is exactly one definition.
 */
export type { JsonValue } from '@wing-agent/client';

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
