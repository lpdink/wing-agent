// src/hooks/useSessionEvents.ts — WebSocket event stream processing.
//
// Listens to WingEvents from WebSocketClient and updates sessionStore.
// Re-subscribes to the active session after WS reconnect.

import { useEffect } from 'react'
import type { WebSocketClient } from '@wing-agent/sdk'
import { useSessionStore } from '@/stores/sessionStore'
import { useConnectionStore } from '@/stores/connectionStore'
import { useUiStore } from '@/stores/uiStore'
import { useGatewayClient } from './useGatewayClient'

export function useSessionEvents(wsClient: WebSocketClient): void {
  const addError = useUiStore((s) => s.addError)
  const connectionStatus = useConnectionStore((s) => s.status)
  const { client } = useGatewayClient()

  useEffect(() => {
    const getActiveSessionId = () => useSessionStore.getState().activeSessionId

    // ── Text events (streaming assistant response) ──────────────
    const handleText = (event: { session_id: string | null; content: string }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendToLastAssistant(event.content, crypto.randomUUID())
    }

    // ── Tool call events ───────────────────────────────────────
    const handleToolCall = (event: {
      session_id: string | null
      tool_name: string
      tool_args: Record<string, unknown>
      tool_call_id: string
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'tool_call',
        id: crypto.randomUUID(),
        toolName: event.tool_name,
        toolArgs: event.tool_args,
        toolCallId: event.tool_call_id,
      })
    }

    // ── Tool call result events ───────────────────────────────
    const handleToolCallResult = (event: {
      session_id: string | null
      tool_name: string
      tool_call_id: string
      tool_result: string
      tool_success: boolean
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'tool_call_result',
        id: crypto.randomUUID(),
        toolName: event.tool_name,
        toolCallId: event.tool_call_id,
        result: event.tool_result,
        success: event.tool_success,
      })
    }

    // ── Reasoning events ──────────────────────────────────────
    const handleReasoning = (event: { session_id: string | null; content: string }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'reasoning',
        id: crypto.randomUUID(),
        content: event.content,
      })
    }

    // ── Turn started events ───────────────────────────────────
    const handleTurnStarted = (event: { session_id: string | null }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'turn_started',
        id: crypto.randomUUID(),
      })
    }

    // ── Done events ───────────────────────────────────────────
    const handleDone = (event: { session_id: string | null }) => {
      if (event.session_id !== getActiveSessionId()) return
      const store = useSessionStore.getState()
      store.finalizeStreaming()
      store.appendMessage({ type: 'done', id: crypto.randomUUID() })
      store.setSending(false)
    }

    // ── Interrupted events ────────────────────────────────────
    const handleInterrupted = (event: { session_id: string | null }) => {
      if (event.session_id !== getActiveSessionId()) return
      const store = useSessionStore.getState()
      store.finalizeStreaming()
      store.setSending(false)
    }

    // ── Error events ──────────────────────────────────────────
    const handleError = (event: { session_id: string | null; message: string }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'error',
        id: crypto.randomUUID(),
        message: event.message,
      })
      useSessionStore.getState().setSending(false)
      addError(event.message)
    }

    // ── Session state changed events ──────────────────────────
    const handleSessionStateChanged = (event: {
      session_id: string | null
      title: string | null
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      // Update session title in the list if changed
      if (event.title) {
        const store = useSessionStore.getState()
        const sessions = store.sessions.map((s) =>
          s.id === event.session_id ? { ...s, name: event.title } : s,
        )
        store.setSessions(sessions)
      }
    }

    // Register all handlers
    wsClient.on('text', handleText)
    wsClient.on('tool_call', handleToolCall)
    wsClient.on('tool_call_result', handleToolCallResult)
    wsClient.on('reasoning', handleReasoning)
    wsClient.on('turn_started', handleTurnStarted)
    wsClient.on('done', handleDone)
    wsClient.on('interrupted', handleInterrupted)
    wsClient.on('error', handleError)
    wsClient.on('session_state_changed', handleSessionStateChanged)

    return () => {
      wsClient.off('text', handleText)
      wsClient.off('tool_call', handleToolCall)
      wsClient.off('tool_call_result', handleToolCallResult)
      wsClient.off('reasoning', handleReasoning)
      wsClient.off('turn_started', handleTurnStarted)
      wsClient.off('done', handleDone)
      wsClient.off('interrupted', handleInterrupted)
      wsClient.off('error', handleError)
      wsClient.off('session_state_changed', handleSessionStateChanged)
    }
  }, [wsClient, addError])

  // Re-subscribe to active session after WS reconnect
  useEffect(() => {
    if (connectionStatus !== 'connected') return

    const sessionId = useSessionStore.getState().activeSessionId
    const clientId = useConnectionStore.getState().clientId
    if (!sessionId || !clientId) return

    client.subscribe(sessionId).catch(() => {
      // Ignore re-subscribe errors — will retry on next reconnect
    })
  }, [client, connectionStatus])
}
