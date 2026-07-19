// src/stores/uiStore.ts — UI state (sidebar, errors, toasts, panels).

import { create } from 'zustand'

export interface UiToast {
  id: string
  message: string
  level: 'info' | 'success' | 'warning' | 'error'
  timestamp: number
}

interface UiState {
  sidebarOpen: boolean
  commandPaletteOpen: boolean
  settingsOpen: boolean
  errors: UiToast[]
  toasts: UiToast[]
}

interface UiActions {
  toggleSidebar: () => void
  setSidebarOpen: (open: boolean) => void
  setCommandPaletteOpen: (open: boolean) => void
  toggleCommandPalette: () => void
  setSettingsOpen: (open: boolean) => void
  toggleSettings: () => void
  addError: (message: string) => void
  dismissError: (id: string) => void
  clearErrors: () => void
  addToast: (message: string, level?: UiToast['level']) => void
  dismissToast: (id: string) => void
}

export type UiStore = UiState & UiActions

/** Dedup window: suppress identical message+level within this many ms. */
const TOAST_DEDUP_MS = 3000

export const useUiStore = create<UiStore>((set, get) => ({
  // State
  sidebarOpen: true,
  commandPaletteOpen: false,
  settingsOpen: false,
  errors: [],
  toasts: [],

  // Actions
  toggleSidebar: () => set((state) => ({ sidebarOpen: !state.sidebarOpen })),

  setSidebarOpen: (open) => set({ sidebarOpen: open }),

  setCommandPaletteOpen: (open) => set({ commandPaletteOpen: open }),

  toggleCommandPalette: () => set((state) => ({ commandPaletteOpen: !state.commandPaletteOpen })),

  setSettingsOpen: (open) => set({ settingsOpen: open }),

  toggleSettings: () => set((state) => ({ settingsOpen: !state.settingsOpen })),

  addError: (message) =>
    set((state) => ({
      errors: [
        ...state.errors,
        { id: crypto.randomUUID(), message, level: 'error', timestamp: Date.now() },
      ],
    })),

  dismissError: (id) => set((state) => ({ errors: state.errors.filter((e) => e.id !== id) })),

  clearErrors: () => set({ errors: [] }),

  addToast: (message, level = 'info') => {
    const now = Date.now()
    const { toasts } = get()
    // Dedup: skip if same message+level exists within the dedup window
    const isDuplicate = toasts.some(
      (t) => t.message === message && t.level === level && now - t.timestamp < TOAST_DEDUP_MS,
    )
    if (isDuplicate) return
    set((state) => ({
      toasts: [...state.toasts, { id: crypto.randomUUID(), message, level, timestamp: now }],
    }))
  },

  dismissToast: (id) => set((state) => ({ toasts: state.toasts.filter((t) => t.id !== id) })),
}))
