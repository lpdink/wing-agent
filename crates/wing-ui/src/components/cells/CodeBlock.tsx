// src/components/cells/CodeBlock.tsx — Shiki-powered syntax highlighting + copy button.
//
// Embedded inside AssistantMessage's Markdown renderer for fenced code blocks.

import { useState, useEffect, useRef, useCallback } from 'react'
import { Check, Copy } from 'lucide-react'
import { codeToHtml, type BundledLanguage } from 'shiki'

interface CodeBlockProps {
  code: string
  language?: string
}

export function CodeBlock({ code, language }: CodeBlockProps) {
  const [html, setHtml] = useState<string>('')
  const [copied, setCopied] = useState(false)
  const copyTimer = useRef<ReturnType<typeof setTimeout>>(null)

  const lang = (language ?? 'text') as BundledLanguage

  useEffect(() => {
    let cancelled = false

    codeToHtml(code, {
      lang,
      // Use a theme that works on both light and dark backgrounds
      themes: { light: 'github-light', dark: 'github-dark' },
    })
      .then((result) => {
        if (!cancelled) setHtml(result)
      })
      .catch(() => {
        // Fallback: render as plain text
        if (!cancelled) {
          const escaped = code.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
          setHtml(`<pre><code>${escaped}</code></pre>`)
        }
      })

    return () => {
      cancelled = true
    }
  }, [code, lang])

  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(code).then(() => {
      setCopied(true)
      if (copyTimer.current) clearTimeout(copyTimer.current)
      copyTimer.current = setTimeout(() => setCopied(false), 2000)
    })
  }, [code])

  useEffect(() => {
    return () => {
      if (copyTimer.current) clearTimeout(copyTimer.current)
    }
  }, [])

  return (
    <div className="my-3 overflow-hidden rounded-lg border border-border">
      {/* Header — language label + copy button */}
      <div className="flex items-center justify-between border-b border-border bg-bg-elevated px-3 py-1.5">
        <span className="text-[11px] font-medium uppercase tracking-wider text-text-muted">
          {language ?? 'text'}
        </span>
        <button
          onClick={handleCopy}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-xs text-text-muted transition-colors hover:bg-bg-hover hover:text-text"
        >
          {copied ? (
            <>
              <Check className="h-3 w-3 text-success" />
              <span className="text-success">Copied!</span>
            </>
          ) : (
            <>
              <Copy className="h-3 w-3" />
              <span>Copy</span>
            </>
          )}
        </button>
      </div>

      {/* Code content */}
      <div
        className="code-block-content max-h-96 overflow-auto bg-bg-code p-3 text-[13px] leading-relaxed"
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </div>
  )
}
