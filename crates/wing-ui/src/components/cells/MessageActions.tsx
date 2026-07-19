// src/components/cells/MessageActions.tsx — Hover action bar for assistant messages.
//
// Shows Fork / Rewind / Copy buttons on hover. Fork and Rewind require a valid
// backend message uuid (only available after the LLM turn is sealed).

import { useState, useRef, useEffect, useCallback } from 'react'
import { GitFork, RotateCcw, Copy, Check } from 'lucide-react'

interface MessageActionsProps {
  /** Backend message uuid — required for fork/rewind. */
  messageUuid?: string
  /** Raw text content for copy. */
  content: string
  onFork: (uuid: string) => void
  onRewind: (uuid: string) => void
}

export function MessageActions({ messageUuid, content, onFork, onRewind }: MessageActionsProps) {
  const [copied, setCopied] = useState(false)
  const copyTimer = useRef<ReturnType<typeof setTimeout>>(null)

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(content).then(() => {
      setCopied(true)
      if (copyTimer.current) clearTimeout(copyTimer.current)
      copyTimer.current = setTimeout(() => setCopied(false), 2000)
    })
  }, [content])

  useEffect(() => {
    return () => {
      if (copyTimer.current) clearTimeout(copyTimer.current)
    }
  }, [])

  return (
    <div className="mt-1.5 flex items-center gap-1 opacity-0 transition-opacity group-hover:opacity-100">
      {messageUuid && (
        <>
          <button
            onClick={() => onFork(messageUuid)}
            title="Fork from here"
            className="rounded p-1 text-text-muted transition-colors hover:bg-bg-hover hover:text-text"
          >
            <GitFork className="h-3.5 w-3.5" />
          </button>
          <button
            onClick={() => onRewind(messageUuid)}
            title="Rewind to here"
            className="rounded p-1 text-text-muted transition-colors hover:bg-bg-hover hover:text-text"
          >
            <RotateCcw className="h-3.5 w-3.5" />
          </button>
        </>
      )}
      <button
        onClick={handleCopy}
        title="Copy message"
        className="rounded p-1 text-text-muted transition-colors hover:bg-bg-hover hover:text-text"
      >
        {copied ? <Check className="h-3.5 w-3.5 text-success" /> : <Copy className="h-3.5 w-3.5" />}
      </button>
    </div>
  )
}
