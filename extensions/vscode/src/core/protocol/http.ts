/**
 * HTTP request / response models — the typed mirror of `wing/gateway/protocol.py`.
 *
 * Field names, nullability and defaults follow the Pydantic models one by one;
 * each docstring names the class it mirrors, so a reviewer can diff this file
 * against the Python source directly.
 *
 * Requests are *encoded* by `http-client.ts` (null fields are omitted, like
 * `skip_serializing_if = "Option::is_none"`). Responses are **decoded here,
 * never cast**: a required field missing from a 2xx body surfaces as
 * `GatewayHttpError{kind:'malformed-response'}` instead of poisoning the host
 * with wrong-typed data. Optional fields (Pydantic defaults) are tolerant.
 */

import {
  type JsonObject,
  ProtocolDecodeError,
  booleanOr,
  decodeEach,
  enumOr,
  isJsonObject,
  numberOr,
  optString,
  readJsonArray,
  readStringArray,
  reqBoolean,
  reqNumber,
  reqString,
  stringOr,
} from './json';
import { type AgentInfo, type BranchTarget, decodeAgentInfo, decodeBranchTarget } from './models';
import { type SessionMessage, decodeSessionMessage } from './history';

// ============================================================
// Shared
// ============================================================

/** `SessionStatus` (`wing/event/base.py`) — the backend's runtime status vocabulary. */
export const SESSION_STATUSES = ['inactive', 'idle', 'working', 'waiting'] as const;
export type SessionStatus = (typeof SESSION_STATUSES)[number];

/** `wing/event/base.py::SessionInfo` — one row of the session list. */
export interface SessionInfo {
  readonly id: string;
  readonly name: string | null;
  /** ISO-8601 string (`datetime` on the wire). */
  readonly created_at: string | null;
  readonly template_name: string | null;
  readonly workspace: string | null;
  readonly last_interaction: string | null;
  readonly status: SessionStatus;
}

/** `wing/event/base.py::CommandInfo` — one prompt command. */
export interface CommandInfo {
  readonly name: string;
  readonly aliases: readonly string[];
  readonly description: string;
  readonly params: string;
}

/** `protocol.py::ContextStatsInfo`. */
export interface ContextStatsInfo {
  readonly message_count: number;
  readonly total_tokens: number;
}

/** `protocol.py::ToolInfo` — one registered tool. */
export interface ToolInfo {
  readonly ref: string;
  readonly namespace: string;
  readonly name: string;
  readonly llm_name: string;
  readonly description: string;
}

// ============================================================
// Requests
// ============================================================

/** `protocol.py::AgentOverride`. `null` means "keep the template's value". */
export interface AgentOverride {
  readonly model: string | null;
  readonly provider: string | null;
  readonly system_prompt: string | null;
  readonly append_system_prompt: string | null;
  readonly tools: readonly string[] | null;
  readonly max_turns: number | null;
  /** `low | medium | high | xhigh | max`. */
  readonly effort: string | null;
  readonly yolo: boolean | null;
}

/** `protocol.py::CreateSessionRequest`. */
export interface CreateSessionRequest {
  readonly template_name: string | null;
  readonly workspace: string | null;
  readonly agent: AgentOverride | null;
  /** `file` (durable) | `memory` (process-local). */
  readonly backend: string | null;
}

/** `protocol.py::ResumeSessionRequest`. */
export interface ResumeSessionRequest {
  readonly session_id: string;
}

/** `protocol.py::ForkSessionRequest`. */
export interface ForkSessionRequest {
  readonly source_session_id: string;
  readonly target_uuid: string;
}

/** `protocol.py::SubscribeRequest` (needs `X-Client-Id`). */
export interface SubscribeRequest {
  readonly session_id: string;
}

/** `protocol.py::UnsubscribeRequest` (needs `X-Client-Id`). */
export interface UnsubscribeRequest {
  readonly session_id: string;
}

/** `protocol.py::SendMessageRequest`. */
export interface SendMessageRequest {
  readonly session_id: string;
  readonly content: string;
  readonly tool_call_id: string | null;
}

/** `protocol.py::CompactRequest`. */
export interface CompactRequest {
  readonly session_id: string;
  readonly instruction: string | null;
}

/** `protocol.py::InterruptRequest`. */
export interface InterruptRequest {
  readonly session_id: string;
}

