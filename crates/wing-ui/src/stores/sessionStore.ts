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

// ============================================================
// Store
// ============================================================

interface SessionState {
  sessions: SessionInfo[]
  activeSessionId: string | null
  messages: ChatItem[]
  isLoading: boolean
  isSending: boolean
  sessionInfo: SessionInfoResponse | null
}

interface SessionActions {
  setSessions: (sessions: SessionInfo[]) => void
  setActiveSessionId: (id: string | null) => void
  setMessages: (messages: ChatItem[]) => void
  appendMessage: (item: ChatItem) => void
  /** Append text to the last assistant message (streaming). Creates one if none exists. */
  appendToLastAssistant: (text: string, id: string) => void
  clearMessages: () => void
  setLoading: (loading: boolean) => void
  setSending: (sending: boolean) => void
  setSessionInfo: (info: SessionInfoResponse | null) => void
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

  // Actions
  setSessions: (sessions) => set({ sessions }),

  setActiveSessionId: (id) => set({ activeSessionId: id }),

  setMessages: (messages) => set({ messages }),

  appendMessage: (item) => set((state) => ({ messages: [...state.messages, item] })),

  appendToLastAssistant: (text, id) =>
    set((state) => {
      const msgs = [...state.messages]
      const last = msgs[msgs.length - 1]
      if (last && last.type === 'assistant') {
        msgs[msgs.length - 1] = { ...last, content: last.content + text, streaming: true }
      } else {
        msgs.push({ type: 'assistant', id, content: text, streaming: true })
      }
      return { messages: msgs }
    }),

  clearMessages: () => set({ messages: [] }),

  setLoading: (loading) => set({ isLoading: loading }),

  setSending: (sending) => set({ isSending: sending }),

  setSessionInfo: (info) => set({ sessionInfo: info }),
}))
