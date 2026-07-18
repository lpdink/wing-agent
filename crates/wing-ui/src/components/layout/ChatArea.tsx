import { useEffect, useRef } from 'react'
import { Loader2 } from 'lucide-react'
import { WelcomeView } from './WelcomeView'
import { SimpleMessageList } from '@/components/chat/SimpleMessageList'
import { useSessionStore } from '@/stores/sessionStore'

/**
 * ChatArea — scrollable message container.
 *
 * Shows WelcomeView when no session active or no messages.
 * Shows SimpleMessageList (Phase 4) when messages exist.
 * Phase 5: will render Cell components via CellRegistry.
 */
export function ChatArea() {
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const messages = useSessionStore((s) => s.messages)
  const isLoading = useSessionStore((s) => s.isLoading)
  const scrollRef = useRef<HTMLDivElement>(null)

  // Auto-scroll to bottom on new messages
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight
    }
  }, [messages])

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
  if (messages.length === 0) {
    return (
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
        <WelcomeView />
      </div>
    )
  }

  return (
    <div ref={scrollRef} className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto max-w-3xl">
        <SimpleMessageList messages={messages} />
      </div>
    </div>
  )
}
