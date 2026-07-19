// src/components/ui/Toast.tsx — Toast notifications (error + info/success/warning).
//
// Consumes uiStore.errors and uiStore.toasts, displays them as auto-dismissing toasts.

import { useEffect, useRef } from 'react'
import { X, AlertCircle, WifiOff, Info, CheckCircle2, AlertTriangle } from 'lucide-react'
import { useUiStore, type UiToast } from '@/stores/uiStore'

const AUTO_DISMISS_MS = 5000

export function ToastContainer() {
  const errors = useUiStore((s) => s.errors)
  const toasts = useUiStore((s) => s.toasts)
  const dismissError = useUiStore((s) => s.dismissError)
  const dismissToast = useUiStore((s) => s.dismissToast)

  const allToasts: Array<UiToast & { isLegacyError: boolean }> = [
    ...errors.map((e) => ({ ...e, isLegacyError: true })),
    ...toasts.map((t) => ({ ...t, isLegacyError: false })),
  ]

  // Sort by timestamp (newest last)
  allToasts.sort((a, b) => a.timestamp - b.timestamp)

  return (
    <div className="pointer-events-none fixed right-4 top-4 z-50 flex flex-col gap-2">
      {allToasts.map((toast) => (
        <ToastItem
          key={toast.id}
          toast={toast}
          onDismiss={() => (toast.isLegacyError ? dismissError(toast.id) : dismissToast(toast.id))}
        />
      ))}
    </div>
  )
}

function toastStyle(level: UiToast['level']): { border: string; iconColor: string } {
  switch (level) {
    case 'error':
      return { border: 'border-error/30', iconColor: 'text-error' }
    case 'warning':
      return { border: 'border-warning/30', iconColor: 'text-warning' }
    case 'success':
      return { border: 'border-success/30', iconColor: 'text-success' }
    case 'info':
      return { border: 'border-accent/30', iconColor: 'text-accent' }
  }
}

function toastIcon(level: UiToast['level'], message: string) {
  if (level === 'error') {
    const isConnectionError =
      message.toLowerCase().includes('connect') ||
      message.toLowerCase().includes('fetch') ||
      message.toLowerCase().includes('network')
    return isConnectionError ? WifiOff : AlertCircle
  }
  if (level === 'success') return CheckCircle2
  if (level === 'warning') return AlertTriangle
  return Info
}

function ToastItem({ toast, onDismiss }: { toast: UiToast; onDismiss: () => void }) {
  // Hold onDismiss in a ref so the timer is not reset on re-render
  const dismissRef = useRef(onDismiss)
  dismissRef.current = onDismiss

  useEffect(() => {
    const timer = setTimeout(() => dismissRef.current(), AUTO_DISMISS_MS)
    return () => clearTimeout(timer)
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  const style = toastStyle(toast.level)
  const Icon = toastIcon(toast.level, toast.message)

  return (
    <div
      className={`pointer-events-auto flex max-w-sm items-start gap-2 rounded-lg border ${style.border} bg-bg-elevated px-3 py-2.5 shadow-lg animate-in`}
    >
      <Icon className={`mt-0.5 h-4 w-4 shrink-0 ${style.iconColor}`} />
      <span className="flex-1 text-sm text-text">{toast.message}</span>
      <button
        onClick={onDismiss}
        className="shrink-0 rounded p-0.5 text-text-muted transition-colors hover:text-text"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </div>
  )
}
