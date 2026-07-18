// src/stores/uiStore.ts — UI state (sidebar, errors, toasts).

import { create } from 'zustand'

export interface UiToast {
  id: string
  message: string
  level: 'info' | 'success' | 'warning' | 'error'
  timestamp: number
}

interface UiState {
  sidebarOpen: boolean
  errors: UiToast[]
  toasts: UiToast[]
}

interface UiActions {
  toggleSidebar: () => void
  setSidebarOpen: (open: boolean) => void
  addError: (message: string) => void
  dismissError: (id: string) => void
  clearErrors: () => void
  addToast: (message: string, level?: UiToast['level']) => void
  dismissToast: (id: string) => void
}

export type UiStore = UiState & UiActions

export const useUiStore = create<UiStore>((set) => ({
  // State
  sidebarOpen: true,
  errors: [],
  toasts: [],

  // Actions
  toggleSidebar: () => set((state) => ({ sidebarOpen: !state.sidebarOpen })),

  setSidebarOpen: (open) => set({ sidebarOpen: open }),

  addError: (message) =>
    set((state) => ({
      errors: [
        ...state.errors,
        { id: crypto.randomUUID(), message, level: 'error', timestamp: Date.now() },
      ],
    })),

  dismissError: (id) => set((state) => ({ errors: state.errors.filter((e) => e.id !== id) })),

  clearErrors: () => set({ errors: [] }),

  addToast: (message, level = 'info') =>
    set((state) => ({
      toasts: [...state.toasts, { id: crypto.randomUUID(), message, level, timestamp: Date.now() }],
    })),

  dismissToast: (id) => set((state) => ({ toasts: state.toasts.filter((t) => t.id !== id) })),
}))
