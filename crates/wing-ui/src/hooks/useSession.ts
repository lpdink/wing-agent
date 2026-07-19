// src/hooks/useSession.ts — Session CRUD operations.
//
// Wraps GatewayClient methods with store updates.

import { useCallback } from 'react'
import type { CreateSessionRequest, UpdateSessionRequest, SessionInfo } from '@wing-agent/sdk'
import { useGatewayClient } from './useGatewayClient'
import {
  useSessionStore,
  type ChatItem,
  type ToolGroupEntry,
  getToolCategory,
  buildToolGroupSummary,
} from '@/stores/sessionStore'
import { useConnectionStore } from '@/stores/connectionStore'
import { useUiStore } from '@/stores/uiStore'

/** Convert API message format to ChatItem[]. */
export function apiMessagesToChatItems(messages: Record<string, unknown>[]): ChatItem[] {
  const items: ChatItem[] = []
  // Build toolCallId → toolName mapping for tool results
  const toolCallNames = new Map<string, string>()

  // First pass: collect tool call names from assistant messages
  for (const msg of messages) {
    if (msg.role === 'assistant' && msg.tool_calls) {
      const toolCalls = msg.tool_calls as Array<{ id: string; name: string }>
      for (const tc of toolCalls) {
        toolCallNames.set(tc.id, tc.name)
      }
    }
  }

  // Second pass: build ChatItems
  for (const msg of messages) {
    const role = msg.role as string
    const content = (msg.content as string) ?? ''
    const uuid = (msg.uuid as string) ?? crypto.randomUUID()

    if (role === 'user') {
      items.push({ type: 'user', id: uuid, content })
    } else if (role === 'assistant') {
      // Order: reasoning → tool_calls → assistant text
      // 1. Reasoning content (thinking)
      const reasoning = msg.reasoning_content as string | undefined
      if (reasoning) {
        items.push({
          type: 'reasoning',
          id: `${uuid}-reasoning`,
          content: reasoning,
          messageUuid: uuid,
        })
      }
      // 2. Tool calls from this assistant message
      const toolCalls = msg.tool_calls as
        Array<{ id: string; name: string; arguments: string }> | undefined
      if (toolCalls) {
        for (const tc of toolCalls) {
          let args: Record<string, unknown> = {}
          try {
            args = JSON.parse(tc.arguments)
          } catch {
            args = { raw: tc.arguments }
          }
          items.push({
            type: 'tool_call',
            id: `${uuid}-tc-${tc.id}`,
            toolName: tc.name,
            toolArgs: args,
            toolCallId: tc.id,
          })
        }
      }
      // 3. Main assistant text (final response)
      if (content) {
        items.push({ type: 'assistant', id: uuid, content, messageUuid: uuid })
      }
    } else if (role === 'tool') {
      // Tool result — resolve tool name from mapping
      const toolCallId = (msg.tool_call_id as string) ?? ''
      items.push({
        type: 'tool_call_result',
        id: uuid,
        toolName: toolCallNames.get(toolCallId) ?? 'tool',
        toolCallId,
        result: content,
        success: true,
      })
    }
    // Skip 'system' role messages
  }

  return groupReadonlyTools(items)
}

/**
 * Post-process: merge consecutive readonly tool_call + tool_call_result
 * items into ToolGroupChatItem for compressed rendering.
 */
function groupReadonlyTools(items: ChatItem[]): ChatItem[] {
  const result: ChatItem[] = []
  let currentGroup: ToolGroupEntry[] = []
  let groupId = ''

  const flushGroup = () => {
    if (currentGroup.length > 0) {
      result.push({
        type: 'tool_group',
        id: groupId,
        tools: currentGroup,
        summary: buildToolGroupSummary(currentGroup),
      })
      currentGroup = []
    }
  }

  for (const item of items) {
    if (item.type === 'tool_call' && getToolCategory(item.toolName) === 'readonly') {
      if (currentGroup.length === 0) groupId = `tg-${item.id}`
      currentGroup.push({ name: item.toolName, args: item.toolArgs, toolCallId: item.toolCallId })
    } else if (item.type === 'tool_call_result' && getToolCategory(item.toolName) === 'readonly') {
      // Attach result by toolCallId
      const entry = currentGroup.find((t) => t.toolCallId === item.toolCallId)
      if (entry) {
        entry.result = item.result
        entry.success = item.success
      }
    } else {
      flushGroup()
      result.push(item)
    }
  }
  flushGroup()

  return result
}

