// src/hooks/useKeyboardShortcuts.ts — Global keyboard shortcuts.
//
// Registers a window-level keydown listener for app-wide shortcuts.
// Skips handling when focus is inside an input/textarea (except Escape).

import { useEffect } from 'react'
import { isModPressed } from '@/lib/platform'
import { useUiStore } from '@/stores/uiStore'
import { useSessionStore } from '@/stores/sessionStore'
import { useSession } from './useSession'

/** Check if the event target is an editable element (input, textarea, contenteditable). */
function isEditableTarget(e: KeyboardEvent): boolean {
  const el = e.target as HTMLElement | null
  if (!el) return false
  const tag = el.tagName
  return tag === 'INPUT' || tag === 'TEXTAREA' || el.isContentEditable
}

/**
 * Global keyboard shortcuts hook. Mount once at the app shell level.
 *
 * Shortcuts:
 * - Mod+N: New session
 * - Mod+K: Toggle command palette
 * - Mod+B: Toggle sidebar
 * - Mod+Shift+C: Copy last assistant message
 * - Mod+,: Open settings
 * - Escape: Interrupt agent (when sending)
 */
export function useKeyboardShortcuts(): void {
  const { createSession, interruptSession } = useSession()

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      const mod = isModPressed(e)

      // Escape — interrupt agent turn (works even in inputs)
      if (e.key === 'Escape') {
        const { commandPaletteOpen, settingsOpen, setCommandPaletteOpen, setSettingsOpen } =
          useUiStore.getState()
        if (commandPaletteOpen) {
          setCommandPaletteOpen(false)
          e.preventDefault()
          return
        }
        if (settingsOpen) {
          setSettingsOpen(false)
          e.preventDefault()
          return
        }
        // Interrupt if agent is working
        if (useSessionStore.getState().isSending) {
          interruptSession()
          e.preventDefault()
        }
        return
      }

      // All other shortcuts require the modifier key and should not fire in inputs
      if (!mod) return
      if (isEditableTarget(e)) return

      const key = e.key.toLowerCase()

      // Mod+K — command palette
      if (key === 'k' && !e.shiftKey) {
        e.preventDefault()
        useUiStore.getState().toggleCommandPalette()
        return
      }

      // Mod+N — new session
      if (key === 'n' && !e.shiftKey) {
        e.preventDefault()
        createSession()
        return
      }

      // Mod+B — toggle sidebar
      if (key === 'b' && !e.shiftKey) {
        e.preventDefault()
        useUiStore.getState().toggleSidebar()
        return
      }

      // Mod+, — settings
      if (key === ',') {
        e.preventDefault()
        useUiStore.getState().toggleSettings()
        return
      }

      // Mod+Shift+C — copy last assistant message
      if (key === 'c' && e.shiftKey) {
        e.preventDefault()
        const messages = useSessionStore.getState().messages
        for (let i = messages.length - 1; i >= 0; i--) {
          if (messages[i].type === 'assistant') {
            const content = (messages[i] as { content: string }).content
            navigator.clipboard.writeText(content).then(
              () => useUiStore.getState().addToast('Copied last response', 'success'),
              () => useUiStore.getState().addToast('Failed to copy', 'error'),
            )
            break
          }
        }
        return
      }
    }

    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [createSession, interruptSession])
}
