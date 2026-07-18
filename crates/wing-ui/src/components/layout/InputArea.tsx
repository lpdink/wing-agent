import { useState, useRef, useCallback, type KeyboardEvent, type ChangeEvent } from 'react'
import { SendHorizontal, Paperclip } from 'lucide-react'

/**
 * InputArea — multiline textarea with send button, attachment placeholder,
 * and keyboard shortcut hints.
 *
 * Behavior:
 * - Enter sends (calls onSend)
 * - Shift+Enter inserts newline
 * - Auto-grows up to 160px
 */
export function InputArea() {
  const [value, setValue] = useState('')
  const textareaRef = useRef<HTMLTextAreaElement>(null)

  const autoResize = useCallback(() => {
    const el = textareaRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = Math.min(el.scrollHeight, 160) + 'px'
  }, [])

  const handleSend = useCallback(() => {
    // Phase 3: no-op. Phase 4 will dispatch via SDK.
  }, [])

  const handleChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setValue(e.target.value)
    autoResize()
  }

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      if (value.trim()) {
        handleSend()
        setValue('')
        // Reset height after clearing
        requestAnimationFrame(() => {
          if (textareaRef.current) {
            textareaRef.current.style.height = 'auto'
          }
        })
      }
    }
  }

  return (
    <div className="shrink-0 border-t border-border bg-bg-surface px-4 py-3">
      <div className="mx-auto max-w-3xl">
        <div className="rounded-xl border border-border bg-bg-input transition-colors focus-within:border-accent">
          <textarea
            ref={textareaRef}
            value={value}
            onChange={handleChange}
            onKeyDown={handleKeyDown}
            placeholder="Send a message…"
            rows={1}
            className="block w-full resize-none bg-transparent px-4 py-3 text-text placeholder-text-muted outline-none"
          />

          {/* Bottom toolbar */}
          <div className="flex items-center justify-between px-3 pb-2">
            {/* Left — attachment (placeholder) */}
            <button className="rounded-md p-1 text-text-muted transition-colors hover:bg-bg-elevated hover:text-text">
              <Paperclip className="h-4 w-4" />
            </button>

            {/* Right — shortcut hint + send */}
            <div className="flex items-center gap-3">
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
                onClick={() => {
                  if (value.trim()) {
                    handleSend()
                    setValue('')
                  }
                }}
                disabled={!value.trim()}
                className="rounded-lg bg-accent p-1.5 text-white transition-colors hover:bg-accent-hover disabled:cursor-not-allowed disabled:opacity-30"
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