export function useSession() {
  const { client } = useGatewayClient()
  const addError = useUiStore((s) => s.addError)
  const addToast = useUiStore((s) => s.addToast)

  const loadSessions = useCallback(async () => {
    try {
      const resp = await client.listSessions()
      useSessionStore.getState().setSessions(resp.sessions)
    } catch (e) {
      addError(`Failed to load sessions: ${e instanceof Error ? e.message : String(e)}`)
    }
  }, [client, addError])

  const selectSession = useCallback(
    async (sessionId: string) => {
      const store = useSessionStore.getState()
      const prevId = store.activeSessionId

      // Unsubscribe from previous session
      if (prevId && prevId !== sessionId && client.clientId) {
        try {
          await client.unsubscribe(prevId)
        } catch {
          // Ignore unsubscribe errors
        }
      }

      store.setActiveSessionId(sessionId)
      store.setLoading(true)
      store.clearMessages()
      store.resetSessionState()

      try {
        // Resume session (loads into Gateway memory, triggers sync_session via WS)
        await client.resumeSession(sessionId)

        // Subscribe to events — sync_session handler will populate messages + config
        if (client.clientId) {
          await client.subscribe(sessionId)
        }

        // Fallback: if WS is not connected, load via HTTP
        const wsConnected = useConnectionStore.getState().status === 'connected'
        if (!wsConnected) {
          const sessionData = await client.getSession(sessionId)
          store.setMessages(apiMessagesToChatItems(sessionData.messages))
          try {
            const info = await client.getSessionInfo(sessionId)
            store.setSessionInfo(info)
          } catch {
            // Session info is optional
          }
        }
      } catch (e) {
        addError(`Failed to load session: ${e instanceof Error ? e.message : String(e)}`)
      } finally {
        store.setLoading(false)
      }
    },
    [client, addError],
  )

  /** Build an optimistic SessionInfo from create response for immediate sidebar insert. */
  const buildOptimisticSession = (resp: {
    session_id: string
    template_name: string
    workspace: string | null
  }): SessionInfo => {
    const now = new Date().toISOString()
    return {
      id: resp.session_id,
      name: null,
      created_at: now,
      template_name: resp.template_name ?? null,
      workspace: resp.workspace ?? null,
      last_interaction: now,
    }
  }

  const createSession = useCallback(
    async (options?: CreateSessionRequest) => {
      try {
        const resp = await client.createSession(options)
        const store = useSessionStore.getState()
        store.setSessions([buildOptimisticSession(resp), ...store.sessions])
        await selectSession(resp.session_id)
      } catch (e) {
        addError(`Failed to create session: ${e instanceof Error ? e.message : String(e)}`)
        loadSessions() // Recover sidebar state
      }
    },
    [client, selectSession, addError, loadSessions],
  )

  /** Create a new session and immediately send the first message. */
  const createAndSend = useCallback(
    async (message: string) => {
      try {
        const resp = await client.createSession()
        const store = useSessionStore.getState()
        const sessionId = resp.session_id

        // Optimistic sidebar insert
        store.setSessions([buildOptimisticSession(resp), ...store.sessions])

        // Activate and subscribe
        store.setActiveSessionId(sessionId)
        store.clearMessages()
        store.resetSessionState()

        if (client.clientId) {
          await client.subscribe(sessionId)
        }

        // Show user message and send
        store.appendMessage({ type: 'user', id: crypto.randomUUID(), content: message })
        store.setSending(true)

        await client.sendMessage(sessionId, message)
      } catch (e) {
        useSessionStore.getState().setSending(false)
        addError(`Failed to start session: ${e instanceof Error ? e.message : String(e)}`)
        loadSessions() // Recover sidebar state
      }
    },
    [client, addError, loadSessions],
  )

  const sendMessage = useCallback(
    async (content: string) => {
      const store = useSessionStore.getState()
      const sessionId = store.activeSessionId
      if (!sessionId) {
        addError('No active session')
        return
      }

      // Add user message to display immediately
      store.appendMessage({
        type: 'user',
        id: crypto.randomUUID(),
        content,
      })
      store.setSending(true)

      try {
        await client.sendMessage(sessionId, content)
      } catch (e) {
        store.setSending(false)
        addError(`Failed to send message: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, addError],
  )

  const interruptSession = useCallback(async () => {
    const sessionId = useSessionStore.getState().activeSessionId
    if (!sessionId) return

    try {
      await client.interruptSession(sessionId)
    } catch (e) {
      addError(`Failed to interrupt: ${e instanceof Error ? e.message : String(e)}`)
    }
  }, [client, addError])

  const forkSession = useCallback(
    async (targetUuid: string) => {
      const store = useSessionStore.getState()
      const sessionId = store.activeSessionId
      if (!sessionId) return

      try {
        const resp = await client.forkSession(sessionId, targetUuid)
        addToast('Session forked', 'success')
        await loadSessions()
        await selectSession(resp.session_id)
      } catch (e) {
        addError(`Fork failed: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, loadSessions, selectSession, addError, addToast],
  )

  const rewindSession = useCallback(
    async (targetUuid: string) => {
      const store = useSessionStore.getState()
      const sessionId = store.activeSessionId
      if (!sessionId) return

      try {
        await client.rewindSession(sessionId, targetUuid)
        addToast('Session rewound', 'info')
        // Reload messages for the current session
        const sessionData = await client.getSession(sessionId)
        const items = apiMessagesToChatItems(sessionData.messages)
        store.setMessages(items)
      } catch (e) {
        addError(`Rewind failed: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, addError, addToast],
  )

  const compactSession = useCallback(async () => {
    const sessionId = useSessionStore.getState().activeSessionId
    if (!sessionId) return

    try {
      const resp = await client.compactSession(sessionId)
      const saved = resp.original_tokens - resp.compressed_tokens
      addToast(`Context compacted: saved ${saved.toLocaleString()} tokens`, 'info')
    } catch (e) {
      addError(`Compact failed: ${e instanceof Error ? e.message : String(e)}`)
    }
  }, [client, addError, addToast])

  const updateSession = useCallback(
    async (req: Omit<UpdateSessionRequest, 'session_id'>, sessionId?: string) => {
      const targetId = sessionId ?? useSessionStore.getState().activeSessionId
      if (!targetId) return

      try {
        await client.updateSession({ session_id: targetId, ...req })
        // Optimistically patch local sessionInfo (only if updating active session)
        const store = useSessionStore.getState()
        const patch: Record<string, unknown> = {}
        if (req.model != null) patch.model = req.model
        if (req.thinking != null) patch.thinking = req.thinking
        if (req.reasoning_effort != null) patch.reasoning_effort = req.reasoning_effort
        if (req.yolo != null) patch.yolo = req.yolo
        if (req.title != null) {
          const newTitle = req.title
          // Update the sessions list name
          const sessions = store.sessions.map((s) =>
            s.id === targetId ? { ...s, name: newTitle } : s,
          )
          store.setSessions(sessions)
          // Only patch sessionInfo if this is the active session
          if (targetId === store.activeSessionId) {
            patch.session_name = newTitle
          }
        }
        if (Object.keys(patch).length > 0 && targetId === store.activeSessionId) {
          store.patchSessionInfo(patch)
        }
      } catch (e) {
        addError(`Update failed: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, addError],
  )

  return {
    loadSessions,
    selectSession,
    createSession,
    createAndSend,
    sendMessage,
    interruptSession,
    forkSession,
    rewindSession,
    compactSession,
    updateSession,
  }
}
