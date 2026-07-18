// sdk/src/errors.ts — ApiClientError for HTTP failures.
//
// Mirrors: crates/wing-api-client/src/error.rs

import type { ErrorResponse } from './protocol'

export type ApiClientErrorKind = 'transport' | 'api' | 'deserialize'

export class ApiClientError extends Error {
  readonly kind: ApiClientErrorKind
  readonly status?: number
  readonly body?: ErrorResponse | null

  private constructor(
    kind: ApiClientErrorKind,
    message: string,
    status?: number,
    body?: ErrorResponse | null,
  ) {
    super(message)
    this.name = 'ApiClientError'
    this.kind = kind
    this.status = status
    this.body = body
  }

  /** HTTP request failed (network error, timeout, etc.). */
  static transport(cause: unknown): ApiClientError {
    const msg = cause instanceof Error ? cause.message : String(cause)
    return new ApiClientError('transport', `HTTP request failed: ${msg}`)
  }

  /** Server returned a non-2xx response. */
  static api(status: number, detail: string, body?: ErrorResponse | null): ApiClientError {
    return new ApiClientError('api', `API error (${status}): ${detail}`, status, body)
  }

  /** Response body could not be parsed as JSON. */
  static deserialize(cause: unknown): ApiClientError {
    const msg = cause instanceof Error ? cause.message : String(cause)
    return new ApiClientError('deserialize', `Failed to deserialize response: ${msg}`)
  }
}

/**
 * Attempt to extract a structured ApiClientError from a non-2xx Response.
 * Tries to parse the body as an ErrorResponse; falls back to raw text.
 */
export async function extractApiError(resp: Response): Promise<ApiClientError> {
  const status = resp.status
  const text = await resp.text().catch(() => '')

  let body: ErrorResponse | null = null
  try {
    body = JSON.parse(text) as ErrorResponse
  } catch {
    // not valid JSON — ignore
  }

  const detail = body?.detail ?? text
  return ApiClientError.api(status, detail, body)
}
