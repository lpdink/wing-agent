// src/hooks/useSession.ts — Session CRUD operations.
//
// Wraps GatewayClient methods with store updates.

import { useCallback } from 'react'
import type { CreateSessionRequest } from '@wing-agent/sdk'
import { useGatewayClient } from './useGatewayClient'
import { useSessionStore, type ChatItem } from '@/stores/sessionStore'
import { useUiStore } from '@/stores/uiStore'

/** Convert API message format to ChatItem[]. */
function apiMessagesToChatItems(messages: Record<string, unknown>[]): ChatItem[] {
  const items: ChatItem[] = []

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
        items.push({ type: 'reasoning', id: `${uuid}-reasoning`, content: reasoning })
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
        items.push({ type: 'assistant', id: uuid, content })
      }
    } else if (role === 'tool') {
      // Tool result
      const toolCallId = msg.tool_call_id as string
      items.push({
        type: 'tool_call_result',
        id: uuid,
        toolName: 'tool',
        toolCallId: toolCallId ?? '',
        result: content,
        success: true,
      })
    }
    // Skip 'system' role messages
  }

  return items
}

export function useSession() {
  const { client } = useGatewayClient()
  const addError = useUiStore((s) => s.addError)

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

      try {
        // Resume session first (loads it into Gateway memory)
        await client.resumeSession(sessionId)

        // Subscribe to events (must happen before getSession to receive sync_session)
        if (client.clientId) {
          await client.subscribe(sessionId)
        }

        // Load message history
        const sessionData = await client.getSession(sessionId)
        const items = apiMessagesToChatItems(sessionData.messages)
        store.setMessages(items)

        // Try to get session info (model, tokens, etc.)
        try {
          const info = await client.getSessionInfo(sessionId)
          store.setSessionInfo(info)
        } catch {
          // Session info is optional
        }
      } catch (e) {
        addError(`Failed to load session: ${e instanceof Error ? e.message : String(e)}`)
      } finally {
        store.setLoading(false)
      }
    },
    [client, addError],
  )

  const createSession = useCallback(
    async (options?: CreateSessionRequest) => {
      try {
        const resp = await client.createSession(options)
        // Reload sessions and select the new one
        await loadSessions()
        await selectSession(resp.session_id)
      } catch (e) {
        addError(`Failed to create session: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, loadSessions, selectSession, addError],
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

  return {
    loadSessions,
    selectSession,
    createSession,
    sendMessage,
    interruptSession,
  }
}
