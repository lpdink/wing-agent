// sdk/src/client.ts — GatewayClient: typed HTTP client for all Gateway endpoints.
//
// Mirrors:
//   - wing/gateway/routes/session.py  (14 session endpoints)
//   - wing/gateway/routes/health.py   (1 health endpoint)
//   - wing/gateway/routes/system.py   (5 system endpoints)
//   - crates/wing-api-client/src/client.rs (API design reference)

import { ApiClientError, extractApiError } from './errors'
import type {
  AgentsResponse,
  BranchesResponse,
  CommandsResponse,
  CompactRequest,
  CompactResponse,
  CreateSessionRequest,
  CreateSessionResponse,
  ForkSessionRequest,
  ForkSessionResponse,
  HealthResponse,
  InterruptRequest,
  ModelsResponse,
  OkResponse,
  ReloadResponse,
  ResumeSessionRequest,
  ResumeSessionResponse,
  RewindRequest,
  RewindResponse,
  SendMessageRequest,
  SendMessageResponse,
  SessionGetResponse,
  SessionInfoResponse,
  SessionListResponse,
  SubscribeRequest,
  UnsubscribeRequest,
  UpdateSessionRequest,
  UpdateSessionResponse,
} from './protocol'

export interface GatewayClientOptions {
  /** Gateway base URL, e.g. "http://127.0.0.1:32523" */
  baseUrl: string
  /** Client ID obtained from WS ConnectResponse. Can be set later. */
  clientId?: string | null
}

/**
 * Typed HTTP client for Wing Gateway API.
 *
 * Uses native `fetch` — zero dependencies.
 * subscribe/unsubscribe automatically inject `X-Client-Id` header.
 */
export class GatewayClient {
  private baseUrl: string
  clientId: string | null

  constructor(options: GatewayClientOptions) {
    // Strip trailing slash
    this.baseUrl = options.baseUrl.replace(/\/+$/, '')
    this.clientId = options.clientId ?? null
  }

  // ============================================================
  // Session Lifecycle
  // ============================================================

  async createSession(req: CreateSessionRequest = {}): Promise<CreateSessionResponse> {
    return this.postJson('/api/session/create', req)
  }

  async resumeSession(sessionId: string): Promise<ResumeSessionResponse> {
    const body: ResumeSessionRequest = { session_id: sessionId }
    return this.postJson('/api/session/resume', body)
  }

  async forkSession(sourceSessionId: string, targetUuid: string): Promise<ForkSessionResponse> {
    const body: ForkSessionRequest = {
      source_session_id: sourceSessionId,
      target_uuid: targetUuid,
    }
    return this.postJson('/api/session/fork', body)
  }

  // ============================================================
  // Subscription Management
  // ============================================================

  async subscribe(sessionId: string): Promise<OkResponse> {
    const body: SubscribeRequest = { session_id: sessionId }
    return this.postJsonWithClientId('/api/session/subscribe', body)
  }

  async unsubscribe(sessionId: string): Promise<OkResponse> {
    const body: UnsubscribeRequest = { session_id: sessionId }
    return this.postJsonWithClientId('/api/session/unsubscribe', body)
  }

  // ============================================================
  // Message Sending
  // ============================================================

  async sendMessage(sessionId: string, content: string): Promise<SendMessageResponse> {
    const body: SendMessageRequest = {
      session_id: sessionId,
      content,
    }
    return this.postJson('/api/session/send', body)
  }

  // ============================================================
  // Queries
  // ============================================================

  async listSessions(): Promise<SessionListResponse> {
    return this.getJson('/api/session/list')
  }

  async getSession(sessionId: string): Promise<SessionGetResponse> {
    return this.getJson('/api/session/get', { session_id: sessionId })
  }

  async getSessionInfo(sessionId: string): Promise<SessionInfoResponse> {
    return this.getJson('/api/session/info', { session_id: sessionId })
  }

  async getBranches(sessionId: string): Promise<BranchesResponse> {
    return this.getJson('/api/session/branches', { session_id: sessionId })
  }

