// src/components/cells/UserMessage.tsx — User message bubble (right-aligned).

import { User } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { UserChatItem } from '@/stores/sessionStore'

export function UserMessageCell({ data }: CellProps<UserChatItem>) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[75%] rounded-2xl bg-user-bubble-bg px-4 py-3 text-user-bubble-text shadow-sm">
        <div className="mb-1 flex items-center justify-end gap-1.5 text-xs opacity-75">
          <span>You</span>
          <User className="h-3 w-3" />
        </div>
        <div className="whitespace-pre-wrap text-[14.5px] leading-relaxed">{data.content}</div>
      </div>
    </div>
  )
}
