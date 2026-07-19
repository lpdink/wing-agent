// src/components/ui/SettingsPanel.tsx — Settings modal (theme + Gateway URL).
//
// Centered modal accessible via ⌘, or Sidebar settings button.
// Unifies theme toggle and Gateway connection configuration.

import { useState, useEffect } from 'react'
import { X, Sun, Moon, Server, Check } from 'lucide-react'
import { useUiStore } from '@/stores/uiStore'
import { useConnectionStore } from '@/stores/connectionStore'
import { setStoredGatewayUrl } from '@/lib/gateway-config'
import { THEME_STORAGE_KEY } from '@/lib/constants'

export function SettingsPanel() {
  const open = useUiStore((s) => s.settingsOpen)
  const setOpen = useUiStore((s) => s.setSettingsOpen)
  const addToast = useUiStore((s) => s.addToast)
  const gatewayUrl = useConnectionStore((s) => s.gatewayUrl)
  const setGatewayUrl = useConnectionStore((s) => s.setGatewayUrl)

  const [isDark, setIsDark] = useState(false)
  const [urlInput, setUrlInput] = useState('')
  const [urlSaved, setUrlSaved] = useState(false)

  // Sync local state when opened
  useEffect(() => {
    if (open) {
      setIsDark(document.documentElement.dataset.theme === 'dark')
      setUrlInput(gatewayUrl)
      setUrlSaved(false)
    }
  }, [open, gatewayUrl])

  const toggleTheme = () => {
    const next = !isDark
    setIsDark(next)
    document.documentElement.dataset.theme = next ? 'dark' : 'light'
    localStorage.setItem(THEME_STORAGE_KEY, next ? 'dark' : 'light')
  }

  const [urlError, setUrlError] = useState('')

  const handleSaveUrl = () => {
    const trimmed = urlInput.trim().replace(/\/+$/, '')
    if (!trimmed) return

    // Validate URL format and protocol
    try {
      const parsed = new URL(trimmed)
      if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
        setUrlError('Only http:// and https:// are supported')
        return
      }
    } catch {
      setUrlError('Invalid URL format')
      return
    }

    setUrlError('')
    setStoredGatewayUrl(trimmed)
    setGatewayUrl(trimmed)
    setUrlSaved(true)
    addToast('Gateway URL updated — reconnecting…', 'info')

    // Reset saved indicator after a moment
    setTimeout(() => setUrlSaved(false), 2000)
  }

  const handleUrlKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault()
      handleSaveUrl()
    }
  }

  if (!open) return null

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-black/40" onClick={() => setOpen(false)} />

      {/* Panel */}
      <div className="relative w-full max-w-md rounded-xl border border-border bg-bg-elevated shadow-2xl">
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-5 py-4">
          <h2 className="text-sm font-semibold text-text">Settings</h2>
          <button
            onClick={() => setOpen(false)}
            className="rounded-md p-1 text-text-muted transition-colors hover:bg-bg-hover hover:text-text"
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        {/* Body */}
        <div className="space-y-6 px-5 py-5">
          {/* Theme section */}
          <section>
            <h3 className="mb-3 text-xs font-medium uppercase tracking-wider text-text-muted">
              Appearance
            </h3>
            <div className="flex items-center justify-between rounded-lg border border-border bg-bg-surface px-4 py-3">
              <div className="flex items-center gap-3">
                {isDark ? (
                  <Moon className="h-4 w-4 text-accent" />
                ) : (
                  <Sun className="h-4 w-4 text-warning" />
                )}
                <span className="text-sm text-text">{isDark ? 'Dark' : 'Light'} theme</span>
              </div>
              <button
                onClick={toggleTheme}
                className={`relative h-6 w-11 rounded-full transition-colors ${
                  isDark ? 'bg-accent' : 'bg-border'
                }`}
              >
                <span
                  className={`absolute top-0.5 h-5 w-5 rounded-full bg-white shadow transition-transform ${
                    isDark ? 'translate-x-[22px]' : 'translate-x-0.5'
                  }`}
                />
              </button>
            </div>
          </section>

          {/* Gateway section */}
          <section>
            <h3 className="mb-3 text-xs font-medium uppercase tracking-wider text-text-muted">
              Gateway Connection
            </h3>
            <div className="rounded-lg border border-border bg-bg-surface px-4 py-3">
              <div className="mb-2 flex items-center gap-2 text-sm text-text-dim">
                <Server className="h-4 w-4 text-text-muted" />
                <span>Gateway URL</span>
              </div>
              <div className="flex items-center gap-2">
                <input
                  value={urlInput}
                  onChange={(e) => {
                    setUrlInput(e.target.value)
                    setUrlError('')
                  }}
                  onKeyDown={handleUrlKeyDown}
                  placeholder="http://127.0.0.1:32523"
                  className={`flex-1 rounded-lg border bg-bg-input px-3 py-2 text-sm text-text placeholder-text-muted outline-none transition-colors focus:border-border-focus ${
                    urlError ? 'border-error' : 'border-border'
                  }`}
                />
                <button
                  onClick={handleSaveUrl}
                  className={`flex items-center gap-1.5 rounded-lg px-3 py-2 text-sm font-medium transition-colors ${
                    urlSaved
                      ? 'bg-success/10 text-success'
                      : 'bg-accent text-text-inverse hover:bg-accent-hover'
                  }`}
                >
                  {urlSaved ? <Check className="h-3.5 w-3.5" /> : null}
                  {urlSaved ? 'Saved' : 'Save'}
                </button>
              </div>
              {urlError && <p className="mt-1.5 text-[11px] text-error">{urlError}</p>}
              <p className="mt-2 text-[11px] text-text-muted">
                Changes take effect immediately. The app will reconnect to the new address.
              </p>
            </div>
          </section>
        </div>
      </div>
    </div>
  )
}
