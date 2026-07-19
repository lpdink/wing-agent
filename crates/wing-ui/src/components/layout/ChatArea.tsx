import { useCallback, useMemo, useEffect, useRef } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Loader2, ArrowDown } from 'lucide-react'
import { WelcomeView } from './WelcomeView'
import { registry } from '@/core/cell-registry'
import { TypingIndicator } from '@/components/cells/TypingIndicator'
import { ToolCallCell } from '@/components/cells/ToolCallCell'
import { AskCell } from '@/components/cells/AskCell'
import { useSessionStore, type ToolResultChatItem, type ChatItem } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'
import { useAutoScroll } from '@/hooks/useAutoScroll'
// Ensure all cells are registered
import '@/components/cells'

/** Types that should not be rendered in the virtualized list. */
const SKIP_TYPES = new Set(['turn_started', 'done'])

/**
 * ChatArea — virtualized scrollable message container using Cell Registry.
 *
 * Shows WelcomeView when no session active or no messages.
 * Renders messages via @tanstack/react-virtual for performance with long conversations.
 * Smart auto-scroll: follows bottom unless user scrolls up.
 */
export function ChatArea() {
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const messages = useSessionStore((s) => s.messages)
  const isLoading = useSessionStore((s) => s.isLoading)
  const isSending = useSessionStore((s) => s.isSending)
  const { sendMessage } = useSession()

  // Filter out non-renderable items (turn_started, done) and merged tool results
  const toolResultMap = useMemo(() => {
    const map = new Map<string, ToolResultChatItem>()
    for (const msg of messages) {
      if (msg.type === 'tool_call_result') {
        map.set(msg.toolCallId, msg)
      }
    }
    return map
  }, [messages])

  const mergedResultIds = useMemo(() => {
    const ids = new Set<string>()
    for (const msg of messages) {
      if (msg.type === 'tool_call' && toolResultMap.has(msg.toolCallId)) {
        ids.add(toolResultMap.get(msg.toolCallId)!.id)
      }
    }
    return ids
  }, [messages, toolResultMap])

  // Build the renderable items list (skip turn_started/done and merged results)
  const renderItems = useMemo(() => {
    return messages.filter((item) => {
      if (SKIP_TYPES.has(item.type)) return false
      if (item.type === 'tool_call_result' && mergedResultIds.has(item.id)) return false
      return true
    })
  }, [messages, mergedResultIds])

  // Include typing indicator as a virtual row when active
  const showTyping = isSending && !messages.some((m) => m.type === 'assistant' && m.streaming)
  const totalRows = renderItems.length + (showTyping ? 1 : 0)

  const { scrollRef, scrollElementRef, isFollowing, scrollToBottom } =
    useAutoScroll<HTMLDivElement>({
      deps: [messages],
    })

  const virtualizer = useVirtualizer({
    count: totalRows,
    getScrollElement: () => scrollElementRef.current,
    estimateSize: () => 80,
    overscan: 5,
    getItemKey: (index) => {
      if (index < renderItems.length) return renderItems[index].id
      return '__typing_indicator__'
    },
  })

  // When following and new content arrives, pin to bottom via virtualizer
  const prevCountRef = useRef(totalRows)
  useEffect(() => {
    if (isFollowing && totalRows > prevCountRef.current) {
      // Use requestAnimationFrame to let the virtualizer measure new items first
      requestAnimationFrame(() => {
        virtualizer.scrollToIndex(totalRows - 1, { align: 'end' })
      })
    }
    prevCountRef.current = totalRows
  }, [totalRows, isFollowing, virtualizer])

  const handleAskAnswer = useCallback(
    (choice: string) => {
      sendMessage(choice)
    },
    [sendMessage],
  )

  const handleScrollToBottom = useCallback(() => {
    virtualizer.scrollToIndex(totalRows - 1, { align: 'end', behavior: 'smooth' })
    scrollToBottom()
  }, [virtualizer, totalRows, scrollToBottom])

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
  if (renderItems.length === 0 && !showTyping) {
    return (
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
        <WelcomeView />
      </div>
    )
  }

  const virtualRows = virtualizer.getVirtualItems()

  return (
    <div className="relative min-h-0 flex-1">
      <div ref={scrollRef} className="h-full overflow-y-auto">
        <div className="mx-auto max-w-3xl">
          {/* Virtual spacer — total height of all items */}
          <div
            style={{
              height: virtualizer.getTotalSize(),
              width: '100%',
              position: 'relative',
            }}
          >
            <div className="flex flex-col gap-4 px-4 py-6">
              {virtualRows.map((virtualRow) => {
                const isTypingRow = virtualRow.index >= renderItems.length
                if (isTypingRow) {
                  return (
                    <div
                      key="__typing_indicator__"
                      data-index={virtualRow.index}
                      ref={virtualizer.measureElement}
                      style={{
                        position: 'absolute',
                        top: 0,
                        left: 0,
                        width: '100%',
                        transform: `translateY(${virtualRow.start}px)`,
                      }}
                    >
                      <TypingIndicator />
                    </div>
                  )
                }

                const item = renderItems[virtualRow.index]
                return (
                  <div
                    key={item.id}
                    data-index={virtualRow.index}
                    ref={virtualizer.measureElement}
                    style={{
                      position: 'absolute',
                      top: 0,
                      left: 0,
                      width: '100%',
                      transform: `translateY(${virtualRow.start}px)`,
                    }}
                  >
                    {renderCell(item)}
                  </div>
                )
              })}
            </div>
          </div>
        </div>
      </div>

      {/* Floating "scroll to bottom" button — visible when not following */}
      {!isFollowing && (
        <button
          onClick={handleScrollToBottom}
          className="absolute bottom-4 left-1/2 z-10 flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-border bg-bg-elevated px-3 py-1.5 text-xs text-text-muted shadow-lg transition-colors hover:bg-bg-hover hover:text-text"
        >
          <ArrowDown className="h-3.5 w-3.5" />
          Latest
        </button>
      )}
    </div>
  )

  // ── Cell rendering (preserves special-case logic) ──────────────

  function renderCell(item: ChatItem) {
    // Special handling for tool_call: merge with result
    if (item.type === 'tool_call') {
      const result = toolResultMap.get(item.toolCallId) ?? null
      return <ToolCallCell data={item} result={result} />
    }

    // Special handling for ask: inject onAnswer callback
    if (item.type === 'ask') {
      return <AskCell data={item} onAnswer={handleAskAnswer} />
    }

    // Standard cell rendering via registry
    const Cell = registry.resolve(item.type)
    return <Cell data={item} />
  }
}
