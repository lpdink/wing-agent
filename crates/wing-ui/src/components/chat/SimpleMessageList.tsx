// src/components/chat/SimpleMessageList.tsx — Temporary Phase 4 message renderer.
//
// Simple text-based rendering of ChatItem[]. Phase 5 will replace this
// with the Cell Registry + specialized Cell components.

import { useState } from 'react'
import { ChevronDown, ChevronRight, User, Bot, Wrench, Brain, AlertCircle } from 'lucide-react'
import type { ChatItem } from '@/stores/sessionStore'

interface SimpleMessageListProps {
  messages: ChatItem[]
}

export function SimpleMessageList({ messages }: SimpleMessageListProps) {
  return (
    <div className="flex flex-col gap-4 px-4 py-6">
      {messages.map((item) => (
        <ChatItemView key={item.id} item={item} />
      ))}
    </div>
  )
}

function ChatItemView({ item }: { item: ChatItem }) {
  switch (item.type) {
    case 'user':
      return <UserMessage content={item.content} />
    case 'assistant':
      return <AssistantMessage content={item.content} streaming={item.streaming} />
    case 'tool_call':
      return <ToolCallMessage toolName={item.toolName} toolArgs={item.toolArgs} />
    case 'tool_call_result':
      return (
        <ToolResultMessage toolName={item.toolName} result={item.result} success={item.success} />
      )
    case 'reasoning':
      return <ReasoningMessage content={item.content} />
    case 'turn_started':
      return <TurnIndicator text="Turn started" />
    case 'done':
      return <TurnIndicator text="Turn complete" />
    case 'error':
      return <ErrorMessage message={item.message} />
  }
}

// ── User message (right-aligned bubble) ─────────────────────────

function UserMessage({ content }: { content: string }) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[80%] rounded-2xl bg-accent px-4 py-2.5 text-text-inverse">
        <div className="mb-1 flex items-center gap-1.5 text-xs opacity-75">
          <User className="h-3 w-3" />
          <span>You</span>
        </div>
        <div className="whitespace-pre-wrap text-sm">{content}</div>
      </div>
    </div>
  )
}

// ── Assistant message (left-aligned text) ───────────────────────

function AssistantMessage({ content, streaming }: { content: string; streaming?: boolean }) {
  return (
    <div className="flex justify-start">
      <div className="max-w-[85%]">
        <div className="mb-1 flex items-center gap-1.5 text-xs text-text-muted">
          <Bot className="h-3 w-3" />
          <span>Assistant</span>
          {streaming && <span className="animate-pulse">●</span>}
        </div>
        <div className="whitespace-pre-wrap text-sm text-text">{content}</div>
      </div>
    </div>
  )
}

// ── Tool call (collapsible card) ────────────────────────────────

function ToolCallMessage({
  toolName,
  toolArgs,
}: {
  toolName: string
  toolArgs: Record<string, unknown>
}) {
  const [expanded, setExpanded] = useState(false)
  const argsSummary = Object.keys(toolArgs).length > 0 ? JSON.stringify(toolArgs, null, 2) : '{}'

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg border border-border bg-bg-elevated px-3 py-2">
        <button
          onClick={() => setExpanded(!expanded)}
          className="flex w-full items-center gap-2 text-left text-sm"
        >
          {expanded ? (
            <ChevronDown className="h-3.5 w-3.5 text-text-muted" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 text-text-muted" />
          )}
          <Wrench className="h-3.5 w-3.5 text-accent" />
          <span className="font-medium text-text">{toolName}</span>
        </button>
        {expanded && (
          <pre className="mt-2 overflow-x-auto rounded bg-bg-code p-2 text-xs text-text-dim">
            {argsSummary}
          </pre>
        )}
      </div>
    </div>
  )
}

// ── Tool result (collapsible card) ──────────────────────────────

function ToolResultMessage({
  toolName,
  result,
  success,
}: {
  toolName: string
  result: string
  success: boolean
}) {
  const [expanded, setExpanded] = useState(false)
  const truncated = result.length > 200 ? result.slice(0, 200) + '…' : result

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-lg border border-border bg-bg-elevated px-3 py-2">
        <button
          onClick={() => setExpanded(!expanded)}
          className="flex w-full items-center gap-2 text-left text-sm"
        >
          {expanded ? (
            <ChevronDown className="h-3.5 w-3.5 text-text-muted" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 text-text-muted" />
          )}
          <Wrench className="h-3.5 w-3.5 text-text-muted" />
          <span className="text-text-dim">
            {toolName} result {success ? '✓' : '✗'}
          </span>
        </button>
        {expanded ? (
          <pre className="mt-2 max-h-60 overflow-auto rounded bg-bg-code p-2 text-xs text-text-dim">
            {result}
          </pre>
        ) : (
          <div className="mt-1 text-xs text-text-muted">{truncated}</div>
        )}
      </div>
    </div>
  )
}

// ── Reasoning (dimmed italic) ───────────────────────────────────

function ReasoningMessage({ content }: { content: string }) {
  return (
    <div className="flex justify-start">
      <div className="max-w-[85%]">
        <div className="mb-1 flex items-center gap-1.5 text-xs text-text-muted">
          <Brain className="h-3 w-3" />
          <span>Thinking</span>
        </div>
        <div className="italic text-text-dim">{content}</div>
      </div>
    </div>
  )
}

// ── Turn indicator ──────────────────────────────────────────────

function TurnIndicator({ text }: { text: string }) {
  return (
    <div className="flex justify-center">
      <span className="rounded-full bg-bg-elevated px-3 py-1 text-xs text-text-muted">{text}</span>
    </div>
  )
}

// ── Error message ───────────────────────────────────────────────

function ErrorMessage({ message }: { message: string }) {
  return (
    <div className="flex justify-center">
      <div className="flex items-center gap-2 rounded-lg bg-error/10 px-3 py-2 text-sm text-error">
        <AlertCircle className="h-4 w-4" />
        <span>{message}</span>
      </div>
    </div>
  )
}