  // ============================================================
  // Session Update
  // ============================================================

  async updateSession(req: UpdateSessionRequest): Promise<UpdateSessionResponse> {
    return this.postJson('/api/session/update', req)
  }

  // ============================================================
  // Session Operations
  // ============================================================

  async compactSession(sessionId: string): Promise<CompactResponse> {
    const body: CompactRequest = { session_id: sessionId }
    return this.postJson('/api/session/compact', body)
  }

  async interruptSession(sessionId: string): Promise<OkResponse> {
    const body: InterruptRequest = { session_id: sessionId }
    return this.postJson('/api/session/interrupt', body)
  }

  async rewindSession(sessionId: string, targetUuid: string): Promise<RewindResponse> {
    const body: RewindRequest = {
      session_id: sessionId,
      target_uuid: targetUuid,
    }
    return this.postJson('/api/session/rewind', body)
  }

  // ============================================================
  // System
  // ============================================================

  async health(): Promise<HealthResponse> {
    return this.getJson('/api/health')
  }

  async getCommands(): Promise<CommandsResponse> {
    return this.getJson('/api/commands')
  }

  async getModels(): Promise<ModelsResponse> {
    return this.getJson('/api/models')
  }

  async getAgents(): Promise<AgentsResponse> {
    return this.getJson('/api/agents')
  }

  async reloadSystem(): Promise<ReloadResponse> {
    return this.postEmpty('/api/system/reload')
  }

  async shutdown(): Promise<void> {
    const resp = await this.rawPost('/api/shutdown')
    if (!resp.ok) {
      throw await extractApiError(resp)
    }
  }

  // ============================================================
  // Internal helpers
  // ============================================================

  private async getJson<T>(path: string, params?: Record<string, string>): Promise<T> {
    const url = new URL(`${this.baseUrl}${path}`)
    if (params) {
      for (const [k, v] of Object.entries(params)) {
        url.searchParams.set(k, v)
      }
    }

    let resp: Response
    try {
      resp = await fetch(url.toString())
    } catch (e) {
      throw ApiClientError.transport(e)
    }
    if (!resp.ok) {
      throw await extractApiError(resp)
    }
    return this.parseJson<T>(resp)
  }

  private async postJson<T>(path: string, body: unknown): Promise<T> {
    const resp = await this.rawPost(path, body)
    if (!resp.ok) {
      throw await extractApiError(resp)
    }
    return this.parseJson<T>(resp)
  }

  private async postJsonWithClientId<T>(path: string, body: unknown): Promise<T> {
    if (!this.clientId) {
      throw ApiClientError.transport(new Error('clientId is required for subscribe/unsubscribe'))
    }
    let resp: Response
    try {
      resp = await fetch(`${this.baseUrl}${path}`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'X-Client-Id': this.clientId,
        },
        body: JSON.stringify(body),
      })
    } catch (e) {
      throw ApiClientError.transport(e)
    }
    if (!resp.ok) {
      throw await extractApiError(resp)
    }
    return this.parseJson<T>(resp)
  }

  private async postEmpty<T>(path: string): Promise<T> {
    let resp: Response
    try {
      resp = await fetch(`${this.baseUrl}${path}`, { method: 'POST' })
    } catch (e) {
      throw ApiClientError.transport(e)
    }
    if (!resp.ok) {
      throw await extractApiError(resp)
    }
    return this.parseJson<T>(resp)
  }

  private async rawPost(path: string, body?: unknown): Promise<Response> {
    try {
      return await fetch(`${this.baseUrl}${path}`, {
        method: 'POST',
        headers: body !== undefined ? { 'Content-Type': 'application/json' } : undefined,
        body: body !== undefined ? JSON.stringify(body) : undefined,
      })
    } catch (e) {
      throw ApiClientError.transport(e)
    }
  }

  private async parseJson<T>(resp: Response): Promise<T> {
    try {
      return (await resp.json()) as T
    } catch (e) {
      throw ApiClientError.deserialize(e)
    }
  }
}
