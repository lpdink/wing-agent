import { PanelLeft, Brain, MoreHorizontal, Square } from 'lucide-react'
import { useSessionStore } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'

interface MainHeaderProps {
  sidebarOpen: boolean
  onToggleSidebar: () => void
}

/**
 * MainHeader — session title, model badge, thinking toggle, control buttons.
 */
export function MainHeader({ sidebarOpen, onToggleSidebar }: MainHeaderProps) {
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const sessions = useSessionStore((s) => s.sessions)
  const sessionInfo = useSessionStore((s) => s.sessionInfo)
  const isSending = useSessionStore((s) => s.isSending)
  const { interruptSession } = useSession()

  // Get session title from sessions list
  const activeSession = sessions.find((s) => s.id === activeSessionId)
  const sessionTitle = activeSession?.name ?? sessionInfo?.session_name ?? 'New Session'
  const model = sessionInfo?.model ?? ''

  return (
    <div className="flex h-14 shrink-0 items-center justify-between border-b border-border bg-bg-surface px-4">
      {/* Left section */}
      <div className="flex items-center gap-3">
        {/* Sidebar toggle — visible when sidebar is collapsed or on narrow screens */}
        <button
          onClick={onToggleSidebar}
          className={`rounded-md p-1.5 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text ${sidebarOpen ? 'md:hidden' : ''}`}
          aria-label="Toggle sidebar"
        >
          <PanelLeft className="h-5 w-5" />
        </button>

        <h1 className="truncate text-base font-semibold text-text">{sessionTitle}</h1>
      </div>

      {/* Right section */}
      <div className="flex items-center gap-2">
        {/* Model badge */}
        {model && (
          <span className="rounded-full bg-bg-elevated px-2.5 py-0.5 text-xs font-medium text-text-dim">
            {model}
          </span>
        )}

        {/* Interrupt button (visible when sending) */}
        {isSending && (
          <button
            onClick={() => interruptSession()}
            className="flex items-center gap-1.5 rounded-md bg-error/10 px-2 py-1 text-xs text-error transition-colors hover:bg-error/20"
          >
            <Square className="h-3.5 w-3.5" />
            <span>Stop</span>
          </button>
        )}

        {/* Thinking toggle (placeholder) */}
        <button className="flex items-center gap-1.5 rounded-md px-2 py-1 text-xs text-text-muted transition-colors hover:bg-bg-elevated hover:text-text">
          <Brain className="h-3.5 w-3.5" />
          <span>Thinking</span>
        </button>

        {/* More options (placeholder) */}
        <button className="rounded-md p-1.5 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text">
          <MoreHorizontal className="h-4 w-4" />
        </button>
      </div>
    </div>
  )
}
