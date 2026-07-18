// src/hooks/useAutoScroll.ts — Smart auto-scrolling for chat area.
//
// Behavior:
// - New messages: auto-scroll to bottom if user is near bottom (within threshold)
// - User scrolls up: don't interrupt (don't force back to bottom)
// - Streaming text updates: keep following if already at bottom

import { useRef, useCallback, useEffect, type RefObject } from 'react'

const AUTO_SCROLL_THRESHOLD = 80 // px from bottom to count as "near bottom"

interface UseAutoScrollOptions {
  /** Dependency that triggers auto-scroll check (e.g., messages array length) */
  deps: unknown[]
}

/**
 * Smart auto-scroll hook for chat containers.
 *
 * Returns a ref to attach to the scrollable container.
 */
export function useAutoScroll<T extends HTMLElement>({
  deps,
}: UseAutoScrollOptions): RefObject<T | null> {
  const scrollRef = useRef<T>(null)
  const isNearBottomRef = useRef(true)

  // Track whether user is near bottom on scroll events
  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    isNearBottomRef.current = distanceFromBottom < AUTO_SCROLL_THRESHOLD
  }, [])

  // Attach scroll listener
  useEffect(() => {
    const el = scrollRef.current
    if (!el) return
    el.addEventListener('scroll', handleScroll, { passive: true })
    return () => el.removeEventListener('scroll', handleScroll)
  }, [handleScroll])

  // Auto-scroll when deps change (new messages or streaming updates)
  useEffect(() => {
    if (!isNearBottomRef.current) return
    const el = scrollRef.current
    if (!el) return
    el.scrollTop = el.scrollHeight
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  return scrollRef
}
