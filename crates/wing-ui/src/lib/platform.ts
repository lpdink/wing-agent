// src/lib/platform.ts — Platform detection and keyboard modifier helpers.

/** True if running on macOS (uses ⌘ instead of Ctrl). */
export function isMac(): boolean {
  // Prefer userAgentData (modern), fall back to userAgent
  const ua = navigator as Navigator & { userAgentData?: { platform: string } }
  if (ua.userAgentData?.platform) {
    return ua.userAgentData.platform.toUpperCase().includes('MAC')
  }
  return navigator.userAgent.includes('Mac')
}

/** The modifier key label for the current platform. */
export const modKey = isMac() ? '⌘' : 'Ctrl'

/** Check if the platform modifier key is pressed in a keyboard event. */
export function isModPressed(e: KeyboardEvent | React.KeyboardEvent): boolean {
  return isMac() ? e.metaKey : e.ctrlKey
}
