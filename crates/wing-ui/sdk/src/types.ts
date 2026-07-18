// sdk/src/types.ts — Shared data types used across events and protocol.
//
// Mirrors:
//   - wing/event/base.py (SessionInfo, AgentInfo, CommandInfo, EventTarget)
//   - wing/event/query_response.py (BranchTargetInfo)

/** Routing target injected by EventBus. SDK consumers can ignore this. */
export interface EventTarget {
  scope: 'global' | 'session' | 'client'
  client_ids: string[]
}

/** Session summary info for list/detail views. */
export interface SessionInfo {
  id: string
  name: string | null
  created_at: string | null
  template_name: string | null
  workspace: string | null
  last_interaction: string | null
}

/** Agent configuration snapshot. */
export interface AgentInfo {
  model_name: string
  system_prompt: string | null
  tools: string[]
  skills: string[]
  rules: string[]
  workspace: string | null
}

/** Magic command metadata. */
export interface CommandInfo {
  name: string
  aliases: string[]
  description: string
  params: string
}

/** A forkable/rewindable message node. */
export interface BranchTargetInfo {
  uuid: string
  content: string
  role: string
}
