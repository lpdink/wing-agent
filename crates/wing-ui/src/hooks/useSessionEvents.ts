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
  const addToast = useUiStore((s) => s.addToast)
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

    // ── Reasoning events (streaming — append to last, don't create new) ──
    const handleReasoning = (event: { session_id: string | null; content: string }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendToLastReasoning(event.content, crypto.randomUUID())
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
      store.finalizeReasoningStreaming()
      store.appendMessage({ type: 'done', id: crypto.randomUUID() })
      store.setSending(false)
    }

    // ── Interrupted events ────────────────────────────────────
    const handleInterrupted = (event: { session_id: string | null }) => {
      if (event.session_id !== getActiveSessionId()) return
      const store = useSessionStore.getState()
      store.finalizeStreaming()
      store.finalizeReasoningStreaming()
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
      if (event.title) {
        const store = useSessionStore.getState()
        const sessions = store.sessions.map((s) =>
          s.id === event.session_id ? { ...s, name: event.title } : s,
        )
        store.setSessions(sessions)
      }
    }

    // ── Ask events (interactive question + choices) ───────────
    const handleAsk = (event: {
      session_id: string | null
      question: string
      choices: string[]
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'ask',
        id: crypto.randomUUID(),
        question: event.question,
        choices: event.choices,
      })
    }

    // ── Diff content events ───────────────────────────────────
    const handleDiffContent = (event: {
      session_id: string | null
      path: string
      old_text: string | null
      new_text: string
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'diff',
        id: crypto.randomUUID(),
        path: event.path,
        oldText: event.old_text,
        newText: event.new_text,
      })
    }

    // ── LLM call metrics events ───────────────────────────────
    const handleLLMCallMetrics = (event: {
      session_id: string | null
      model: string
      prompt_tokens: number
      completion_tokens: number
      cached_tokens: number
      first_chunk_rt_ms: number
      tokens_per_sec: number
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      useSessionStore.getState().appendMessage({
        type: 'metrics',
        id: crypto.randomUUID(),
        model: event.model,
        promptTokens: event.prompt_tokens,
        completionTokens: event.completion_tokens,
        cachedTokens: event.cached_tokens,
        firstChunkRtMs: event.first_chunk_rt_ms,
        tokensPerSec: event.tokens_per_sec,
      })
    }

    // ── Sync session events (full context replacement) ────────
    // When session is loaded/forked, Gateway sends the complete message history.
    // We skip this since useSession.selectSession() already loads messages via HTTP.
    // If we ever need to handle live sync, we'd replace messages here.

    // ── Compact done events (context compaction finished) ─────
    const handleCompactDone = (event: {
      session_id: string | null
      original_tokens: number
      compressed_tokens: number
    }) => {
      if (event.session_id !== getActiveSessionId()) return
      const saved = event.original_tokens - event.compressed_tokens
      addToast(`Context compacted: saved ${saved.toLocaleString()} tokens`, 'info')
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
    wsClient.on('ask', handleAsk)
    wsClient.on('diff_content', handleDiffContent)
    wsClient.on('llm_call_metrics', handleLLMCallMetrics)
    wsClient.on('compact_done', handleCompactDone)

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
      wsClient.off('ask', handleAsk)
      wsClient.off('diff_content', handleDiffContent)
      wsClient.off('llm_call_metrics', handleLLMCallMetrics)
      wsClient.off('compact_done', handleCompactDone)
    }
  }, [wsClient, addError, addToast, client])

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
