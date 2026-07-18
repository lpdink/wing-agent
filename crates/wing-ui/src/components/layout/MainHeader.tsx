import { PanelLeft, Brain, MoreHorizontal } from 'lucide-react'

interface MainHeaderProps {
  sessionTitle: string
  sidebarOpen: boolean
  onToggleSidebar: () => void
}

/**
 * MainHeader — session title, model badge, thinking toggle, control buttons.
 */
export function MainHeader({ sessionTitle, sidebarOpen, onToggleSidebar }: MainHeaderProps) {
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

        <h1 className="text-base font-semibold text-text">{sessionTitle}</h1>
      </div>

      {/* Right section */}
      <div className="flex items-center gap-2">
        {/* Model badge */}
        <span className="rounded-full bg-bg-elevated px-2.5 py-0.5 text-xs font-medium text-text-dim">
          gpt-4o
        </span>

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
