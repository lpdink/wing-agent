// src/components/cells/TodoChecklist.tsx — Compact checklist for TodoWrite tool calls.
//
// Renders as a process line with progress summary when collapsed,
// and a checklist with status icons when expanded.
// Visual weight matches process lines (13px, muted, no card container).

import { useState } from 'react'
import { ListTodo } from 'lucide-react'
import type { ToolCallChatItem } from '@/stores/sessionStore'
import { ProcessLine } from './ProcessLine'

interface TodoItem {
  content: string
  status: 'pending' | 'in_progress' | 'completed'
}

/** Parse todos array from TodoWrite tool args. */
function parseTodos(args: Record<string, unknown>): TodoItem[] {
  const raw = args.todos
  if (!Array.isArray(raw)) return []
  return raw
    .filter((item): item is Record<string, unknown> => typeof item === 'object' && item !== null)
    .map((item) => ({
      content: typeof item.content === 'string' ? item.content : String(item.content ?? ''),
      status: (['pending', 'in_progress', 'completed'].includes(item.status as string)
        ? item.status
        : 'pending') as TodoItem['status'],
    }))
}

const STATUS_ICON: Record<TodoItem['status'], { symbol: string; className: string }> = {
  completed: { symbol: '✓', className: 'text-success' },
  in_progress: { symbol: '●', className: 'text-accent' },
  pending: { symbol: '○', className: 'text-text-dim' },
}

export function TodoChecklist({ data }: { data: ToolCallChatItem }) {
  const [expanded, setExpanded] = useState(false)
  const todos = parseTodos(data.toolArgs)
  const doneCount = todos.filter((t) => t.status === 'completed').length
  const progressSummary = todos.length > 0 ? `${doneCount}/${todos.length} done` : undefined

  return (
    <ProcessLine
      icon={ListTodo}
      label="TodoWrite"
      subject={progressSummary}
      expanded={expanded}
      onToggle={() => setExpanded(!expanded)}
    >
      <div className="space-y-0.5">
        {todos.map((todo, i) => {
          const icon = STATUS_ICON[todo.status]
          return (
            <div key={i} className="flex items-start gap-2 text-[13px] leading-5">
              <span className={`shrink-0 ${icon.className}`}>{icon.symbol}</span>
              <span className="text-text-dim">{todo.content}</span>
            </div>
          )
        })}
      </div>
    </ProcessLine>
  )
}
