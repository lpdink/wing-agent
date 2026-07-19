// src/components/ui/CommandPalette.tsx — ⌘K command palette modal.
//
// Fuzzy-searchable, keyboard-driven command palette.
// Groups: Sessions, Models, Actions.

import { useState, useEffect, useRef, useMemo, useCallback } from 'react'
import { Search, MessageSquare, Cpu, Plus, Minimize2, PanelLeft, Settings } from 'lucide-react'
import { useUiStore } from '@/stores/uiStore'
import { useSessionStore } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'
import { useGatewayClient } from '@/hooks/useGatewayClient'
import { modKey } from '@/lib/platform'

// ── Command types ──────────────────────────────────────────────

interface Command {
  id: string
  group: 'Sessions' | 'Models' | 'Actions'
  label: string
  icon: React.ReactNode
  keywords?: string
  execute: () => void
}

// ── Component ──────────────────────────────────────────────────

export function CommandPalette() {
  const open = useUiStore((s) => s.commandPaletteOpen)
  const setOpen = useUiStore((s) => s.setCommandPaletteOpen)
  const addToast = useUiStore((s) => s.addToast)

  const sessions = useSessionStore((s) => s.sessions)
  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const { selectSession, createSession, compactSession, updateSession } = useSession()
  const { client } = useGatewayClient()

  const [query, setQuery] = useState('')
  const [models, setModels] = useState<string[]>([])
  const [selectedIndex, setSelectedIndex] = useState(0)
  const inputRef = useRef<HTMLInputElement>(null)
  const listRef = useRef<HTMLDivElement>(null)

  // Focus input when opened
  useEffect(() => {
    if (open) {
      setQuery('')
      setSelectedIndex(0)
      // Fetch models on open
      client
        .getModels()
        .then((resp) => setModels(resp.models))
        .catch(() => setModels([]))
      requestAnimationFrame(() => inputRef.current?.focus())
    }
  }, [open, client])

  // Build command list
  const commands = useMemo<Command[]>(() => {
    const cmds: Command[] = []

    // Session commands
    for (const s of sessions) {
      cmds.push({
        id: `session-${s.id}`,
        group: 'Sessions',
        label: s.name ?? 'Untitled',
        icon: <MessageSquare className="h-4 w-4 text-text-muted" />,
        keywords: s.workspace ?? '',
        execute: () => {
          selectSession(s.id)
          setOpen(false)
        },
      })
    }

    // Model commands
    for (const m of models) {
      cmds.push({
        id: `model-${m}`,
        group: 'Models',
        label: m,
        icon: <Cpu className="h-4 w-4 text-text-muted" />,
        execute: () => {
          updateSession({ model: m })
          addToast(`Switched to ${m}`, 'success')
          setOpen(false)
        },
      })
    }

    // Action commands
    cmds.push(
      {
        id: 'action-new-session',
        group: 'Actions',
        label: 'New Session',
        icon: <Plus className="h-4 w-4 text-text-muted" />,
        keywords: 'create',
        execute: () => {
          createSession()
          setOpen(false)
        },
      },
      {
        id: 'action-compact',
        group: 'Actions',
        label: 'Compact Context',
        icon: <Minimize2 className="h-4 w-4 text-text-muted" />,
        keywords: 'compress tokens',
        execute: () => {
          compactSession()
          setOpen(false)
        },
      },
      {
        id: 'action-toggle-sidebar',
        group: 'Actions',
        label: 'Toggle Sidebar',
        icon: <PanelLeft className="h-4 w-4 text-text-muted" />,
        keywords: 'panel',
        execute: () => {
          useUiStore.getState().toggleSidebar()
          setOpen(false)
        },
      },
      {
        id: 'action-settings',
        group: 'Actions',
        label: 'Open Settings',
        icon: <Settings className="h-4 w-4 text-text-muted" />,
        keywords: 'preferences config gateway',
        execute: () => {
          useUiStore.getState().setSettingsOpen(true)
          setOpen(false)
        },
      },
    )

    return cmds
  }, [
    sessions,
    models,
    selectSession,
    createSession,
    compactSession,
    updateSession,
    addToast,
    setOpen,
  ])

  // Filter by query
  const filtered = useMemo(() => {
    if (!query.trim()) return commands
    const q = query.toLowerCase()
    return commands.filter(
      (c) => c.label.toLowerCase().includes(q) || (c.keywords ?? '').toLowerCase().includes(q),
    )
  }, [commands, query])

  // Group filtered results
  const groups = useMemo(() => {
    const map = new Map<string, Command[]>()
    for (const cmd of filtered) {
      const list = map.get(cmd.group) ?? []
      list.push(cmd)
      map.set(cmd.group, list)
    }
    return map
  }, [filtered])

  // Reset selection when query changes
  useEffect(() => {
    setSelectedIndex(0)
  }, [query])

  // Scroll selected item into view
  useEffect(() => {
    const el = listRef.current?.querySelector('[data-selected="true"]')
    el?.scrollIntoView({ block: 'nearest' })
  }, [selectedIndex])

  const executeSelected = useCallback(() => {
    const cmd = filtered[selectedIndex]
    if (cmd) cmd.execute()
  }, [filtered, selectedIndex])

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSelectedIndex((i) => Math.min(i + 1, filtered.length - 1))
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSelectedIndex((i) => Math.max(i - 1, 0))
    } else if (e.key === 'Enter') {
      e.preventDefault()
      executeSelected()
    } else if (e.key === 'Escape') {
      e.preventDefault()
      setOpen(false)
    }
  }

  if (!open) return null

  let flatIndex = -1

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh]">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/40" onClick={() => setOpen(false)} />

      {/* Panel */}
      <div className="relative w-full max-w-lg rounded-xl border border-border bg-bg-elevated shadow-2xl">
        {/* Search input */}
        <div className="flex items-center gap-3 border-b border-border px-4 py-3">
          <Search className="h-4 w-4 shrink-0 text-text-muted" />
          <input
            ref={inputRef}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Search sessions, models, actions…"
            className="w-full bg-transparent text-sm text-text placeholder-text-muted outline-none"
          />
          <kbd className="shrink-0 rounded border border-border bg-bg-surface px-1.5 py-0.5 text-[10px] text-text-muted">
            Esc
          </kbd>
        </div>

        {/* Results */}
        <div ref={listRef} className="max-h-80 overflow-y-auto py-2">
          {filtered.length === 0 ? (
            <div className="px-4 py-8 text-center text-sm text-text-muted">No results</div>
          ) : (
            (['Sessions', 'Models', 'Actions'] as const).map((group) => {
              const items = groups.get(group)
              if (!items || items.length === 0) return null
              return (
                <div key={group}>
                  <div className="px-4 pb-1 pt-2 text-[11px] font-medium uppercase tracking-wider text-text-muted">
                    {group}
                  </div>
                  {items.map((cmd) => {
                    flatIndex++
                    const idx = flatIndex
                    const isSelected = idx === selectedIndex
                    const isActive = cmd.id === `session-${activeSessionId}`
                    return (
                      <button
                        key={cmd.id}
                        data-selected={isSelected}
                        onClick={() => {
                          setSelectedIndex(idx)
                          executeSelected()
                        }}
                        onMouseEnter={() => setSelectedIndex(idx)}
                        className={`flex w-full items-center gap-3 px-4 py-2 text-left text-sm transition-colors ${
                          isSelected ? 'bg-bg-hover text-text' : 'text-text-dim'
                        }`}
                      >
                        {cmd.icon}
                        <span className="min-w-0 flex-1 truncate">{cmd.label}</span>
                        {isActive && (
                          <span className="shrink-0 rounded bg-accent/10 px-1.5 py-0.5 text-[10px] text-accent">
                            active
                          </span>
                        )}
                      </button>
                    )
                  })}
                </div>
              )
            })
          )}
        </div>

        {/* Footer hint */}
        <div className="flex items-center gap-4 border-t border-border px-4 py-2 text-[11px] text-text-muted">
          <span>
            <kbd className="rounded border border-border bg-bg-surface px-1 py-px font-mono text-[10px]">
              ↑↓
            </kbd>{' '}
            navigate
          </span>
          <span>
            <kbd className="rounded border border-border bg-bg-surface px-1 py-px font-mono text-[10px]">
              ↵
            </kbd>{' '}
            select
          </span>
          <span>
            <kbd className="rounded border border-border bg-bg-surface px-1 py-px font-mono text-[10px]">
              {modKey}K
            </kbd>{' '}
            toggle
          </span>
        </div>
      </div>
    </div>
  )
}
