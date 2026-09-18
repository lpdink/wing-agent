/**
 * Typed error surface for the gateway client.
 *
 * `src/core` never throws bare strings: every failure is one of the two classes
 * below, so 03 can branch on `kind` / `status` instead of parsing messages.
 *
 * Mirrors `crates/wing-api-client/src/error.rs` (`ApiClientError`) on the HTTP
 * side and `crates/wing/src/gateway/client.rs` (`CloseReason`) on the WS side,
 * with one deliberate difference: JS' WebSocket does not surface a
 * "frame too large" client error (undici / the browser handle it), so the
 * `CloseReason::FrameTooLarge` variant has no counterpart here.
 */

import type { ErrorResponse } from './protocol/http';

/** Why an HTTP call failed. */
export type HttpErrorKind =
  /** The transport itself failed (DNS, connection refused, socket reset, …). */
  | 'network'
  /** The request exceeded its deadline. Transient; retrying is usually right. */
  | 'timeout'
  /** The gateway answered with a 4xx/5xx status (see `status` / `body`). */
  | 'http'
  /** A 2xx response could not be decoded into the expected model. */
  | 'malformed-response'
  /** The client was built with an unusable configuration (bad base URL). */
  | 'config';

export interface GatewayHttpErrorInit {
  readonly kind: HttpErrorKind;
  readonly message: string;
  /** Server status; `null` for transport-level failures. */
  readonly status?: number | null;
  /** Decoded `ErrorResponse` when the body matched the backend's shape. */
  readonly body?: ErrorResponse | null;
  /** Raw response body (kept even when it is not an `ErrorResponse`). */
  readonly rawBody?: string | null;
  readonly cause?: unknown;
}

/**
 * Every HTTP-level failure of `GatewayHttpClient`.
 *
 * The backend answers errors with one shape (`wing/gateway/protocol.py::
 * ErrorResponse`); `body` carries it when the body was parseable and `rawBody`
 * always carries the raw text — the same split `extract_api_error` implements
 * in the Rust client.
 */
export class GatewayHttpError extends Error {
  override readonly name: string = 'GatewayHttpError';
  readonly kind: HttpErrorKind;
  readonly status: number | null;
  readonly body: ErrorResponse | null;
  readonly rawBody: string | null;

  constructor(init: GatewayHttpErrorInit) {
    super(init.message, init.cause === undefined ? undefined : { cause: init.cause });
    this.kind = init.kind;
    this.status = init.status ?? null;
    this.body = init.body ?? null;
    this.rawBody = init.rawBody ?? null;
  }

  /** Best-effort human detail: `ErrorResponse.detail`, else `ErrorResponse.error`, else the raw body. */
  get detail(): string | null {
    const fromBody = this.body?.detail ?? this.body?.error ?? null;
    if (fromBody !== null && fromBody !== '') {
      return fromBody;
    }
    return this.rawBody === null || this.rawBody === '' ? null : this.rawBody;
  }

  /**
   * The gateway said "this resource does not exist" (404).
   *
   * Like `ApiClientError::is_not_found`, this never improves with a retry: the
   * caller should stop and recover (start a fresh session, refresh the list)
   * instead of looping.
   */
  isNotFound(): boolean {
    return this.status === 404;
  }

  /** 401 / 403 — credentials missing, wrong, or the role is not allowed. */
  isUnauthorized(): boolean {
    return this.status === 401 || this.status === 403;
  }

  /** Transport-level failure (no HTTP status): network, timeout or config. */
  isTransport(): boolean {
    return this.status === null;
  }
}

/** Why a WebSocket-level operation failed. */
export type SocketErrorKind =
  /** The socket could not be opened at all (bad URL, refused, no WebSocket impl). */
  | 'connect-failed'
  /** The first frame was not a valid `ConnectResponse`, or the handshake timed out. */
  | 'handshake'
  /** The gateway rejected the connection for auth reasons (close code 4001). */
  | 'unauthorized'
  /** A chunked event violated the reassembly contract; the stream is unusable. */
  | 'reassembly'
  /** The connection was lost after a successful handshake. */
  | 'disconnected'
  /** `send()` was called while no socket was open. */
  | 'not-connected'
  /** The socket accepted the write and then threw. */
  | 'send-failed';

export interface GatewaySocketErrorInit {
  readonly kind: SocketErrorKind;
  readonly message: string;
  /** WebSocket close code when the failure came from a close frame. */
  readonly closeCode?: number | null;
  /** Extra, machine-readable-ish context (reassembly detail, close reason, …). */
  readonly detail?: string | null;
  readonly cause?: unknown;
}

/**
 * Every WebSocket-level failure of `GatewayConnection`.
 *
 * `retryable` encodes the reconnect policy in one place: everything is worth
 * retrying except a rejected credential (close code 4001) — a wrong API key
 * does not fix itself, and a 30 s retry loop would only spam the user.
 */
export class GatewaySocketError extends Error {
  override readonly name: string = 'GatewaySocketError';
  readonly kind: SocketErrorKind;
  readonly closeCode: number | null;
  readonly detail: string | null;

  constructor(init: GatewaySocketErrorInit) {
    super(init.message, init.cause === undefined ? undefined : { cause: init.cause });
    this.kind = init.kind;
    this.closeCode = init.closeCode ?? null;
    this.detail = init.detail ?? null;
  }

  get retryable(): boolean {
    return this.kind !== 'unauthorized' && this.kind !== 'not-connected' && this.kind !== 'send-failed';
  }
}

/** WebSocket close code the gateway uses for a rejected API key (`routes/ws.py`). */
export const WS_CLOSE_UNAUTHORIZED = 4001;

/** One-line description of an unknown thrown value (never `"[object Object]"`). */
export function describeThrown(cause: unknown): string {
  if (cause instanceof Error) {
    return cause.message;
  }
  if (typeof cause === 'string') {
    return cause;
  }
  try {
    return JSON.stringify(cause) ?? String(cause);
  } catch {
    return String(cause);
  }
}