/** `protocol.py::RewindRequest`. */
export interface RewindRequest {
  readonly session_id: string;
  readonly target_uuid: string;
}

/** `protocol.py::UpdateSessionRequest` — every field optional, at least one required. */
export interface UpdateSessionRequest {
  readonly session_id: string;
  readonly model: string | null;
  readonly provider: string | null;
  readonly agent: string | null;
  readonly title: string | null;
  readonly thinking: boolean | null;
  readonly reasoning_effort: string | null;
  readonly yolo: boolean | null;
  readonly workspace: string | null;
  readonly tools: readonly string[] | null;
}

/** `protocol.py::RemoteToolSpec`. */
export interface RemoteToolSpec {
  readonly name: string;
  readonly description: string;
  readonly llm_name: string | null;
  readonly params: readonly JsonObject[];
}

/** `protocol.py::RegisterToolsRequest` (needs `X-Client-Id`). */
export interface RegisterToolsRequest {
  readonly tools: readonly RemoteToolSpec[];
}

// ============================================================
// Responses
// ============================================================

/** `protocol.py::CreateSessionResponse`. */
export interface CreateSessionResponse {
  readonly session_id: string;
  readonly template_name: string;
  readonly workspace: string | null;
  readonly backend: string;
}

/** `protocol.py::ResumeSessionResponse`. */
export interface ResumeSessionResponse {
  readonly session_id: string;
  readonly template_name: string | null;
  readonly workspace: string | null;
}

/** `protocol.py::ForkSessionResponse`. */
export interface ForkSessionResponse {
  readonly session_id: string;
  readonly draft: string | null;
}

/** `protocol.py::OkResponse`. */
export interface OkResponse {
  readonly ok: boolean;
}

/** `protocol.py::SendMessageResponse`. */
export interface SendMessageResponse {
  readonly ok: boolean;
  readonly request_id: string;
}

/** `protocol.py::UpdateSessionResponse`. */
export interface UpdateSessionResponse {
  readonly ok: boolean;
}

/** `protocol.py::RegisterToolsResponse`. */
export interface RegisterToolsResponse {
  readonly ok: boolean;
  readonly registered: readonly string[];
}

/** `protocol.py::SessionListResponse`. */
export interface SessionListResponse {
  readonly sessions: readonly SessionInfo[];
}

/** `protocol.py::SessionGetResponse`. */
export interface SessionGetResponse {
  readonly session_id: string;
  readonly name: string | null;
  readonly template_name: string | null;
  readonly workspace: string | null;
  readonly status: SessionStatus;
  readonly messages: readonly SessionMessage[];
  readonly agent: AgentInfo | null;
}

/** `protocol.py::SessionInfoResponse` — `GET /api/session/info`. */
export interface SessionInfoResponse {
  readonly model: string;
  readonly api_url: string;
  readonly tools: readonly string[];
  readonly total_tokens: number;
  readonly context_window_tokens: number;
  readonly thinking: boolean;
  readonly reasoning_effort: string | null;
  readonly yolo: boolean;
  readonly session_name: string | null;
  readonly workdir: string | null;
  readonly status: SessionStatus;
  readonly context_stats: ContextStatsInfo;
  readonly skills_info: string;
  readonly system_prompt: string;
}

/** `protocol.py::CompactResponse`. */
export interface CompactResponse {
  readonly ok: boolean;
  readonly original_tokens: number;
  readonly compressed_tokens: number;
}

/** `protocol.py::RewindResponse`. */
export interface RewindResponse {
  readonly ok: boolean;
  readonly draft: string | null;
}

/** `protocol.py::ReloadResultItem`. */
export interface ReloadResultItem {
  readonly name: string;
  readonly ok: boolean;
  readonly detail: string | null;
}

/** `protocol.py::ReloadResponse`. */
export interface ReloadResponse {
  readonly ok: boolean;
  readonly results: readonly ReloadResultItem[];
}

/** `protocol.py::BranchesResponse`. */
export interface BranchesResponse {
  readonly targets: readonly BranchTarget[];
}

/** `protocol.py::CommandsResponse`. */
export interface CommandsResponse {
  readonly commands: readonly CommandInfo[];
}

/** `protocol.py::ProviderModels`. */
export interface ProviderModels {
  readonly provider: string;
  readonly models: readonly string[];
}

