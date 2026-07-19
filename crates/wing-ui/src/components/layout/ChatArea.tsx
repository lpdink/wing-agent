import { useCallback, useMemo } from 'react'
import { Loader2, ArrowDown } from 'lucide-react'
import { WelcomeView } from './WelcomeView'
import { registry } from '@/core/cell-registry'
import { TypingIndicator } from '@/components/cells/TypingIndicator'
import { ToolCallCell } from '@/components/cells/ToolCallCell'
import { AskCell } from '@/components/cells/AskCell'
import { useSessionStore, type ToolResultChatItem } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'
import { useAutoScroll } from '@/hooks/useAutoScroll'
// Ensure all cells are registered
import '@/components/cells'

/**
 * ChatArea — scrollable message container using Cell Registry.
 *
 * Shows WelcomeView when no session active or no messages.
 * Renders messages via Cell Registry for extensibility.
 * Smart auto-scroll: follows bottom unless user scrolls up.
 */
export function ChatArea() {
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const messages = useSessionStore((s) => s.messages)
  const isLoading = useSessionStore((s) => s.isLoading)
  const isSending = useSessionStore((s) => s.isSending)
  const { sendMessage } = useSession()

  const { scrollRef, isFollowing, scrollToBottom } = useAutoScroll<HTMLDivElement>({
    deps: [messages],
  })

  // Build toolCallId → ToolResultChatItem lookup for merging
  const toolResultMap = useMemo(() => {
    const map = new Map<string, ToolResultChatItem>()
    for (const msg of messages) {
      if (msg.type === 'tool_call_result') {
        map.set(msg.toolCallId, msg)
      }
    }
    return map
  }, [messages])

  // Track which tool_call_results have been merged into ToolCallCells
  const mergedResultIds = useMemo(() => {
    const ids = new Set<string>()
    for (const msg of messages) {
      if (msg.type === 'tool_call' && toolResultMap.has(msg.toolCallId)) {
        ids.add(toolResultMap.get(msg.toolCallId)!.id)
      }
    }
    return ids
  }, [messages, toolResultMap])

  const handleAskAnswer = useCallback(
    (choice: string) => {
      sendMessage(choice)
    },
    [sendMessage],
  )

  // Determine if we should show the typing indicator
  const showTyping = isSending && !messages.some((m) => m.type === 'assistant' && m.streaming)

  // No session selected — show welcome
  if (!activeSessionId) {
    return (
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
        <WelcomeView />
      </div>
    )
  }

  // Loading session
  if (isLoading) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center">
        <Loader2 className="h-6 w-6 animate-spin text-text-muted" />
      </div>
    )
  }

  // Empty session — show welcome
  if (messages.length === 0 && !showTyping) {
    return (
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
        <WelcomeView />
      </div>
    )
  }

  return (
    <div className="relative min-h-0 flex-1">
      <div ref={scrollRef} className="h-full overflow-y-auto">
        <div className="mx-auto max-w-3xl">
          <div className="flex flex-col gap-4 px-4 py-6">
            {messages.map((item) => {
              // Skip tool_call_results that are merged into ToolCallCells
              if (item.type === 'tool_call_result' && mergedResultIds.has(item.id)) {
                return null
              }

              // Special handling for tool_call: merge with result
              if (item.type === 'tool_call') {
                const result = toolResultMap.get(item.toolCallId) ?? null
                return <ToolCallCell key={item.id} data={item} result={result} />
              }

              // Special handling for ask: inject onAnswer callback
              if (item.type === 'ask') {
                return <AskCell key={item.id} data={item} onAnswer={handleAskAnswer} />
              }

              // Standard cell rendering via registry
              const Cell = registry.resolve(item.type)
              return <Cell key={item.id} data={item} />
            })}

            {/* Typing indicator while agent is processing */}
            {showTyping && <TypingIndicator />}
          </div>
        </div>
      </div>

      {/* Floating "scroll to bottom" button — visible when not following */}
      {!isFollowing && (
        <button
          onClick={scrollToBottom}
          className="absolute bottom-4 left-1/2 z-10 flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-border bg-bg-elevated px-3 py-1.5 text-xs text-text-muted shadow-lg transition-colors hover:bg-bg-hover hover:text-text"
        >
          <ArrowDown className="h-3.5 w-3.5" />
          Latest
        </button>
      )}
    </div>
  )
}
