// src/components/ui/Popover.tsx — Lightweight popover built on @floating-ui/react.
//
// Provides positioned overlay content triggered by a reference element.
// Handles: click-outside dismiss, Escape key, auto-placement with flip/shift.

import { useState, type ReactNode } from 'react'
import {
  useFloating,
  autoUpdate,
  offset,
  flip,
  shift,
  useClick,
  useDismiss,
  useRole,
  useInteractions,
  type Placement,
} from '@floating-ui/react'

interface PopoverProps {
  /** The trigger element (rendered inline). */
  trigger: ReactNode
  /** Popover content. */
  children: ReactNode
  /** Preferred placement relative to trigger. */
  placement?: Placement
  /** Offset distance in px from the trigger. */
  offsetDistance?: number
  /** Additional class for the floating panel. */
  panelClassName?: string
  /** Controlled open state (optional — uncontrolled by default). */
  open?: boolean
  onOpenChange?: (open: boolean) => void
}

export function Popover({
  trigger,
  children,
  placement = 'bottom-start',
  offsetDistance = 6,
  panelClassName,
  open: controlledOpen,
  onOpenChange: controlledOnOpenChange,
}: PopoverProps) {
  const [uncontrolledOpen, setUncontrolledOpen] = useState(false)

  const isOpen = controlledOpen ?? uncontrolledOpen
  const setIsOpen = controlledOnOpenChange ?? setUncontrolledOpen

  const { refs, floatingStyles, context } = useFloating({
    open: isOpen,
    onOpenChange: setIsOpen,
    placement,
    middleware: [offset(offsetDistance), flip(), shift({ padding: 8 })],
    whileElementsMounted: autoUpdate,
  })

  const click = useClick(context)
  const dismiss = useDismiss(context)
  const role = useRole(context)

  const { getReferenceProps, getFloatingProps } = useInteractions([click, dismiss, role])

  return (
    <>
      <div ref={refs.setReference} {...getReferenceProps()} className="inline-flex">
        {trigger}
      </div>
      {isOpen && (
        <div
          ref={refs.setFloating}
          style={floatingStyles}
          {...getFloatingProps()}
          className={`z-50 rounded-lg border border-border bg-bg-elevated shadow-lg ${panelClassName ?? ''}`}
        >
          {children}
        </div>
      )}
    </>
  )
}