/** `protocol.py::ModelsResponse`. */
export interface ModelsResponse {
  readonly providers: readonly ProviderModels[];
}

/** `protocol.py::AgentsResponse`. */
export interface AgentsResponse {
  readonly agents: readonly string[];
  readonly default_agent: string;
}

/** `protocol.py::ToolsListResponse`. */
export interface ToolsListResponse {
  readonly tools: readonly ToolInfo[];
}

/** `protocol.py::HealthResponse`. */
export interface HealthResponse {
  readonly service: string;
  readonly status: string;
  readonly version: string;
  readonly commit: string | null;
  readonly uptime: number;
}

/**
 * `POST /api/shutdown` answer.
 *
 * The only endpoint whose body is *not* a Pydantic model (`routes/system.py`
 * returns a bare `{"status": "shutting_down"}` dict) — mirrored as-is.
 */
export interface ShutdownResponse {
  readonly status: string;
}

/** `protocol.py::ErrorResponse` — the single shape every failure uses. */
export interface ErrorResponse {
  readonly error: string;
  readonly detail: string | null;
  readonly session_id: string | null;
  readonly uuid: string | null;
}

// ============================================================
// Decoders
// ============================================================

function requireObject(value: JsonObject, key: string): JsonObject {
  const nested = value[key];
  if (!isJsonObject(nested)) {
    throw new ProtocolDecodeError(`response field "${key}" must be an object`);
  }
  return nested;
}

/** Shared summary decoder (`SessionInfo` is nested in list / get responses). */
function decodeSessionInfo(value: unknown): SessionInfo | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    id: reqString(value, 'id'),
    name: optString(value, 'name'),
    created_at: optString(value, 'created_at'),
    template_name: optString(value, 'template_name'),
    workspace: optString(value, 'workspace'),
    last_interaction: optString(value, 'last_interaction'),
    // Pydantic default is "inactive"; unknown values degrade instead of throwing.
    status: enumOr(value, 'status', SESSION_STATUSES, 'inactive'),
  };
}

export function decodeCreateSessionResponse(value: unknown): CreateSessionResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    session_id: reqString(value, 'session_id'),
    template_name: stringOr(value, 'template_name', ''),
    workspace: optString(value, 'workspace'),
    backend: stringOr(value, 'backend', 'file'),
  };
}

export function decodeResumeSessionResponse(value: unknown): ResumeSessionResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    session_id: reqString(value, 'session_id'),
    template_name: optString(value, 'template_name'),
    workspace: optString(value, 'workspace'),
  };
}

export function decodeForkSessionResponse(value: unknown): ForkSessionResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { session_id: reqString(value, 'session_id'), draft: optString(value, 'draft') };
}

export function decodeOkResponse(value: unknown): OkResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { ok: booleanOr(value, 'ok', true) };
}

export function decodeSendMessageResponse(value: unknown): SendMessageResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { ok: booleanOr(value, 'ok', true), request_id: reqString(value, 'request_id') };
}

export function decodeUpdateSessionResponse(value: unknown): UpdateSessionResponse | null {
  return decodeOkResponse(value);
}

export function decodeRegisterToolsResponse(value: unknown): RegisterToolsResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { ok: booleanOr(value, 'ok', true), registered: readStringArray(value, 'registered') };
}

export function decodeSessionListResponse(value: unknown): SessionListResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { sessions: decodeEach(readJsonArray(value, 'sessions'), decodeSessionInfo) };
}

export function decodeSessionGetResponse(value: unknown): SessionGetResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    session_id: reqString(value, 'session_id'),
    name: optString(value, 'name'),
    template_name: optString(value, 'template_name'),
    workspace: optString(value, 'workspace'),
    status: enumOr(value, 'status', SESSION_STATUSES, 'idle'),
    messages: decodeEach(readJsonArray(value, 'messages'), decodeSessionMessage),
    agent: decodeAgentInfo(value['agent']),
  };
}

