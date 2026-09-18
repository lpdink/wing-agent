/**
 * HTTP transport seam.
 *
 * `GatewayHttpClient` builds absolute URLs, headers and bodies; the transport
 * only performs the round trip. Two injectable levels:
 *
 * - whole transport (`HttpTransport`) — what the client tests inject to assert
 *   request shapes and script responses;
 * - the `fetch` implementation (`fetchImpl`) — what the transport tests inject
 *   to check timeout / network-error classification.
 *
 * The default uses `globalThis.fetch` (Node ≥ 18 / Electron), i.e. no npm
 * dependency for either side of the client.
 */

import { GatewayHttpError, describeThrown } from '../errors';

export type HttpMethod = 'GET' | 'POST';

export interface HttpTransportRequest {
  readonly method: HttpMethod;
  /** Absolute URL, query string already included. */
  readonly url: string;
  readonly headers: Readonly<Record<string, string>>;
  /** JSON text; `null` for GET / bodyless POST. */
  readonly body: string | null;
  /** Deadline for the whole round trip. */
  readonly timeoutMs: number;
}

export interface HttpTransportResponse {
  readonly status: number;
  readonly body: string;
}

export interface HttpTransport {
  request(request: HttpTransportRequest): Promise<HttpTransportResponse>;
}

/** Minimal structural view of a `fetch` response. */
export interface FetchResponseLike {
  readonly status: number;
  text(): Promise<string>;
}

export interface FetchInitLike {
  readonly method: string;
  readonly headers: Readonly<Record<string, string>>;
  readonly body: string | undefined;
  readonly signal: AbortSignal;
}

export type FetchLike = (url: string, init: FetchInitLike) => Promise<FetchResponseLike>;

export interface FetchTransportOptions {
  /** Override `fetch` (tests, custom agents); defaults to `globalThis.fetch`. */
  readonly fetchImpl?: FetchLike;
}

/** Create the `fetch`-backed transport. */
export function createFetchTransport(options: FetchTransportOptions = {}): HttpTransport {
  const fetchImpl = options.fetchImpl ?? readGlobalFetch();

  return {
    async request(request: HttpTransportRequest): Promise<HttpTransportResponse> {
      const controller = new AbortController();
      let timedOut = false;
      let timer: ReturnType<typeof setTimeout> | null = null;

      const deadline = new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          timedOut = true;
          controller.abort();
          reject(
            new GatewayHttpError({
              kind: 'timeout',
              message: `${request.method} ${request.url} timed out after ${request.timeoutMs} ms`,
            }),
          );
        }, request.timeoutMs);
      });

      const roundTrip = (): Promise<FetchResponseLike> =>
        fetchImpl(request.url, {
          method: request.method,
          headers: request.headers,
          body: request.body ?? undefined,
          signal: controller.signal,
        });

      try {
        const pending = roundTrip();
        // The loser of the race must not surface as an unhandled rejection.
        pending.catch(() => undefined);
        const response = await Promise.race([pending, deadline]);
        const body = await response.text();
        return { status: response.status, body };
      } catch (cause) {
        if (cause instanceof GatewayHttpError) {
          throw cause;
        }
        if (timedOut) {
          // The implementation ignored the abort signal; still report the deadline.
          throw new GatewayHttpError({
            kind: 'timeout',
            message: `${request.method} ${request.url} timed out after ${request.timeoutMs} ms`,
            cause,
          });
        }
        throw new GatewayHttpError({
          kind: 'network',
          message: `${request.method} ${request.url} failed: ${describeThrown(cause)}`,
          cause,
        });
      } finally {
        if (timer !== null) {
          clearTimeout(timer);
        }
      }
    },
  };
}

function readGlobalFetch(): FetchLike {
  const candidate: unknown = globalThis.fetch;
  if (typeof candidate !== 'function') {
    throw new GatewayHttpError({
      kind: 'config',
      message: 'no global fetch implementation available (Node >= 18 or Electron required)',
    });
  }
  return candidate as FetchLike;
}
