// src/stores/sessionStore.ts — Session and message state.

import { create } from 'zustand'
import type { SessionInfo, SessionInfoResponse } from '@wing-agent/sdk'

// ============================================================
// ChatItem — unified display type for messages + streaming events
// ============================================================

export interface UserChatItem {
  type: 'user'
  id: string
  content: string
}

export interface AssistantChatItem {
  type: 'assistant'
  id: string
  content: string
  /** True if still streaming (more text events expected) */
  streaming?: boolean
  /** Backend message uuid (stamped when the LLM turn completes) */
  messageUuid?: string
}

export interface ToolCallChatItem {
  type: 'tool_call'
  id: string
  toolName: string
  toolArgs: Record<string, unknown>
  toolCallId: string
}

export interface ToolResultChatItem {
  type: 'tool_call_result'
  id: string
  toolName: string
  toolCallId: string
  result: string
  success: boolean
}

export interface ReasoningChatItem {
  type: 'reasoning'
  id: string
  content: string
  /** True if still streaming (more reasoning events expected) */
  streaming?: boolean
  /** Backend message uuid (stamped when the LLM turn completes) */
  messageUuid?: string
}

export interface AskChatItem {
  type: 'ask'
  id: string
  question: string
  choices: string[]
  /** True after user has answered */
  answered?: boolean
  /** The choice the user selected */
  selectedChoice?: string
}

export interface DiffChatItem {
  type: 'diff'
  id: string
  path: string
  oldText: string | null
  newText: string
}

/** A single tool entry within a ToolGroup. */
export interface ToolGroupEntry {
  name: string
  args: Record<string, unknown>
  result?: string
  success?: boolean
}

/** Grouped consecutive read-only tool calls (compressed summary). */
export interface ToolGroupChatItem {
  type: 'tool_group'
  id: string
  tools: ToolGroupEntry[]
  summary: string
}

export interface TurnStartedChatItem {
  type: 'turn_started'
  id: string
}

export interface DoneChatItem {
  type: 'done'
  id: string
}

export interface ErrorChatItem {
  type: 'error'
  id: string
  message: string
}

export type ChatItem =
  | UserChatItem
  | AssistantChatItem
  | ToolCallChatItem
  | ToolResultChatItem
  | ReasoningChatItem
  | TurnStartedChatItem
  | DoneChatItem
  | ErrorChatItem
  | AskChatItem
  | DiffChatItem
  | ToolGroupChatItem

// ============================================================
// Tool classification for layered rendering
// ============================================================

/** Read-only / exploration tools — compressed into group summaries. */
export const READONLY_TOOLS = new Set(['Read', 'Glob', 'Grep', 'LS', 'WebSearch'])

/** Mutation tools — rendered with diff focus. */
export const MUTATION_TOOLS = new Set(['Write', 'Edit'])

/** Execution tools — standard ToolCallCell rendering. */
export const EXECUTION_TOOLS = new Set(['Bash'])

export function getToolCategory(name: string): 'readonly' | 'mutation' | 'execution' {
  if (READONLY_TOOLS.has(name)) return 'readonly'
  if (MUTATION_TOOLS.has(name)) return 'mutation'
  return 'execution'
}

/** Build a human-readable summary for a group of read-only tools. */
export function buildToolGroupSummary(tools: ToolGroupEntry[]): string {
  const counts = new Map<string, number>()
  for (const t of tools) {
    counts.set(t.name, (counts.get(t.name) ?? 0) + 1)
  }
  const parts: string[] = []
  for (const [name, count] of counts) {
    const label = name === 'Read' ? 'file' : name === 'Grep' ? 'pattern' : 'item'
    parts.push(`${name} ${count} ${label}${count > 1 ? 's' : ''}`)
  }
  return parts.join(', ')
}

// ============================================================
// Store
// ============================================================

/** Real-time LLM metrics (updated by llm_call_metrics events). */
export interface LLMetrics {
  model: string
  promptTokens: number
  completionTokens: number
  cachedTokens: number
  tokensPerSec: number
}

/** Session config state (from sync_session / session_state_changed). */
export interface SessionConfig {
  model: string | null
  thinking: boolean | null
  reasoningEffort: string | null
  yolo: boolean | null
}

interface SessionState {
  sessions: SessionInfo[]
  activeSessionId: string | null
  messages: ChatItem[]
  isLoading: boolean
  isSending: boolean
  sessionInfo: SessionInfoResponse | null
  metrics: LLMetrics | null
  sessionConfig: SessionConfig
}

interface SessionActions {
  setSessions: (sessions: SessionInfo[]) => void
  setActiveSessionId: (id: string | null) => void
  setMessages: (messages: ChatItem[]) => void
  appendMessage: (item: ChatItem) => void
  /** Append text to the last assistant message (streaming). Creates one if none exists. */
  appendToLastAssistant: (text: string, id: string) => void
  /** Mark the last assistant message as no longer streaming. */
  finalizeStreaming: () => void
  /** Append text to the last reasoning message (streaming). Creates one if none exists. */
  appendToLastReasoning: (text: string, id: string) => void
  /** Mark the last reasoning message as no longer streaming. */
  finalizeReasoningStreaming: () => void
  /** Seal the current LLM turn: finalize streaming blocks and stamp the backend uuid. */
  sealCurrentTurn: (uuid: string) => void
  clearMessages: () => void
  setLoading: (loading: boolean) => void
  setSending: (sending: boolean) => void
  setSessionInfo: (info: SessionInfoResponse | null) => void
  /** Merge partial fields into sessionInfo (for incremental state sync). */
  patchSessionInfo: (patch: Partial<SessionInfoResponse>) => void
  /** Update real-time LLM metrics (from llm_call_metrics events). */
  setMetrics: (metrics: LLMetrics | null) => void
  /** Update session config state (from sync_session / session_state_changed). */
  setSessionConfig: (config: Partial<SessionConfig>) => void
  /** Reset metrics and config (on session switch). */
  resetSessionState: () => void
  /** Append a readonly tool call to the current group (or start a new group). */
  appendReadonlyTool: (name: string, args: Record<string, unknown>, toolCallId: string) => void
  /** Attach a result to a readonly tool in the current group. */
  appendReadonlyToolResult: (
    name: string,
    toolCallId: string,
    result: string,
    success: boolean,
  ) => void
}

