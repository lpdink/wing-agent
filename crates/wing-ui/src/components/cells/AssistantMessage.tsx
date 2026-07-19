// src/components/cells/AssistantMessage.tsx — Assistant text with Markdown rendering.
//
// Uses react-markdown + remark-gfm for rich text rendering.
// Code blocks are rendered via the CodeBlock component (Shiki highlighting).
// Shows a streaming cursor while text events are still arriving.
// Hover action bar (fork/rewind/copy) via MessageActions.
//
// NOTE: This component is memo'd. Do NOT call useSession() inside —
// fork/rewind use store.getState() directly to keep references stable.

import { memo, useCallback } from 'react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { Bot } from 'lucide-react'
import { CodeBlock } from './CodeBlock'
import { MessageActions } from './MessageActions'
import type { CellProps } from '@/core/cell-types'
import type { AssistantChatItem } from '@/stores/sessionStore'
import { useGatewayClient } from '@/hooks/useGatewayClient'
import { useSessionStore } from '@/stores/sessionStore'
import { useUiStore } from '@/stores/uiStore'

/** Custom renderers for react-markdown. */
const components: Components = {
  code({ className, children, ...props }) {
    // Fenced code block: has className like "language-python"
    const match = /language-(\w+)/.exec(className ?? '')
    const code = String(children).replace(/\n$/, '')

    if (match) {
      return <CodeBlock code={code} language={match[1]} />
    }

    // Inline code
    return (
      <code className="rounded bg-bg-code px-1.5 py-0.5 font-mono text-[13px] text-text" {...props}>
        {children}
      </code>
    )
  },
  pre({ children }) {
    // Pass through — code blocks are handled in the `code` component
    return <>{children}</>
  },
  a({ href, children, ...props }) {
    return (
      <a
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className="text-text-link underline decoration-text-link/30 underline-offset-2 transition-colors hover:decoration-text-link"
        {...props}
      >
        {children}
      </a>
    )
  },
  h1({ children }) {
    return <h1 className="mb-2 mt-4 text-lg font-bold text-text">{children}</h1>
  },
  h2({ children }) {
    return <h2 className="mb-2 mt-3 text-base font-bold text-text">{children}</h2>
  },
  h3({ children }) {
    return <h3 className="mb-1.5 mt-3 text-sm font-bold text-text">{children}</h3>
  },
  p({ children }) {
    return <p className="mb-3 text-[14.5px] leading-relaxed text-text last:mb-0">{children}</p>
  },
  ul({ children }) {
    return <ul className="mb-3 ml-4 list-disc space-y-1 text-[14.5px] text-text">{children}</ul>
  },
  ol({ children }) {
    return <ol className="mb-3 ml-4 list-decimal space-y-1 text-[14.5px] text-text">{children}</ol>
  },
  li({ children }) {
    return <li className="leading-relaxed">{children}</li>
  },
  blockquote({ children }) {
    return (
      <blockquote className="mb-3 border-l-[3px] border-accent pl-3 italic text-text-dim">
        {children}
      </blockquote>
    )
  },
  table({ children }) {
    return (
      <div className="mb-3 overflow-x-auto">
        <table className="min-w-full border-collapse border border-border text-sm">
          {children}
        </table>
      </div>
    )
  },
  th({ children }) {
    return (
      <th className="border border-border bg-bg-elevated px-3 py-1.5 text-left font-medium text-text">
        {children}
      </th>
    )
  },
  td({ children }) {
    return <td className="border border-border px-3 py-1.5 text-text-dim">{children}</td>
  },
  hr() {
    return <hr className="my-4 border-border" />
  },
}

export const AssistantMessageCell = memo(function AssistantMessageCell({
  data,
}: CellProps<AssistantChatItem>) {
  const { client } = useGatewayClient()
  const addToast = useUiStore((s) => s.addToast)
  const addError = useUiStore((s) => s.addError)

  // Stable callbacks using getState() — no hook subscription, memo stays effective
  const handleFork = useCallback(
    async (targetUuid: string) => {
      const store = useSessionStore.getState()
      const sessionId = store.activeSessionId
      if (!sessionId) return
      try {
        const resp = await client.forkSession(sessionId, targetUuid)
        addToast('Session forked', 'success')
        // Reload sessions and select the forked one
        const listResp = await client.listSessions()
        store.setSessions(listResp.sessions)
        store.setActiveSessionId(resp.session_id)
        store.clearMessages()
        store.resetSessionState()
        if (client.clientId) await client.subscribe(resp.session_id)
      } catch (e) {
        addError(`Fork failed: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, addToast, addError],
  )

  const handleRewind = useCallback(
    async (targetUuid: string) => {
      const store = useSessionStore.getState()
      const sessionId = store.activeSessionId
      if (!sessionId) return
      try {
        await client.rewindSession(sessionId, targetUuid)
        addToast('Session rewound', 'info')
        // Messages will be updated via sync_session event
      } catch (e) {
        addError(`Rewind failed: ${e instanceof Error ? e.message : String(e)}`)
      }
    },
    [client, addToast, addError],
  )

  return (
    <div className="group flex justify-start">
      <div className="flex gap-3">
        {/* Avatar */}
        <div className="mt-0.5 flex h-[30px] w-[30px] shrink-0 items-center justify-center rounded-sm bg-accent/10">
          <Bot className="h-[18px] w-[18px] text-accent" />
        </div>

        {/* Content */}
        <div className="min-w-0 max-w-[calc(100%-3rem)]">
          <div className="prose-custom">
            <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
              {data.content}
            </ReactMarkdown>
          </div>
          {data.streaming && (
            <span className="ml-0.5 inline-block h-4 w-1.5 animate-pulse bg-accent align-middle" />
          )}
          {/* Hover actions — only when not streaming */}
          {!data.streaming && (
            <MessageActions
              messageUuid={data.messageUuid}
              content={data.content}
              onFork={handleFork}
              onRewind={handleRewind}
            />
          )}
        </div>
      </div>
    </div>
  )
})
