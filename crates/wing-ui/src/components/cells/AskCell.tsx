// src/components/cells/AskCell.tsx — Interactive question + choices card.
//
// Displays agent's question with clickable choice buttons.
// After user selects a choice, the card shows the selection and disables interaction.

import { useState } from 'react'
import { HelpCircle, Check } from 'lucide-react'
import type { CellProps } from '@/core/cell-types'
import type { AskChatItem } from '@/stores/sessionStore'

interface AskCellProps extends CellProps<AskChatItem> {
  /** Callback when user selects a choice. Sends the choice back to Gateway. */
  onAnswer?: (choice: string) => void
}

export function AskCell({ data, onAnswer }: AskCellProps) {
  const [selected, setSelected] = useState<string | null>(data.selectedChoice ?? null)
  const isAnswered = data.answered || selected !== null

  const handleSelect = (choice: string) => {
    if (isAnswered) return
    setSelected(choice)
    onAnswer?.(choice)
  }

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] rounded-xl border-2 border-ask-border bg-bg-surface px-4 py-3 shadow-md">
        {/* Header */}
        <div className="mb-2 flex items-center gap-2 text-xs font-medium text-accent">
          <HelpCircle className="h-4 w-4" />
          <span>Wing needs your input</span>
        </div>

        {/* Question */}
        <div className="mb-3 text-sm leading-relaxed text-text">{data.question}</div>

        {/* Choices */}
        {data.choices.length > 0 && (
          <div className="flex flex-wrap gap-2">
            {data.choices.map((choice) => {
              const isSelected = selected === choice
              return (
                <button
                  key={choice}
                  onClick={() => handleSelect(choice)}
                  disabled={isAnswered}
                  className={`flex items-center gap-1.5 rounded-lg border px-3 py-1.5 text-sm transition-all ${
                    isSelected
                      ? 'border-accent bg-accent text-text-inverse'
                      : isAnswered
                        ? 'cursor-not-allowed border-border bg-bg-elevated text-text-muted opacity-60'
                        : 'border-border bg-bg-elevated text-text hover:border-accent hover:bg-accent-muted'
                  }`}
                >
                  {isSelected && <Check className="h-3.5 w-3.5" />}
                  <span>{choice}</span>
                </button>
              )
            })}
          </div>
        )}
      </div>
    </div>
  )
}
