import { useState, useRef, useCallback, type KeyboardEvent, type ChangeEvent } from 'react'
import { SendHorizontal, Paperclip, Square } from 'lucide-react'
import { useSessionStore } from '@/stores/sessionStore'
import { useSession } from '@/hooks/useSession'

/**
 * InputArea — multiline textarea with send button, attachment placeholder,
 * and keyboard shortcut hints.
 *
 * Behavior:
 * - Enter sends (calls onSend)
 * - Shift+Enter inserts newline
 * - Auto-grows up to 160px
 * - Disabled while sending (with interrupt option)
 */
export function InputArea() {
  const [value, setValue] = useState('')
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const isSending = useSessionStore((s) => s.isSending)
  const { sendMessage, interruptSession } = useSession()

  const autoResize = useCallback(() => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = Math.min(el.scrollHeight, 160) + 'px'
  }, [])

  const handleSend = useCallback(() => {
    if (!value.trim() || !activeSessionId || isSending) return
    sendMessage(value.trim())
    setValue('')
    // Reset height after clearing
    requestAnimationFrame(() => {
      if (textareaRef.current) {
        textareaRef.current.style.height = 'auto'
      }
    })
  }, [value, activeSessionId, isSending, sendMessage])

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setValue(e.target.value)
    autoResize()
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      handleSend()
    }
  }

  const disabled = !activeSessionId || isSending

  return (
    <div className="shrink-0 border-t border-border bg-bg-surface px-4 py-3">
      <div className="mx-auto max-w-3xl">
        <div className="rounded-xl border border-border bg-bg-input transition-colors focus-within:border-border-focus">
          <textarea
            ref={textareaRef}
            value={value}
            onChange={handleChange}
            onKeyDown={handleKeyDown}
            placeholder={
              activeSessionId ? 'Send a message…' : 'Select or create a session to start'
            }
            rows={1}
            disabled={disabled}
            className="block w-full resize-none bg-transparent px-4 py-3 text-text placeholder-text-muted outline-none disabled:cursor-not-allowed disabled:opacity-50"
          />

          {/* Bottom toolbar */}
          <div className="flex items-center justify-between px-3 pb-2">
            {/* Left — attachment (placeholder) */}
            <button
              disabled={!activeSessionId}
              className="rounded-md p-1 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text disabled:opacity-50"
            >
              <Paperclip className="h-4 w-4" />
            </button>

            {/* Right — shortcut hint + send/interrupt */}
            <div className="flex items-center gap-3">
              {isSending ? (
                <button
                  onClick={() => interruptSession()}
                  className="flex items-center gap-1.5 rounded-lg bg-error px-3 py-1.5 text-sm text-text-inverse transition-colors hover:bg-error/90"
                >
                  <Square className="h-3.5 w-3.5" />
                  <span>Stop</span>
                </button>
              ) : (
                <>
                  <span className="hidden text-[11px] text-text-muted sm:inline">
                    <kbd className="rounded border border-border bg-bg-elevated px-1 py-px font-mono text-[10px]">
                      Enter
                    </kbd>{' '}
                    to send ·{' '}
                    <kbd className="rounded border border-border bg-bg-elevated px-1 py-px font-mono text-[10px]">
                      Shift+Enter
                    </kbd>{' '}
                    for new line
                  </span>

                  <button
                    onClick={handleSend}
                    disabled={disabled || !value.trim()}
                    className="rounded-lg bg-accent p-1.5 text-text-inverse transition-colors hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-30"
                  >
                    <SendHorizontal className="h-4 w-4" />
                  </button>
                </>
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
