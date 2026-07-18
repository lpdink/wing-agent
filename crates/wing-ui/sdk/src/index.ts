// @wing-agent/sdk — TypeScript client for Wing Agent Gateway

// Shared data types
export type { AgentInfo, BranchTargetInfo, CommandInfo, EventTarget, SessionInfo } from './types'

// Event types (discriminated union)
export type {
  WingEvent,
  WingEventBase,
  AskEvent,
  AssistantTurnEvent,
  BranchTargetsEvent,
  CompactDoneEvent,
  ContextStatsEvent,
  DeliveredEvent,
  DiffContentEvent,
  DoneEvent,
  ErrorEvent,
  InterruptedEvent,
  LLMCallMetricsEvent,
  ReasoningEvent,
  SessionInitEvent,
  SessionStateChangedEvent,
  SyncSessionEvent,
  TextEvent,
  ToolCallEvent,
  ToolCallResultEvent,
  ToolResultTurnEvent,
  TurnResultEvent,
  TurnStartedEvent,
} from './events'

// Protocol types (HTTP request/response + WS)
export type {
  AgentsResponse,
  AgentOverride,
  BranchesResponse,
  ClientRequest,
  CommandsResponse,
  CompactRequest,
  CompactResponse,
  ConnectResponse,
  ContextStatsInfo,
  CreateSessionRequest,
  CreateSessionResponse,
  ErrorResponse,
  ForkSessionRequest,
  ForkSessionResponse,
  HealthResponse,
  InterruptRequest,
  ModelsResponse,
  OkResponse,
  ReloadResponse,
  ReloadResultItem,
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

// Error type
export { ApiClientError } from './errors'
export type { ApiClientErrorKind } from './errors'

// HTTP client
export { GatewayClient } from './client'
export type { GatewayClientOptions } from './client'

// WebSocket client
export { WebSocketClient } from './websocket'
export type { ConnectionStatus, WebSocketClientOptions } from './websocket'
