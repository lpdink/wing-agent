/**
 * `GatewayHttpClient` — every gateway HTTP endpoint, one method each.
 *
 * HTTP carries lifecycle / query / mutation (22 RPC endpoints, see
 * `docs/dev/http-api.md`); the WebSocket only carries the live event stream.
 * Session creation and the WS handshake are decoupled, so this client never
 * needs the connection and vice versa.
 *
 * Conventions:
 * - request bodies are built from the typed mirror in `protocol/http.ts`; `null`
 *   fields are dropped (`skip_serializing_if` semantics), so "absent" and
 *   "keep the current value" mean the same thing to the backend;
 * - every non-2xx status becomes a {@link GatewayHttpError} carrying the decoded
 *   `ErrorResponse` **and** the raw body (never a bare string);
 * - every 2xx body is decoded by the mirror's decoder — an undecodable body is
 *   `GatewayHttpError{kind:'malformed-response'}`, not a cast.
 */

import { GatewayHttpError } from './errors';
import { type CoreLogger, consoleLogger } from './logging';
import { type JsonObject, ProtocolDecodeError, jsonBody, tryParseJson } from './protocol/json';
import {
  type AgentOverride,
  type AgentsResponse,
  type BranchesResponse,
  type CompactResponse,
  type CommandsResponse,
  type CreateSessionRequest,
  type CreateSessionResponse,
  type ForkSessionResponse,
  type HealthResponse,
  type ModelsResponse,
  type OkResponse,
  type RegisterToolsResponse,
  type RemoteToolSpec,
  type ResumeSessionResponse,
  type ReloadResponse,
  type RewindResponse,
  type SendMessageResponse,
  type SessionGetResponse,
  type SessionInfoResponse,
  type SessionListResponse,
  type ShutdownResponse,
  type ToolsListResponse,
  type UpdateSessionRequest,
  type UpdateSessionResponse,
  decodeAgentsResponse,
  decodeBranchesResponse,
  decodeCommandsResponse,
  decodeCompactResponse,
  decodeCreateSessionResponse,
  decodeErrorResponse,
  decodeForkSessionResponse,
  decodeHealthResponse,
  decodeModelsResponse,
  decodeOkResponse,
  decodeRegisterToolsResponse,
  decodeReloadResponse,
  decodeResumeSessionResponse,
  decodeRewindResponse,
  decodeSendMessageResponse,
  decodeSessionGetResponse,
  decodeSessionInfoResponse,
  decodeSessionListResponse,
  decodeShutdownResponse,
  decodeToolsListResponse,
  decodeUpdateSessionResponse,
} from './protocol/http';
import { type HttpMethod, type HttpTransport, createFetchTransport } from './transport/http';
import { normalizeApiKey } from './urls';

/** Default deadline for one HTTP round trip (matches the Rust client's 60 s). */
export const DEFAULT_HTTP_TIMEOUT_MS = 60_000;

/**
 * Deadline for `/api/session/compact`.
 *
 * The backend waits up to 1200 s for a manual compaction (`routes/session.py`);
 * the Rust client's flat 60 s budget turns a long compaction into a bogus
 * failure, so the extension gives this endpoint the server's own upper bound.
 */
export const COMPACT_HTTP_TIMEOUT_MS = 1_200_000;

/** Max characters of a raw body kept in an error message. */
const ERROR_BODY_PREVIEW = 300;

export interface GatewayHttpClientOptions {
  /** e.g. `http://127.0.0.1:32523` (see `gatewayUrls`). */
  readonly baseUrl: string;
  /** API key → `Authorization: Bearer …`; blank / `null` disables auth. */
  readonly apiKey?: string | null;
  /** Transport seam (tests); defaults to a `fetch`-backed transport. */
  readonly transport?: HttpTransport;
  /** Default deadline per request. */
  readonly timeoutMs?: number;
  readonly logger?: CoreLogger;
}

interface RequestSpec<T> {
  readonly method: HttpMethod;
  readonly path: string;
  readonly query?: Readonly<Record<string, string>>;
  readonly body?: JsonObject | null;
  readonly headers?: Readonly<Record<string, string>>;
  readonly timeoutMs?: number;
  readonly decode: (value: unknown) => T | null;
}

