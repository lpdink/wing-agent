// sdk/src/events.ts — WingEvent discriminated union.
//
// Mirrors:
//   - wing/event/base.py       (WingEvent, ErrorEvent, DeliveredEvent)
//   - wing/event/react.py      (TextEvent … TurnResultEvent)
//   - wing/event/state_change.py (SyncSessionEvent … SessionStateChangedEvent)
//   - wing/event/query_response.py (ContextStatsEvent, BranchTargetsEvent)

import type { AgentInfo, BranchTargetInfo, EventTarget } from './types'

// ============================================================
// Base fields (present on every event)
// ============================================================

export interface WingEventBase {
  created_at: string
  type: string
  session_id: string | null
  request_id: string
  target?: EventTarget | null
}

// ============================================================
// System events (base.py)
// ============================================================

export interface ErrorEvent extends WingEventBase {
  type: 'error'
  status_code: number
  message: string
  error_code: string | null
  detail: string | null
}

export interface DeliveredEvent extends WingEventBase {
  type: 'delivered'
}

// ============================================================
// React events (react.py)
// ============================================================

export interface TextEvent extends WingEventBase {
  type: 'text'
  content: string
}

export interface ReasoningEvent extends WingEventBase {
  type: 'reasoning'
  content: string
}

export interface ToolCallEvent extends WingEventBase {
  type: 'tool_call'
  tool_name: string
  tool_args: Record<string, unknown>
  tool_call_id: string
}

export interface ToolCallResultEvent extends WingEventBase {
  type: 'tool_call_result'
  tool_name: string
  tool_args: Record<string, unknown>
  tool_call_id: string
  tool_result: string
  tool_success: boolean
  model: string
}

export interface LLMCallMetricsEvent extends WingEventBase {
  type: 'llm_call_metrics'
  model: string
  prompt_tokens: number
  completion_tokens: number
  cached_tokens: number
  first_chunk_rt_ms: number
  tokens_per_sec: number
}

export interface AskEvent extends WingEventBase {
  type: 'ask'
  question: string
  choices: string[]
  required: boolean
}

export interface DoneEvent extends WingEventBase {
  type: 'done'
}

export interface TurnStartedEvent extends WingEventBase {
  type: 'turn_started'
}

export interface DiffContentEvent extends WingEventBase {
  type: 'diff_content'
  path: string
  old_text: string | null
  new_text: string
}

export interface AssistantTurnEvent extends WingEventBase {
  type: 'assistant_turn'
  uuid: string
  content_blocks: Record<string, unknown>[]
  model: string
  stop_reason: string | null
  usage: Record<string, unknown> | null
}

export interface ToolResultTurnEvent extends WingEventBase {
  type: 'tool_result_turn'
  uuid: string
  tool_use_id: string
  tool_name: string
  content: string
  is_error: boolean
}

export interface TurnResultEvent extends WingEventBase {
  type: 'turn_result'
  uuid: string
  subtype: string
  is_error: boolean
  result: string | null
  num_turns: number
  duration_ms: number
  usage: Record<string, unknown> | null
  errors: string[]
}

// ============================================================
// State change events (state_change.py)
// ============================================================

export interface SyncSessionEvent extends WingEventBase {
  type: 'sync_session'
  session_id: string
  messages: Record<string, unknown>[]
  agent: AgentInfo | null
  name: string | null
  draft: string | null
}

export interface SessionStateChangedEvent extends WingEventBase {
  type: 'session_state_changed'
  model: string | null
  thinking: boolean | null
  reasoning_effort: string | null
  yolo: boolean | null
  title: string | null
  agent: string | null
}

export interface InterruptedEvent extends WingEventBase {
  type: 'interrupted'
}

export interface CompactDoneEvent extends WingEventBase {
  type: 'compact_done'
  original_tokens: number
  compressed_tokens: number
  model: string
}

export interface SessionInitEvent extends WingEventBase {
  type: 'session_init'
  uuid: string
  tools: string[]
  model: string
  permission_mode: string
  cwd: string
}

// ============================================================
// Query response events (query_response.py)
// ============================================================

export interface ContextStatsEvent extends WingEventBase {
  type: 'context_stats'
  message_count: number
  total_tokens: number
  context_window_tokens: number
  system_prompt_parts: string[]
}

export interface BranchTargetsEvent extends WingEventBase {
  type: 'branch_targets'
  targets: BranchTargetInfo[]
}

// ============================================================
// Discriminated union
// ============================================================

export type WingEvent =
  | ErrorEvent
  | DeliveredEvent
  | TextEvent
  | ReasoningEvent
  | ToolCallEvent
  | ToolCallResultEvent
  | LLMCallMetricsEvent
  | AskEvent
  | DoneEvent
  | TurnStartedEvent
  | DiffContentEvent
  | AssistantTurnEvent
  | ToolResultTurnEvent
  | TurnResultEvent
  | SyncSessionEvent
  | SessionStateChangedEvent
  | InterruptedEvent
  | CompactDoneEvent
  | SessionInitEvent
  | ContextStatsEvent
  | BranchTargetsEvent
