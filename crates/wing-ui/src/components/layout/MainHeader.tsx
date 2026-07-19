// src/components/layout/MainHeader.tsx — Session title + config controls.
//
// Controls: model selector, thinking toggle, reasoning effort, YOLO mode.
// Title supports double-click inline rename. MoreHorizontal menu has Compact.

import { useState, useRef, useEffect, useCallback } from 'react'
import {
  PanelLeft,
  Brain,
  MoreHorizontal,
  Square,
  Zap,
  AlertTriangle,
  Check,
  ChevronDown,
  Pencil,
} from 'lucide-react'
import { useSessionStore } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'
import { useGatewayClient } from '@/hooks/useGatewayClient'
import { Popover } from '@/components/ui/Popover'

interface MainHeaderProps {
  sidebarOpen: boolean
  onToggleSidebar: () => void
}

const EFFORT_LEVELS = ['low', 'medium', 'high', 'xhigh', 'max'] as const

/**
 * MainHeader — session title, model badge, thinking toggle, control buttons.
 */
export function MainHeader({ sidebarOpen, onToggleSidebar }: MainHeaderProps) {
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const sessions = useSessionStore((s) => s.sessions)
  const sessionInfo = useSessionStore((s) => s.sessionInfo)
  const isSending = useSessionStore((s) => s.isSending)
  const { interruptSession, updateSession, compactSession } = useSession()
  const { client } = useGatewayClient()

  // Title rename state
  const [isRenaming, setIsRenaming] = useState(false)
  const [renameValue, setRenameValue] = useState('')
  const renameInputRef = useRef<HTMLInputElement>(null)

  // Model list cache
  const [models, setModels] = useState<string[]>([])
  const [modelsOpen, setModelsOpen] = useState(false)

  // More menu state
  const [moreOpen, setMoreOpen] = useState(false)

  const activeSession = sessions.find((s) => s.id === activeSessionId)
  const sessionTitle = activeSession?.name ?? sessionInfo?.session_name ?? 'New Session'
  const model = sessionInfo?.model ?? ''
  const thinking = sessionInfo?.thinking ?? false
  const reasoningEffort = sessionInfo?.reasoning_effort ?? null
  const yolo = sessionInfo?.yolo ?? false

  // Fetch models when model popover opens
  const fetchModels = useCallback(async () => {
    try {
      const resp = await client.getModels()
      setModels(resp.models)
    } catch {
      // Ignore — list stays empty
    }
  }, [client])

  // Focus rename input when entering rename mode
  useEffect(() => {
    if (isRenaming && renameInputRef.current) {
      renameInputRef.current.focus()
      renameInputRef.current.select()
    }
  }, [isRenaming])

  const startRename = () => {
    setRenameValue(sessionTitle)
    setIsRenaming(true)
  }

  const commitRename = () => {
    const trimmed = renameValue.trim()
    if (trimmed && trimmed !== sessionTitle) {
      updateSession({ title: trimmed })
    }
    setIsRenaming(false)
  }

  const handleRenameKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      commitRename()
    } else if (e.key === 'Escape') {
      setIsRenaming(false)
    }
  }

  return (
    <div className="flex h-14 shrink-0 items-center justify-between border-b border-border bg-bg-surface px-4">
      {/* Left section */}
      <div className="flex min-w-0 items-center gap-3">
        <button
          onClick={onToggleSidebar}
          className={`rounded-md p-1.5 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text ${sidebarOpen ? 'md:hidden' : ''}`}
          aria-label="Toggle sidebar"
        >
          <PanelLeft className="h-5 w-5" />
        </button>

        {/* Title — double-click to rename */}
        {isRenaming ? (
          <input
            ref={renameInputRef}
            value={renameValue}
            onChange={(e) => setRenameValue(e.target.value)}
            onBlur={commitRename}
            onKeyDown={handleRenameKeyDown}
            className="min-w-0 max-w-[240px] rounded border border-border-focus bg-bg-input px-2 py-0.5 text-base font-semibold text-text outline-none"
          />
        ) : (
          <h1
            onDoubleClick={startRename}
            className="group/title flex min-w-0 cursor-default items-center gap-1.5 truncate text-base font-semibold text-text"
            title="Double-click to rename"
          >
            <span className="truncate">{sessionTitle}</span>
            <Pencil className="h-3 w-3 shrink-0 text-text-muted opacity-0 transition-opacity group-hover/title:opacity-100" />
          </h1>
        )}
      </div>

      {/* Right section */}
      <div className="flex items-center gap-1.5">
        {/* Model selector */}
        {model && (
          <Popover
            open={modelsOpen}
            onOpenChange={(open) => {
              setModelsOpen(open)
              if (open) fetchModels()
            }}
            placement="bottom-end"
            trigger={
              <button className="flex items-center gap-1 rounded-full bg-bg-elevated px-2.5 py-1 text-xs font-medium text-text-dim transition-colors hover:bg-bg-hover hover:text-text">
                {model}
                <ChevronDown className="h-3 w-3" />
              </button>
            }
            panelClassName="max-h-64 w-56 overflow-y-auto py-1"
          >
            {models.length === 0 ? (
              <div className="px-3 py-2 text-xs text-text-muted">Loading…</div>
            ) : (
              models.map((m) => (
                <button
                  key={m}
                  onClick={() => {
                    updateSession({ model: m })
                    setModelsOpen(false)
                  }}
                  className={`flex w-full items-center justify-between px-3 py-1.5 text-left text-xs transition-colors hover:bg-bg-hover ${
                    m === model ? 'text-accent' : 'text-text-dim'
                  }`}
                >
                  <span className="truncate">{m}</span>
                  {m === model && <Check className="h-3 w-3 shrink-0" />}
                </button>
              ))
            )}
          </Popover>
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

        {/* Thinking toggle */}
        <button
          onClick={() => updateSession({ thinking: !thinking })}
          className={`flex items-center gap-1.5 rounded-md px-2 py-1 text-xs transition-colors ${
            thinking
              ? 'bg-accent/10 text-accent hover:bg-accent/20'
              : 'text-text-muted hover:bg-bg-elevated hover:text-text'
          }`}
          title={thinking ? 'Thinking: on' : 'Thinking: off'}
        >
          <Brain className="h-3.5 w-3.5" />
          <span className="hidden sm:inline">Think</span>
        </button>

        {/* Reasoning effort selector */}
        <Popover
          placement="bottom-end"
          trigger={
            <button
              className="flex items-center gap-1 rounded-md px-2 py-1 text-xs text-text-muted transition-colors hover:bg-bg-elevated hover:text-text"
              title="Reasoning effort"
            >
              <Zap className="h-3.5 w-3.5" />
              <span className="hidden sm:inline">{reasoningEffort ?? 'auto'}</span>
            </button>
          }
          panelClassName="py-1 w-32"
        >
          {EFFORT_LEVELS.map((level) => (
            <button
              key={level}
              onClick={() => updateSession({ reasoning_effort: level })}
              className={`flex w-full items-center justify-between px-3 py-1.5 text-left text-xs transition-colors hover:bg-bg-hover ${
                level === reasoningEffort ? 'text-accent' : 'text-text-dim'
              }`}
            >
              <span className="capitalize">{level}</span>
              {level === reasoningEffort && <Check className="h-3 w-3" />}
            </button>
          ))}
        </Popover>

        {/* YOLO mode toggle */}
        <button
          onClick={() => updateSession({ yolo: !yolo })}
          className={`flex items-center gap-1.5 rounded-md px-2 py-1 text-xs transition-colors ${
            yolo
              ? 'bg-error/10 text-error hover:bg-error/20'
              : 'text-text-muted hover:bg-bg-elevated hover:text-text'
          }`}
          title={yolo ? 'YOLO mode: ON (skips confirmation)' : 'YOLO mode: off'}
        >
          <AlertTriangle className="h-3.5 w-3.5" />
          <span className="hidden sm:inline">YOLO</span>
        </button>

        {/* More options */}
        <Popover
          open={moreOpen}
          onOpenChange={setMoreOpen}
          placement="bottom-end"
          trigger={
            <button className="rounded-md p-1.5 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text">
              <MoreHorizontal className="h-4 w-4" />
            </button>
          }
          panelClassName="py-1 w-40"
        >
          <button
            onClick={() => {
              compactSession()
              setMoreOpen(false)
            }}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs text-text-dim transition-colors hover:bg-bg-hover hover:text-text"
          >
            <Zap className="h-3.5 w-3.5" />
            <span>Compact context</span>
          </button>
          <button
            onClick={() => {
              startRename()
              setMoreOpen(false)
            }}
            className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs text-text-dim transition-colors hover:bg-bg-hover hover:text-text"
          >
            <Pencil className="h-3.5 w-3.5" />
            <span>Rename</span>
          </button>
        </Popover>
      </div>
    </div>
  )
}