const EMPTY_CREATE_SESSION: CreateSessionRequest = {
  template_name: null,
  workspace: null,
  agent: null,
  backend: null,
};

export class GatewayHttpClient {
  private readonly baseUrl: string;
  private readonly apiKey: string | null;
  private readonly transport: HttpTransport;
  private readonly timeoutMs: number;
  private readonly logger: CoreLogger;

  constructor(options: GatewayHttpClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/+$/, '');
    this.apiKey = normalizeApiKey(options.apiKey);
    this.transport = options.transport ?? createFetchTransport();
    this.timeoutMs = options.timeoutMs ?? DEFAULT_HTTP_TIMEOUT_MS;
    this.logger = options.logger ?? consoleLogger;
  }

  // ── session lifecycle ────────────────────────────────────────────────

  /** `POST /api/session/create`. */
  async createSession(options: Partial<CreateSessionRequest> = {}): Promise<CreateSessionResponse> {
    const request: CreateSessionRequest = { ...EMPTY_CREATE_SESSION, ...options };
    return this.request({
      method: 'POST',
      path: '/api/session/create',
      body: jsonBody({
        template_name: request.template_name,
        workspace: request.workspace,
        agent: request.agent === null ? null : agentOverrideBody(request.agent),
        backend: request.backend,
      }),
      decode: decodeCreateSessionResponse,
    });
  }

  /** `POST /api/session/resume`. */
  async resumeSession(sessionId: string): Promise<ResumeSessionResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/resume',
      body: jsonBody({ session_id: sessionId }),
      decode: decodeResumeSessionResponse,
    });
  }

  /** `POST /api/session/fork`. */
  async forkSession(sourceSessionId: string, targetUuid: string): Promise<ForkSessionResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/fork',
      body: jsonBody({ source_session_id: sourceSessionId, target_uuid: targetUuid }),
      decode: decodeForkSessionResponse,
    });
  }

  // ── subscription (needs X-Client-Id) ─────────────────────────────────

  /** `POST /api/session/subscribe` — also pushes the `sync_session` replay. */
  async subscribe(sessionId: string, clientId: string): Promise<OkResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/subscribe',
      body: jsonBody({ session_id: sessionId }),
      headers: { 'X-Client-Id': clientId },
      decode: decodeOkResponse,
    });
  }

  /** `POST /api/session/unsubscribe`. */
  async unsubscribe(sessionId: string, clientId: string): Promise<OkResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/unsubscribe',
      body: jsonBody({ session_id: sessionId }),
      headers: { 'X-Client-Id': clientId },
      decode: decodeOkResponse,
    });
  }

  // ── messaging ────────────────────────────────────────────────────────

  /** `POST /api/session/send` (the WS `ClientRequest` path is equivalent). */
  async sendMessage(
    sessionId: string,
    content: string,
    toolCallId: string | null = null,
  ): Promise<SendMessageResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/send',
      body: jsonBody({ session_id: sessionId, content, tool_call_id: toolCallId }),
      decode: decodeSendMessageResponse,
    });
  }

  // ── queries ──────────────────────────────────────────────────────────

  /** `GET /api/session/list`. */
  async listSessions(): Promise<SessionListResponse> {
    return this.request({ method: 'GET', path: '/api/session/list', decode: decodeSessionListResponse });
  }

  /** `GET /api/session/get`. */
  async getSession(sessionId: string): Promise<SessionGetResponse> {
    return this.request({
      method: 'GET',
      path: '/api/session/get',
      query: { session_id: sessionId },
      decode: decodeSessionGetResponse,
    });
  }

  /** `GET /api/session/info` — runtime status (model, thinking, yolo, tokens, …). */
  async sessionInfo(sessionId: string): Promise<SessionInfoResponse> {
    return this.request({
      method: 'GET',
      path: '/api/session/info',
      query: { session_id: sessionId },
      decode: decodeSessionInfoResponse,
    });
  }

  /** `GET /api/session/branches` — rewind / fork candidates. */
  async sessionBranches(sessionId: string): Promise<BranchesResponse> {
    return this.request({
      method: 'GET',
      path: '/api/session/branches',
      query: { session_id: sessionId },
      decode: decodeBranchesResponse,
    });
  }

  // ── mutations ────────────────────────────────────────────────────────

  /**
   * `POST /api/session/update`.
   *
   * Only the fields present in `request` are sent; the backend rejects an empty
   * update (400) and requires `model` + `provider` to travel together.
   */
  async updateSession(
    request: { readonly session_id: string } & Partial<Omit<UpdateSessionRequest, 'session_id'>>,
  ): Promise<UpdateSessionResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/update',
      body: jsonBody({
        session_id: request.session_id,
        model: request.model ?? null,
        provider: request.provider ?? null,
        agent: request.agent ?? null,
        title: request.title ?? null,
        thinking: request.thinking ?? null,
        reasoning_effort: request.reasoning_effort ?? null,
        yolo: request.yolo ?? null,
        workspace: request.workspace ?? null,
        tools: request.tools === undefined || request.tools === null ? null : [...request.tools],
      }),
      decode: decodeUpdateSessionResponse,
    });
  }

  /** `POST /api/session/compact` (long deadline — see {@link COMPACT_HTTP_TIMEOUT_MS}). */
  async compactSession(sessionId: string, instruction: string | null = null): Promise<CompactResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/compact',
      body: jsonBody({ session_id: sessionId, instruction }),
      timeoutMs: COMPACT_HTTP_TIMEOUT_MS,
      decode: decodeCompactResponse,
    });
  }

  /** `POST /api/session/interrupt`. */
  async interruptSession(sessionId: string): Promise<OkResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/interrupt',
      body: jsonBody({ session_id: sessionId }),
      decode: decodeOkResponse,
    });
  }

  /** `POST /api/session/rewind`. */
  async rewindSession(sessionId: string, targetUuid: string): Promise<RewindResponse> {
    return this.request({
      method: 'POST',
      path: '/api/session/rewind',
      body: jsonBody({ session_id: sessionId, target_uuid: targetUuid }),
      decode: decodeRewindResponse,
    });
  }

  // ── system ───────────────────────────────────────────────────────────

  /** `GET /api/commands` — prompt commands only. */
  async listCommands(): Promise<CommandsResponse> {
    return this.request({ method: 'GET', path: '/api/commands', decode: decodeCommandsResponse });
  }

  /** `GET /api/models` — models grouped by provider. */
  async listModels(): Promise<ModelsResponse> {
    return this.request({ method: 'GET', path: '/api/models', decode: decodeModelsResponse });
  }

  /** `GET /api/agents` — agent templates + default. */
  async listAgents(): Promise<AgentsResponse> {
    return this.request({ method: 'GET', path: '/api/agents', decode: decodeAgentsResponse });
  }

  /** `GET /api/tools` — every registered tool (built-in + remote). */
  async listTools(): Promise<ToolsListResponse> {
    return this.request({ method: 'GET', path: '/api/tools', decode: decodeToolsListResponse });
  }

  /** `POST /api/system/reload` — hot reload config / hooks / providers / auth. */
  async reloadSystem(): Promise<ReloadResponse> {
    return this.request({ method: 'POST', path: '/api/system/reload', decode: decodeReloadResponse });
  }

  /** `POST /api/shutdown` — the gateway terminates itself ~100 ms later. */
  async shutdown(): Promise<ShutdownResponse> {
    return this.request({ method: 'POST', path: '/api/shutdown', decode: decodeShutdownResponse });
  }

  /** `GET /api/health` — never requires auth; used for liveness probes. */
  async health(): Promise<HealthResponse> {
    return this.request({ method: 'GET', path: '/api/health', decode: decodeHealthResponse });
  }

  /**
   * `POST /api/tools/register` (needs `X-Client-Id`).
   *
   * Only a tool host calls this; the extension is not one. Mirrored so the
   * protocol lives in one place (see design D1).
   */
  async registerTools(clientId: string, tools: readonly RemoteToolSpec[]): Promise<RegisterToolsResponse> {
    return this.request({
      method: 'POST',
      path: '/api/tools/register',
      body: jsonBody({ tools: tools.map(remoteToolSpecBody) }),
      headers: { 'X-Client-Id': clientId },
      decode: decodeRegisterToolsResponse,
    });
  }

  // ── plumbing ─────────────────────────────────────────────────────────

  private async request<T>(spec: RequestSpec<T>): Promise<T> {
    const url = `${this.baseUrl}${spec.path}${queryString(spec.query)}`;
    const headers: Record<string, string> = { Accept: 'application/json' };
    if (this.apiKey !== null) {
      headers['Authorization'] = `Bearer ${this.apiKey}`;
    }
    if (spec.body !== undefined && spec.body !== null) {
      headers['Content-Type'] = 'application/json';
    }
    for (const [key, value] of Object.entries(spec.headers ?? {})) {
      headers[key] = value;
    }

    const response = await this.transport.request({
      method: spec.method,
      url,
      headers,
      body: spec.body === undefined || spec.body === null ? null : JSON.stringify(spec.body),
      timeoutMs: spec.timeoutMs ?? this.timeoutMs,
    });

    this.logger.debug(`gateway http ${spec.method} ${spec.path} → ${response.status}`);
    if (response.status < 200 || response.status >= 300) {
      throw this.httpError(spec, response.status, response.body);
    }
    return this.decodeBody(spec, response.body);
  }

  private decodeBody<T>(spec: RequestSpec<T>, body: string): T {
    const parsed = tryParseJson(body);
    if (!parsed.ok) {
      throw new GatewayHttpError({
        kind: 'malformed-response',
        message: `${spec.method} ${spec.path} returned a body that is not JSON: ${parsed.error}`,
        status: 200,
        rawBody: body,
      });
    }
    let value: T | null;
    try {
      value = spec.decode(parsed.value);
    } catch (cause) {
      // A `ProtocolDecodeError` means the gateway sent a shape this build does
      // not understand — report it through the client's own error surface
      // instead of leaking the protocol-layer error type.
      if (!(cause instanceof ProtocolDecodeError)) {
        throw cause;
      }
      value = null;
    }
    if (value === null) {
      throw new GatewayHttpError({
        kind: 'malformed-response',
        message: `${spec.method} ${spec.path} returned an unexpected response shape`,
        status: 200,
        rawBody: body,
      });
    }
    return value;
  }

  private httpError(spec: RequestSpec<unknown>, status: number, body: string): GatewayHttpError {
    const parsed = tryParseJson(body);
    const errorBody = parsed.ok ? decodeErrorResponse(parsed.value) : null;
    const detail = errorBody?.detail ?? errorBody?.error ?? preview(body);
    return new GatewayHttpError({
      kind: 'http',
      status,
      message: `${spec.method} ${spec.path} failed: HTTP ${status}${detail === '' ? '' : ` — ${detail}`}`,
      body: errorBody,
      rawBody: body,
    });
  }
}

function queryString(query: Readonly<Record<string, string>> | undefined): string {
  if (query === undefined) {
    return '';
  }
  const pairs = Object.entries(query).map(
    ([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`,
  );
  return pairs.length === 0 ? '' : `?${pairs.join('&')}`;
}

function agentOverrideBody(agent: AgentOverride): JsonObject {
  return jsonBody({
    model: agent.model,
    provider: agent.provider,
    system_prompt: agent.system_prompt,
    append_system_prompt: agent.append_system_prompt,
    tools: agent.tools === null ? null : [...agent.tools],
    max_turns: agent.max_turns,
    effort: agent.effort,
    yolo: agent.yolo,
  });
}

function remoteToolSpecBody(spec: RemoteToolSpec): JsonObject {
  return jsonBody({
    name: spec.name,
    description: spec.description,
    llm_name: spec.llm_name,
    params: spec.params,
  });
}

function preview(body: string): string {
  const trimmed = body.trim();
  return trimmed.length <= ERROR_BODY_PREVIEW ? trimmed : `${trimmed.slice(0, ERROR_BODY_PREVIEW)}…`;
}