export function decodeSessionInfoResponse(value: unknown): SessionInfoResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  const stats = requireObject(value, 'context_stats');
  return {
    model: reqString(value, 'model'),
    api_url: reqString(value, 'api_url'),
    tools: readStringArray(value, 'tools'),
    total_tokens: reqNumber(value, 'total_tokens'),
    context_window_tokens: reqNumber(value, 'context_window_tokens'),
    thinking: reqBoolean(value, 'thinking'),
    reasoning_effort: optString(value, 'reasoning_effort'),
    yolo: reqBoolean(value, 'yolo'),
    session_name: optString(value, 'session_name'),
    workdir: optString(value, 'workdir'),
    status: enumOr(value, 'status', SESSION_STATUSES, 'idle'),
    context_stats: {
      message_count: reqNumber(stats, 'message_count'),
      total_tokens: reqNumber(stats, 'total_tokens'),
    },
    skills_info: stringOr(value, 'skills_info', ''),
    system_prompt: stringOr(value, 'system_prompt', ''),
  };
}

export function decodeCompactResponse(value: unknown): CompactResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    ok: booleanOr(value, 'ok', true),
    original_tokens: numberOr(value, 'original_tokens', 0),
    compressed_tokens: numberOr(value, 'compressed_tokens', 0),
  };
}

export function decodeRewindResponse(value: unknown): RewindResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { ok: booleanOr(value, 'ok', true), draft: optString(value, 'draft') };
}

function decodeReloadResult(value: unknown): ReloadResultItem | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    name: reqString(value, 'name'),
    ok: reqBoolean(value, 'ok'),
    detail: optString(value, 'detail'),
  };
}

export function decodeReloadResponse(value: unknown): ReloadResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    ok: reqBoolean(value, 'ok'),
    results: decodeEach(readJsonArray(value, 'results'), decodeReloadResult),
  };
}

export function decodeBranchesResponse(value: unknown): BranchesResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { targets: decodeEach(readJsonArray(value, 'targets'), decodeBranchTarget) };
}

function decodeCommandInfo(value: unknown): CommandInfo | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    name: reqString(value, 'name'),
    aliases: readStringArray(value, 'aliases'),
    description: stringOr(value, 'description', ''),
    params: stringOr(value, 'params', ''),
  };
}

export function decodeCommandsResponse(value: unknown): CommandsResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { commands: decodeEach(readJsonArray(value, 'commands'), decodeCommandInfo) };
}

function decodeProviderModels(value: unknown): ProviderModels | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { provider: reqString(value, 'provider'), models: readStringArray(value, 'models') };
}

export function decodeModelsResponse(value: unknown): ModelsResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { providers: decodeEach(readJsonArray(value, 'providers'), decodeProviderModels) };
}

export function decodeAgentsResponse(value: unknown): AgentsResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    agents: readStringArray(value, 'agents'),
    default_agent: reqString(value, 'default_agent'),
  };
}

function decodeToolInfo(value: unknown): ToolInfo | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    ref: reqString(value, 'ref'),
    namespace: reqString(value, 'namespace'),
    name: reqString(value, 'name'),
    llm_name: reqString(value, 'llm_name'),
    description: stringOr(value, 'description', ''),
  };
}

export function decodeToolsListResponse(value: unknown): ToolsListResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { tools: decodeEach(readJsonArray(value, 'tools'), decodeToolInfo) };
}

export function decodeHealthResponse(value: unknown): HealthResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return {
    service: stringOr(value, 'service', 'wing-gateway'),
    status: stringOr(value, 'status', 'ok'),
    version: reqString(value, 'version'),
    commit: optString(value, 'commit'),
    uptime: reqNumber(value, 'uptime'),
  };
}

export function decodeShutdownResponse(value: unknown): ShutdownResponse | null {
  if (!isJsonObject(value)) {
    return null;
  }
  return { status: reqString(value, 'status') };
}

/**
 * Decode the shared error shape.
 *
 * Returns `null` for bodies that are not an `ErrorResponse` — the caller keeps
 * the raw text in that case (`GatewayHttpError.rawBody`), matching the Rust
 * client's `extract_api_error`.
 */
export function decodeErrorResponse(value: unknown): ErrorResponse | null {
  if (!isJsonObject(value) || typeof value['error'] !== 'string') {
    return null;
  }
  return {
    error: reqString(value, 'error'),
    detail: optString(value, 'detail'),
    session_id: optString(value, 'session_id'),
    uuid: optString(value, 'uuid'),
  };
}
