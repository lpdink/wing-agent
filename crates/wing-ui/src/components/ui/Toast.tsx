// src/components/ui/Toast.tsx — Error toast notifications.
//
// Consumes uiStore.errors and displays them as auto-dismissing toasts.

import { useEffect } from 'react'
import { X, AlertCircle, WifiOff } from 'lucide-react'
import { useUiStore } from '@/stores/uiStore'

const AUTO_DISMISS_MS = 5000

export function ToastContainer() {
  const errors = useUiStore((s) => s.errors)
  const dismissError = useUiStore((s) => s.dismissError)

  return (
    <div className="pointer-events-none fixed right-4 top-4 z-50 flex flex-col gap-2">
      {errors.map((err) => (
        <Toast key={err.id} message={err.message} onDismiss={() => dismissError(err.id)} />
      ))}
    </div>
  )
}

function Toast({ message, onDismiss }: { message: string; onDismiss: () => void }) {
  // Auto-dismiss after timeout
  useEffect(() => {
    const timer = setTimeout(onDismiss, AUTO_DISMISS_MS)
    return () => clearTimeout(timer)
  }, [onDismiss])

  const isConnectionError =
    message.toLowerCase().includes('connect') ||
    message.toLowerCase().includes('fetch') ||
    message.toLowerCase().includes('network')

  const Icon = isConnectionError ? WifiOff : AlertCircle

  return (
    <div className="pointer-events-auto flex max-w-sm items-start gap-2 rounded-lg border border-error/30 bg-bg-elevated px-3 py-2.5 shadow-lg animate-in">
      <Icon className="mt-0.5 h-4 w-4 shrink-0 text-error" />
      <span className="flex-1 text-sm text-text">{message}</span>
      <button
        onClick={onDismiss}
        className="shrink-0 rounded p-0.5 text-text-muted transition-colors hover:text-text"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </div>
  )
}
