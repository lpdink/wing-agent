// sdk/src/protocol.ts — HTTP Request/Response types and WS protocol.
//
// Mirrors: wing/gateway/protocol.py

import type { AgentInfo, BranchTargetInfo, CommandInfo, SessionInfo } from './types'

// ============================================================
// WS Protocol
// ============================================================

/** First message from server after WS connection — carries client_id. */
export interface ConnectResponse {
  type: string
  client_id: string
}

/** Client request sent over WS. Gateway forwards to WingRuntime. */
export interface ClientRequest {
  request_id: string
  session_id: string
  content: string
}

// ============================================================
// HTTP Request Models
// ============================================================

/** Agent parameter overrides. null fields are not sent (template value kept). */
export interface AgentOverride {
  model?: string | null
  system_prompt?: string | null
  append_system_prompt?: string | null
  tools?: string[] | null
  max_turns?: number | null
  effort?: string | null
  yolo?: boolean | null
}

export interface CreateSessionRequest {
  template_name?: string | null
  workspace?: string | null
  agent?: AgentOverride | null
}

export interface ResumeSessionRequest {
  session_id: string
}

export interface ForkSessionRequest {
  source_session_id: string
  target_uuid: string
}

export interface SubscribeRequest {
  session_id: string
}

export interface UnsubscribeRequest {
  session_id: string
}

export interface SendMessageRequest {
  session_id: string
  content: string
}

export interface CompactRequest {
  session_id: string
}

export interface InterruptRequest {
  session_id: string
}

export interface RewindRequest {
  session_id: string
  target_uuid: string
}

export interface UpdateSessionRequest {
  session_id: string
  model?: string | null
  agent?: string | null
  title?: string | null
  thinking?: boolean | null
  reasoning_effort?: string | null
  yolo?: boolean | null
}

// ============================================================
// HTTP Response Models
// ============================================================

export interface CreateSessionResponse {
  session_id: string
  template_name: string
  workspace: string | null
}

export interface ResumeSessionResponse {
  session_id: string
  template_name: string | null
  workspace: string | null
}

export interface ForkSessionResponse {
  session_id: string
  draft: string | null
}

export interface OkResponse {
  ok: boolean
}

export interface SendMessageResponse {
  ok: boolean
  request_id: string
}

export interface SessionListResponse {
  sessions: SessionInfo[]
}

export interface SessionGetResponse {
  session_id: string
  name: string | null
  template_name: string | null
  workspace: string | null
  messages: Record<string, unknown>[]
  agent: AgentInfo | null
}

export interface HealthResponse {
  service: string
  status: string
  version: string
  uptime: number
}

export interface ErrorResponse {
  error: string
  detail: string | null
  session_id: string | null
  uuid: string | null
}

// ============================================================
// Session Query Responses
// ============================================================

export interface ContextStatsInfo {
  message_count: number
  total_tokens: number
}

export interface SessionInfoResponse {
  model: string
  api_url: string
  tools: string[]
  total_tokens: number
  context_window_tokens: number
  thinking: boolean
  reasoning_effort: string | null
  yolo: boolean
  session_name: string | null
  context_stats: ContextStatsInfo
  skills_info: string
  system_prompt: string
}

export interface CompactResponse {
  ok: boolean
  original_tokens: number
  compressed_tokens: number
}

export interface RewindResponse {
  ok: boolean
  draft: string | null
}

export interface ReloadResultItem {
  name: string
  ok: boolean
  detail: string | null
}

export interface ReloadResponse {
  ok: boolean
  results: ReloadResultItem[]
}

export interface BranchesResponse {
  targets: BranchTargetInfo[]
}

export interface UpdateSessionResponse {
  ok: boolean
}

// ============================================================
// System Query Responses
// ============================================================

export interface CommandsResponse {
  commands: CommandInfo[]
}

export interface ModelsResponse {
  models: string[]
}

export interface AgentsResponse {
  agents: string[]
  default_agent: string
}
