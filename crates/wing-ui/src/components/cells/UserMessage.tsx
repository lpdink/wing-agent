// src/components/cells/UserMessage.tsx — User message bubble (right-aligned).
//
// No label row — identity conveyed by right alignment + bubble style.
// Tightened padding for density.

import type { CellProps } from '@/core/cell-types'
import type { UserChatItem } from '@/stores/sessionStore'

export function UserMessageCell({ data }: CellProps<UserChatItem>) {
  return (
    <div className="flex justify-end">
      <div className="max-w-[75%] rounded-2xl bg-user-bubble-bg px-3.5 py-2.5 text-user-bubble-text shadow-sm">
        <div className="whitespace-pre-wrap text-[14.5px] leading-relaxed">{data.content}</div>
      </div>
    </div>
  )
}
