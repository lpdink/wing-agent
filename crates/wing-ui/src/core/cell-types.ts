// src/core/cell-types.ts — Generic cell component interface.
//
// All Cell components implement this interface. ChatArea uses
// `CellComponent<ChatItem>` to render any message type.

import type { ComponentType } from 'react'
import type { ChatItem } from '@/stores/sessionStore'

/** Props passed to every Cell component. */
export interface CellProps<T extends ChatItem = ChatItem> {
  data: T
}

/** A Cell component that renders a specific ChatItem type. */
export type CellComponent<T extends ChatItem = ChatItem> = ComponentType<CellProps<T>>

/** Callback for AskCell to send user's choice back to Gateway. */
export type OnAskAnswer = (choice: string) => void
