/**
 * PWA install prompt — a small banner shown once on Android Chrome.
 *
 * On iOS Safari the user must manually "Add to Home Screen" — no programmatic
 * prompt exists — so this component only activates when `beforeinstallprompt`
 * fires (Android Chrome / some Chromium-based browsers).
 *
 * Once dismissed it never shows again (tracked by localStorage).
 */

import { useCallback, useEffect, useState, type ReactElement } from 'react';

const DISMISSED_KEY = 'wing.pwa-install.dismissed';

export function InstallPrompt(): ReactElement | null {
  const [visible, setVisible] = useState(false);
  const [event, setEvent] = useState<Event | null>(null);

  useEffect(() => {
    // Check if the user already dismissed the prompt.
    try {
      if (localStorage.getItem(DISMISSED_KEY) === '1') {
        return;
      }
    } catch {
      // Storage unavailable — fall through to show it.
    }

    const handler = (e: Event): void => {
      e.preventDefault();
      setEvent(e);
      setVisible(true);
    };

    globalThis.addEventListener('beforeinstallprompt', handler);
    return () => {
      globalThis.removeEventListener('beforeinstallprompt', handler);
    };
  }, []);

  const handleInstall = useCallback(() => {
    if (
      event !== null &&
      'prompt' in event &&
      typeof (event as { prompt: () => void }).prompt === 'function'
    ) {
      void (event as { prompt: () => Promise<void> }).prompt().then(() => {
        setVisible(false);
      });
    }
  }, [event]);

  const handleDismiss = useCallback(() => {
    setVisible(false);
    try {
      localStorage.setItem(DISMISSED_KEY, '1');
    } catch {
      // Ignore.
    }
  }, []);

  if (!visible) {
    return null;
  }

  return (
    <div className="install-prompt" role="alert">
      <span className="install-prompt__text">Install Wing for the best experience</span>
      <div className="install-prompt__actions">
        <button
          type="button"
          className="install-prompt__install button button--primary"
          onClick={handleInstall}
        >
          Install
        </button>
        <button
          type="button"
          className="install-prompt__dismiss button button--ghost"
          onClick={handleDismiss}
        >
          Not now
        </button>
      </div>
    </div>
  );
}
