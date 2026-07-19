// src/hooks/useAutoScroll.ts — Smart auto-scrolling for chat area.
//
// Behavior (aligned with Rust TUI chat_view.rs auto_scroll model):
// - User at/near bottom → new content auto-follows to bottom
// - User scrolls up → stop following, let them read in peace
// - User scrolls back to bottom → re-enable following
// - Programmatic pin-to-bottom does NOT count as user scroll
//
// Uses a callback ref so the scroll listener reliably binds/unbinds
// when the actual scrollable DOM element mounts/unmounts (conditional rendering).

import { useRef, useState, useCallback, useEffect, type RefObject } from 'react'

const AUTO_SCROLL_THRESHOLD = 80 // px from bottom to count as "near bottom"

interface UseAutoScrollOptions {
  /** Dependency that triggers auto-scroll check (e.g., messages array) */
  deps: unknown[]
}

interface UseAutoScrollReturn<T extends HTMLElement> {
  /** Callback ref — attach to the scrollable container. */
  scrollRef: (node: T | null) => void
  /** RefObject for imperative access (e.g., reading scrollHeight). */
  scrollElementRef: RefObject<T | null>
  /** Whether auto-follow is active. */
  isFollowing: boolean
  /** Smoothly scroll to bottom and re-enable following. */
  scrollToBottom: () => void
}

/**
 * Smart auto-scroll hook for chat containers.
 *
 * Returns a callback ref to attach to the scrollable container,
 * plus `isFollowing` state and a `scrollToBottom` action.
 */
export function useAutoScroll<T extends HTMLElement>({
  deps,
}: UseAutoScrollOptions): UseAutoScrollReturn<T> {
  const scrollElementRef = useRef<T | null>(null)
  const isFollowingRef = useRef(true)
  const isProgrammaticScrollRef = useRef(false)
  const [isFollowing, setIsFollowing] = useState(true)

  // ── Scroll event handler ──────────────────────────────────────
  const handleScroll = useCallback(() => {
    // Ignore programmatic scrolls (our own pin-to-bottom)
    if (isProgrammaticScrollRef.current) return

    const el = scrollElementRef.current
    if (!el) return

    const distanceFromBottom = el.scrollHeight - el.scrollTop - el.clientHeight
    const nearBottom = distanceFromBottom < AUTO_SCROLL_THRESHOLD

    if (nearBottom !== isFollowingRef.current) {
      isFollowingRef.current = nearBottom
      setIsFollowing(nearBottom)
    }
  }, [])

  // ── Callback ref: bind/unbind scroll listener on mount/unmount ──
  const scrollRef = useCallback(
    (node: T | null) => {
      // Unbind from previous element
      const prev = scrollElementRef.current
      if (prev) {
        prev.removeEventListener('scroll', handleScroll)
      }

      scrollElementRef.current = node

      // Bind to new element
      if (node) {
        node.addEventListener('scroll', handleScroll, { passive: true })
        // Pin to bottom on initial mount
        isProgrammaticScrollRef.current = true
        node.scrollTop = node.scrollHeight
        requestAnimationFrame(() => {
          isProgrammaticScrollRef.current = false
        })
      }
    },
    [handleScroll],
  )

  // ── Auto-scroll when deps change (new messages or streaming updates) ──
  useEffect(() => {
    if (!isFollowingRef.current) return
    const el = scrollElementRef.current
    if (!el) return

    isProgrammaticScrollRef.current = true
    el.scrollTop = el.scrollHeight
    requestAnimationFrame(() => {
      isProgrammaticScrollRef.current = false
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps)

  // ── Scroll to bottom (floating button action) ─────────────────
  const scrollToBottom = useCallback(() => {
    const el = scrollElementRef.current
    if (!el) return

    isProgrammaticScrollRef.current = true
    el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' })
    isFollowingRef.current = true
    setIsFollowing(true)
    // Reset programmatic flag after smooth scroll completes
    const onScrollEnd = () => {
      isProgrammaticScrollRef.current = false
      el.removeEventListener('scrollend', onScrollEnd)
    }
    // scrollend is well-supported in modern browsers; fallback with timeout
    if ('onscrollend' in el) {
      el.addEventListener('scrollend', onScrollEnd, { once: true })
    } else {
      setTimeout(() => {
        isProgrammaticScrollRef.current = false
      }, 500)
    }
  }, [])

  return { scrollRef, scrollElementRef, isFollowing, scrollToBottom }
}
