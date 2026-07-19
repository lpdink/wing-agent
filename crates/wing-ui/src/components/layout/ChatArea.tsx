import { useCallback, useMemo, useEffect, useRef, useState } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Loader2, ArrowDown } from 'lucide-react'
import { WelcomeView } from './WelcomeView'
import { registry } from '@/core/cell-registry'
import { TypingIndicator } from '@/components/cells/TypingIndicator'
import { ToolCallCell } from '@/components/cells/ToolCallCell'
import { AskCell } from '@/components/cells/AskCell'
import { useSessionStore, type ToolResultChatItem, type ChatItem } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'
// Ensure all cells are registered
import '@/components/cells'

/** Types that should not be rendered in the virtualized list. */
const SKIP_TYPES = new Set(['turn_started', 'done'])

const AUTO_SCROLL_THRESHOLD = 80 // px from bottom to count as "near bottom"

/** Render a single ChatItem via the cell registry with special-case handling. */
function renderCell(
  item: ChatItem,
  toolResultMap: Map<string, ToolResultChatItem>,
  onAskAnswer: (choice: string) => void,
) {
  if (item.type === 'tool_call') {
    const result = toolResultMap.get(item.toolCallId) ?? null
    return <ToolCallCell data={item} result={result} />
  }
  if (item.type === 'ask') {
    return <AskCell data={item} onAnswer={onAskAnswer} />
  }
  const Cell = registry.resolve(item.type)
  return <Cell data={item} />
}

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

  const scrollElementRef = useRef<HTMLDivElement | null>(null)
  const isFollowingRef = useRef(true)
  const isProgrammaticScrollRef = useRef(false)
  const [isFollowing, setIsFollowing] = useState(true)

  // ── Build renderable items (skip turn_started/done and merged tool results) ──

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

  const renderItems = useMemo(() => {
    return messages.filter((item) => {
      if (SKIP_TYPES.has(item.type)) return false
      if (item.type === 'tool_call_result' && mergedResultIds.has(item.id)) return false
      return true
    })
  }, [messages, mergedResultIds])

  const showTyping = isSending && !messages.some((m) => m.type === 'assistant' && m.streaming)
  const totalRows = renderItems.length + (showTyping ? 1 : 0)

  // ── Scroll follow detection (inline, no dual mechanism) ──

  const handleScroll = useCallback(() => {
    if (isProgrammaticScrollRef.current) return
    const el = scrollElementRef.current
    if (!el) return
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    const nearBottom = distanceFromBottom < AUTO_SCROLL_THRESHOLD
    if (nearBottom !== isFollowingRef.current) {
      isFollowingRef.current = nearBottom
      setIsFollowing(nearBottom)
    }
  }, [])

  // Callback ref: bind/unbind scroll listener
  const scrollRef = useCallback(
    (node: HTMLDivElement | null) => {
      const prev = scrollElementRef.current
      if (prev) prev.removeEventListener('scroll', handleScroll)
      scrollElementRef.current = node
      if (node) {
        node.addEventListener('scroll', handleScroll, { passive: true })
      }
    },
    [handleScroll],
  )

  // ── Virtualizer ──

  const virtualizer = useVirtualizer({
    count: totalRows,
    getScrollElement: () => scrollElementRef.current,
    estimateSize: () => 96, // ~80px content + 16px gap
    overscan: 5,
    getItemKey: (index) => {
      if (index < renderItems.length) return renderItems[index].id
      return '__typing_indicator__'
    },
  })

  // Auto-follow: pin to bottom when new content arrives and user is following
  const prevCountRef = useRef(totalRows)
  useEffect(() => {
    if (isFollowingRef.current && totalRows > 0) {
      isProgrammaticScrollRef.current = true
      virtualizer.scrollToIndex(totalRows - 1, { align: 'end' })
      requestAnimationFrame(() => {
        isProgrammaticScrollRef.current = false
      })
    }
    prevCountRef.current = totalRows
  }, [totalRows, messages, virtualizer])

  const scrollToBottom = useCallback(() => {
    isProgrammaticScrollRef.current = true
    isFollowingRef.current = true
    setIsFollowing(true)
    virtualizer.scrollToIndex(totalRows - 1, { align: 'end', behavior: 'smooth' })
    // Reset programmatic flag after smooth scroll settles
    setTimeout(() => {
      isProgrammaticScrollRef.current = false
    }, 500)
  }, [virtualizer, totalRows])

  const handleAskAnswer = useCallback(
    (choice: string) => {
      sendMessage(choice)
    },
    [sendMessage],
  )

  // ── Early returns for empty/loading states ──

  if (!activeSessionId) {
    return (
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
        <WelcomeView />
      </div>
    )
  }

  if (isLoading) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center">
        <Loader2 className="h-6 w-6 animate-spin text-text-muted" />
      </div>
    )
  }

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
          <div
            style={{
              height: virtualizer.getTotalSize(),
              width: '100%',
              position: 'relative',
            }}
          >
            {virtualRows.map((virtualRow) => {
              const isTypingRow = virtualRow.index >= renderItems.length
              return (
                <div
                  key={isTypingRow ? '__typing_indicator__' : renderItems[virtualRow.index].id}
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
                  {/* Spacing varies by cell type: compact for tools/reasoning, normal for messages */}
                  <div
                    className={`px-4 pt-0 first:pt-6 ${
                      [
                        'tool_call',
                        'tool_call_result',
                        'tool_group',
                        'reasoning',
                        'turn_started',
                      ].includes(renderItems[virtualRow.index]?.type ?? '')
                        ? 'pb-1'
                        : 'pb-4'
                    }`}
                  >
                    {isTypingRow ? (
                      <TypingIndicator />
                    ) : (
                      renderCell(renderItems[virtualRow.index], toolResultMap, handleAskAnswer)
                    )}
                  </div>
                </div>
              )
            })}
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
