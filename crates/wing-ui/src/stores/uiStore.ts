// src/stores/uiStore.ts — UI state (sidebar, errors).

import { create } from 'zustand'

interface UiError {
  id: string
  message: string
  timestamp: number
}

interface UiState {
  sidebarOpen: boolean
  errors: UiError[]
}

interface UiActions {
  toggleSidebar: () => void
  setSidebarOpen: (open: boolean) => void
  addError: (message: string) => void
  dismissError: (id: string) => void
  clearErrors: () => void
}

export type UiStore = UiState & UiActions

export const useUiStore = create<UiStore>((set) => ({
  // State
  sidebarOpen: true,
  errors: [],

  // Actions
  toggleSidebar: () => set((state) => ({ sidebarOpen: !state.sidebarOpen })),

  setSidebarOpen: (open) => set({ sidebarOpen: open }),

  addError: (message) =>
    set((state) => ({
      errors: [...state.errors, { id: crypto.randomUUID(), message, timestamp: Date.now() }],
    })),

  dismissError: (id) => set((state) => ({ errors: state.errors.filter((e) => e.id !== id) })),

  clearErrors: () => set({ errors: [] }),
}))
