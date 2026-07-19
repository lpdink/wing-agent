// src/components/ui/ContextMenu.tsx — Right-click context menu built on @floating-ui/react.
//
// Renders a positioned menu at the cursor location on contextmenu event.

import { useState, type ReactNode } from 'react'
import {
  useFloating,
  offset,
  flip,
  shift,
  useDismiss,
  useRole,
  useInteractions,
} from '@floating-ui/react'

interface ContextMenuProps {
  /** The element that triggers the context menu on right-click. */
  children: ReactNode
  /** Menu content (rendered inside the floating panel). */
  menu: ReactNode
}

export function ContextMenu({ children, menu }: ContextMenuProps) {
  const [open, setOpen] = useState(false)
  const [position, setPosition] = useState({ x: 0, y: 0 })

  const { refs, floatingStyles, context } = useFloating({
    open,
    onOpenChange: setOpen,
    middleware: [offset(4), flip(), shift({ padding: 8 })],
  })

  const dismiss = useDismiss(context)
  const role = useRole(context)
  const { getFloatingProps } = useInteractions([dismiss, role])

  const handleContextMenu = (e: React.MouseEvent) => {
    e.preventDefault()
    setPosition({ x: e.clientX, y: e.clientY })
    setOpen(true)
  }

  return (
    <>
      <div ref={refs.setReference} onContextMenu={handleContextMenu} className="contents">
        {children}
      </div>
      {open && (
        <div
          ref={refs.setFloating}
          style={{ ...floatingStyles, left: position.x, top: position.y, position: 'fixed' }}
          {...getFloatingProps()}
          className="z-50 min-w-[140px] rounded-lg border border-border bg-bg-elevated py-1 shadow-lg"
        >
          {menu}
        </div>
      )}
    </>
  )
}