export type SessionStore = SessionState & SessionActions

export const useSessionStore = create<SessionStore>((set) => ({
  // State
  sessions: [],
  activeSessionId: null,
  messages: [],
  isLoading: false,
  isSending: false,
  sessionInfo: null,
  metrics: null,
  sessionConfig: { model: null, thinking: null, reasoningEffort: null, yolo: null },

  // Actions
  setSessions: (sessions) => set({ sessions }),

  setActiveSessionId: (id) => set({ activeSessionId: id }),

  setMessages: (messages) => set({ messages }),

  appendMessage: (item) => set((state) => ({ messages: [...state.messages, item] })),

  appendToLastAssistant: (text, id) =>
    set((state) => {
      const msgs = [...state.messages]
      // Search backward for the last assistant message that is still streaming.
      // Reasoning and text events can interleave within the same LLM call,
      // so the last message may not be an assistant — we must search back.
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'assistant' && (msgs[i] as AssistantChatItem).streaming) {
          const target = msgs[i] as AssistantChatItem
          msgs[i] = { ...target, content: target.content + text, streaming: true }
          return { messages: msgs }
        }
      }
      msgs.push({ type: 'assistant', id, content: text, streaming: true })
      return { messages: msgs }
    }),

  finalizeStreaming: () =>
    set((state) => {
      const msgs = [...state.messages]
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'assistant' && (msgs[i] as AssistantChatItem).streaming) {
          msgs[i] = { ...(msgs[i] as AssistantChatItem), streaming: false }
          return { messages: msgs }
        }
      }
      return { messages: msgs }
    }),

  appendToLastReasoning: (text, id) =>
    set((state) => {
      const msgs = [...state.messages]
      // Search backward for the last reasoning message that is still streaming.
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'reasoning' && (msgs[i] as ReasoningChatItem).streaming) {
          const target = msgs[i] as ReasoningChatItem
          msgs[i] = { ...target, content: target.content + text, streaming: true }
          return { messages: msgs }
        }
      }
      msgs.push({ type: 'reasoning', id, content: text, streaming: true })
      return { messages: msgs }
    }),

  finalizeReasoningStreaming: () =>
    set((state) => {
      const msgs = [...state.messages]
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'reasoning' && (msgs[i] as ReasoningChatItem).streaming) {
          msgs[i] = { ...(msgs[i] as ReasoningChatItem), streaming: false }
          return { messages: msgs }
        }
      }
      return { messages: msgs }
    }),

  sealCurrentTurn: (uuid) =>
    set((state) => {
      const msgs = [...state.messages]
      // Seal the last streaming assistant block
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'assistant' && (msgs[i] as AssistantChatItem).streaming) {
          msgs[i] = { ...(msgs[i] as AssistantChatItem), streaming: false, messageUuid: uuid }
          break
        }
      }
      // Seal the last streaming reasoning block
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'reasoning' && (msgs[i] as ReasoningChatItem).streaming) {
          msgs[i] = { ...(msgs[i] as ReasoningChatItem), streaming: false, messageUuid: uuid }
          break
        }
      }
      return { messages: msgs }
    }),

  clearMessages: () => set({ messages: [], isSending: false }),

  setLoading: (loading) => set({ isLoading: loading }),

  setSending: (sending) => set({ isSending: sending }),

  setSessionInfo: (info) => set({ sessionInfo: info }),

  patchSessionInfo: (patch) =>
    set((state) => ({
      sessionInfo: state.sessionInfo ? { ...state.sessionInfo, ...patch } : state.sessionInfo,
    })),

  setMetrics: (metrics) => set({ metrics }),

  setSessionConfig: (config) =>
    set((state) => ({ sessionConfig: { ...state.sessionConfig, ...config } })),

  resetSessionState: () =>
    set({
      metrics: null,
      sessionConfig: { model: null, thinking: null, reasoningEffort: null, yolo: null },
    }),

  appendReadonlyTool: (name, args, toolCallId) =>
    set((state) => {
      const msgs = [...state.messages]
      const last = msgs[msgs.length - 1]
      if (last && last.type === 'tool_group') {
        // Extend existing group
        const group = { ...last, tools: [...last.tools, { name, args }] }
        group.summary = buildToolGroupSummary(group.tools)
        msgs[msgs.length - 1] = group
      } else {
        // Start new group
        msgs.push({
          type: 'tool_group',
          id: `tg-${toolCallId}`,
          tools: [{ name, args }],
          summary: buildToolGroupSummary([{ name, args }]),
        })
      }
      return { messages: msgs }
    }),

  appendReadonlyToolResult: (name, toolCallId, result, success) =>
    set((state) => {
      const msgs = [...state.messages]
      // Find the last tool_group and attach result
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i].type === 'tool_group') {
          const group = msgs[i] as ToolGroupChatItem
          const newTools = [...group.tools]
          for (let j = newTools.length - 1; j >= 0; j--) {
            if (newTools[j].name === name && newTools[j].result === undefined) {
              newTools[j] = { ...newTools[j], result, success }
              msgs[i] = { ...group, tools: newTools }
              return { messages: msgs }
            }
          }
          break
        }
      }
      return { messages: msgs }
    }),
}))
