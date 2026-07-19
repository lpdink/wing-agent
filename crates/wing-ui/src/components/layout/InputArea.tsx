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
 * - Always enabled: no session → auto-create on send; isSending → steer message
 * - Send and Stop are independent actions (both visible during agent execution)
 */
export function InputArea() {
  const [value, setValue] = useState('')
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const activeSessionId = useSessionStore((s) => s.activeSessionId)
  const isSending = useSessionStore((s) => s.isSending)
  const { sendMessage, createAndSend, interruptSession } = useSession()

  const autoResize = useCallback(() => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = Math.min(el.scrollHeight, 160) + 'px'
  }, [])

  const handleSend = useCallback(() => {
    const text = value.trim()
    if (!text) return

    if (!activeSessionId) {
      // Auto-create session and send first message
      createAndSend(text)
    } else {
      // Normal send (also works as steer when isSending)
      sendMessage(text)
    }

    setValue('')
    requestAnimationFrame(() => {
      if (textareaRef.current) {
        textareaRef.current.style.height = 'auto'
      }
    })
  }, [value, activeSessionId, sendMessage, createAndSend])

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

  return (
    <div className="shrink-0 border-t border-border bg-bg-surface px-4 py-3">
      <div className="mx-auto max-w-3xl">
        <div className="rounded-xl border border-border bg-bg-input transition-colors focus-within:border-border-focus">
          <textarea
            ref={textareaRef}
            value={value}
            onChange={handleChange}
            onKeyDown={handleKeyDown}
            placeholder={activeSessionId ? 'Send a message…' : 'Type to start a new session…'}
            rows={1}
            className="block w-full resize-none bg-transparent px-4 py-3 text-text placeholder-text-muted outline-none"
          />

          {/* Bottom toolbar */}
          <div className="flex items-center justify-between px-3 pb-2">
            {/* Left — attachment (placeholder) */}
            <button className="rounded-md p-1 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text">
              <Paperclip className="h-4 w-4" />
            </button>

            {/* Right — shortcut hint + send/stop */}
            <div className="flex items-center gap-2">
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

              {/* Stop button — visible during agent execution */}
              {isSending && (
                <button
                  onClick={() => interruptSession()}
                  className="flex items-center gap-1.5 rounded-lg bg-error px-3 py-1.5 text-sm text-text-inverse transition-colors hover:bg-error/90"
                >
                  <Square className="h-3.5 w-3.5" />
                  <span>Stop</span>
                </button>
              )}

              {/* Send button — always visible */}
              <button
                onClick={handleSend}
                disabled={!value.trim()}
                className="rounded-lg bg-accent p-1.5 text-text-inverse transition-colors hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-30"
              >
                <SendHorizontal className="h-4 w-4" />
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}
